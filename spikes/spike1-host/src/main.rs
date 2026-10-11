//! spike1-host: spawns `pi` in a PTY (ConPTY on Windows), bridges PTY I/O to a
//! WebSocket consumed by an xterm.js page, and prints structured events live
//! by tailing the session JSONL pi writes (`--session-dir` points into this
//! spike; that file is what `PI_SESSION_FILE` names for pi's tool children).
//!
//! Wire protocol on the WS port:
//!   client -> server: binary frames = terminal input bytes;
//!                     text frames = JSON control: {"t":"hello"|"resize","cols","rows"}
//!   server -> client: binary frames = PTY output bytes;
//!                     text frames = JSON: {"t":"out"|... } reserved, {"t":"ev","event":{...}}
//!                     for session-tail events, {"t":"info",...} for host notices.
//!
//! Exploratory spike code: minimal error handling, no auth on the sockets.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

use spike1_host::events::{entry_to_events, parse_line};
use spike1_host::tail::SessionDirWatcher;

/// Shared state between PTY threads and WS sessions.
struct Hub {
    pty_out: broadcast::Sender<Vec<u8>>,
    events: broadcast::Sender<String>,
    info: broadcast::Sender<String>,
    pty_writer: Mutex<Box<dyn std::io::Write + Send>>,
    pty_master: Mutex<Box<dyn MasterPty + Send>>,
}

impl Hub {
    fn resize(&self, cols: u16, rows: u16) {
        let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
        if let Err(e) = self.pty_master.lock().unwrap().resize(size) {
            eprintln!("[host] pty resize failed: {e}");
        } else {
            eprintln!("[host] pty resized to {cols}x{rows}");
        }
    }

    fn write_input(&self, bytes: &[u8]) {
        let mut w = self.pty_writer.lock().unwrap();
        if let Err(e) = w.write_all(bytes).and_then(|_| w.flush()) {
            eprintln!("[host] pty write failed: {e}");
        }
    }

    fn announce(&self, v: serde_json::Value) {
        let s = v.to_string();
        eprintln!("[host] {s}");
        let _ = self.info.send(s);
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: spike1-host [--http-port 8791] [--ws-port 8792] [--prog \"<program and args>\"] [-- <pi args...>]\n\
         \n\
         Defaults: prog = resolved pi (node + cli.js from the npm shim), pi args = -a --session-dir <spike>/.pi-run/sessions"
    );
    std::process::exit(2);
}

fn main() -> Result<()> {
    let mut http_port: u16 = 8791;
    let mut ws_port: u16 = 8792;
    let mut prog_override: Option<String> = None;
    let mut pi_args: Vec<String> = vec![];

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--http-port" => http_port = it.next().context("--http-port needs a value")?.parse()?,
            "--ws-port" => ws_port = it.next().context("--ws-port needs a value")?.parse()?,
            "--prog" => prog_override = Some(it.next().context("--prog needs a value")?),
            "--" => {
                pi_args = it.collect();
                break;
            }
            "-h" | "--help" => usage(),
            other => bail!("unknown argument {other}"),
        }
    }

    let spike_dir = std::env::current_dir().context("need cwd (run from spikes/spike1-host)")?;
    let run_dir = spike_dir.join(".pi-run");
    let session_dir = run_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).context("create .pi-run/sessions")?;

    let (program, prefix_args) = resolve_pi_command(prog_override);
    if pi_args.is_empty() {
        pi_args = vec![
            "-a".to_string(),
            "--session-dir".to_string(),
            session_dir.to_string_lossy().into_owned(),
        ];
    }

    eprintln!("[host] program: {} {}", program, prefix_args.join(" "));
    eprintln!("[host] pi args: {}", pi_args.join(" "));
    eprintln!("[host] spike dir: {}", spike_dir.display());

    ensure_mcp_config(&spike_dir)?;

    // ---- spawn pi in a PTY (ConPTY on Windows) ----
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .context("openpty (ConPTY)")?;
    let mut cmd = CommandBuilder::new(&program);
    cmd.args(prefix_args.iter().map(|s| s.as_str()).chain(pi_args.iter().map(|s| s.as_str())));
    cmd.cwd(&spike_dir);
    cmd.env("TERM", "xterm-256color");
    cmd.env("PI_SKIP_VERSION_CHECK", "1");
    // Scrub pi-injected session markers inherited from *our* parent (this host
    // may itself run inside a pi bash tool). pi re-sets PI_SESSION_* for its own
    // tool children, but subagent markers would otherwise leak in uncleaned.
    for var in [
        "AI_AGENT",
        "PI_CODING_AGENT",
        "PI_SESSION_ID",
        "PI_SESSION_FILE",
        "PI_PROVIDER",
        "PI_MODEL",
        "PI_REASONING_LEVEL",
        "PI_SUBAGENT_CHILD",
        "PI_SUBAGENT_PARENT_SESSION",
        "PI_MEMORY_PROMOTION_GATE",
        "PI_ASYNC_NATIVE_RUNNER",
    ] {
        cmd.env_remove(var);
    }
    let mut child = pair.slave.spawn_command(cmd).context("spawn pi in pty")?;
    drop(pair.slave); // close slave so EOF propagates when pi exits

    let reader = pair.master.try_clone_reader().context("pty reader")?;
    let writer = pair.master.take_writer().context("pty writer")?;

    let (out_tx, _) = broadcast::channel(4096);
    let (ev_tx, _) = broadcast::channel(1024);
    let (info_tx, _) = broadcast::channel(256);
    let hub = Arc::new(Hub {
        pty_out: out_tx,
        events: ev_tx,
        info: info_tx,
        pty_writer: Mutex::new(writer),
        pty_master: Mutex::new(pair.master),
    });

    // PTY output pump (std thread: portable-pty readers are blocking)
    {
        let hub = hub.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        hub.announce(json!({"t": "info", "msg": "pty output stream closed"}));
                        break;
                    }
                    Ok(n) => {
                        let _ = hub.pty_out.send(buf[..n].to_vec());
                    }
                    Err(e) => {
                        hub.announce(json!({"t": "info", "msg": format!("pty read error: {e}")}));
                        break;
                    }
                }
            }
        });
    }

    // Child reaper
    {
        std::thread::spawn(move || {
            let status = child.wait();
            eprintln!("[host] pi exited: {:?}", status);
        });
    }

    // Session JSONL tailer: prints structured events live and broadcasts them.
    {
        let hub = hub.clone();
        let dir = session_dir.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("tailer runtime");
            rt.block_on(async move {
                let mut watcher = SessionDirWatcher::baseline_existing(dir).expect("watch session dir");
                let mut first_seen: Option<std::path::PathBuf> = None;
                loop {
                    match watcher.poll() {
                        Ok(lines) => {
                            for (path, line) in lines {
                                if first_seen.is_none() {
                                    first_seen = Some(path.clone());
                                    hub.announce(
                                        json!({"t": "info", "msg": "session file", "path": path.to_string_lossy()}),
                                    );
                                }
                                let Some(entry) = parse_line(&line) else { continue };
                                for ev in entry_to_events(&entry) {
                                    let wrapped = json!({
                                        "ts": iso_now(),
                                        "src": "session-tail",
                                        "file": path.file_name().map(|s| s.to_string_lossy()),
                                        "event": ev,
                                    });
                                    let s = wrapped.to_string();
                                    println!("{s}");
                                    let _ = hub.events.send(s);
                                }
                            }
                        }
                        Err(e) => eprintln!("[host] tail error: {e}"),
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
        });
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let http = tokio::spawn(serve_http(spike_dir.join("web"), http_port));
        let ws = tokio::spawn(serve_ws(hub, ws_port));
        let (r1, r2) = tokio::join!(http, ws);
        r1.context("http server")?.context("http task")?;
        r2.context("ws server")?.context("ws task")?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Resolve the command used to launch pi.
///
/// Priority: `--prog "..."` / `SPIKE1_PI_CMD` env (split on whitespace),
/// then PATH lookup: `pi.exe` directly; an npm `pi.cmd` shim is translated to
/// `node <prefix>/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js`
/// (CreateProcess does not run .cmd shims by itself and wrapping in cmd.exe
/// mangles signals).
fn resolve_pi_command(override_prog: Option<String>) -> (String, Vec<String>) {
    if let Some(p) = override_prog.or_else(|| std::env::var("SPIKE1_PI_CMD").ok()) {
        let mut parts = p.split_whitespace().map(String::from);
        let prog = parts.next().unwrap_or_else(|| "pi".into());
        return (prog, parts.collect());
    }
    if let Ok(found) = which_like("pi") {
        if found.extension().and_then(|s| s.to_str()) == Some("cmd") {
            if let Some(prefix) = found.parent() {
                let cli = prefix
                    .join("node_modules")
                    .join("@earendil-works")
                    .join("pi-coding-agent")
                    .join("dist")
                    .join("bundle")
                    .join("cli.js");
                if cli.exists() {
                    return ("node".into(), vec![cli.to_string_lossy().into_owned()]);
                }
            }
        } else {
            return (found.to_string_lossy().into_owned(), vec![]);
        }
    }
    ("pi".into(), vec![]) // let the OS try
}

/// Tiny which(1) for Windows PATH resolution with common extensions.
fn which_like(name: &str) -> std::io::Result<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &["exe", "cmd", "bat", ""] } else { &[""] };
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for ext in exts {
            let cand = if ext.is_empty() { dir.join(name) } else { dir.join(format!("{name}.{ext}")) };
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{name} not on PATH")))
}

/// Keep `.pi/mcp.json` pointing at this build's `spike1-mcp` binary.
///
/// The committed file uses relative paths as documentation; the host rewrites
/// it with absolute paths so pi's spawn works regardless of how node resolves
/// relative commands.
fn ensure_mcp_config(spike_dir: &std::path::Path) -> Result<()> {
    let exe = std::env::current_exe().context("current exe")?;
    let mcp_bin = exe.parent().context("exe dir")?.join(if cfg!(windows) { "spike1-mcp.exe" } else { "spike1-mcp" });
    if !mcp_bin.exists() {
        eprintln!("[host] NOTE: {} not found yet; .pi/mcp.json left as-is (build spike1-mcp first)", mcp_bin.display());
        return Ok(());
    }
    let cfg = json!({
        "mcpServers": {
            "spike1-tools": {
                "command": mcp_bin.to_string_lossy(),
                "args": [],
                "cwd": spike_dir.to_string_lossy(),
                "exposure": "direct",
                "description": "Spike1 test tools: ping and spool listing"
            }
        }
    });
    let dir = spike_dir.join(".pi");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("mcp.json"), serde_json::to_string_pretty(&cfg)? + "\n")?;
    eprintln!("[host] .pi/mcp.json -> {}", mcp_bin.display());
    Ok(())
}

async fn serve_http(web_dir: PathBuf, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("[host] http://127.0.0.1:{port}/ (static from {})", web_dir.display());
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_http(stream, web_dir.clone()));
    }
}

async fn handle_http(mut stream: TcpStream, web_dir: PathBuf) {
    let mut buf = vec![0u8; 16384];
    let mut used = 0usize;
    // read until end of headers
    loop {
        if used == buf.len() {
            return;
        }
        match stream.read(&mut buf[used..]).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                used += n;
                if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let head = String::from_utf8_lossy(&buf[..used]);
    let mut parts = head.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "GET" {
        let _ = write_simple(&mut stream, 405, "text/plain", b"method not allowed").await;
        return;
    }
    let path = path.split('?').next().unwrap_or("/");
    if path == "/healthz" {
        let _ = write_simple(&mut stream, 200, "text/plain", b"ok").await;
        return;
    }
    let rel = path.trim_start_matches('/');
    if rel.contains("..") || rel.contains('\\') {
        let _ = write_simple(&mut stream, 400, "text/plain", b"bad path").await;
        return;
    }
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let file = web_dir.join(rel);
    match tokio::fs::read(&file).await {
        Ok(body) => {
            let ct = content_type(&file);
            let _ = write_simple(&mut stream, 200, ct, &body).await;
            eprintln!("[host] GET {path} -> 200 ({} bytes)", body.len());
        }
        Err(_) => {
            let _ = write_simple(&mut stream, 404, "text/plain", b"not found").await;
            eprintln!("[host] GET {path} -> 404");
        }
    }
}

fn content_type(p: &std::path::Path) -> &'static str {
    match p.extension().and_then(|s| s.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

async fn write_simple(stream: &mut TcpStream, code: u16, ct: &str, body: &[u8]) -> Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!("HTTP/1.1 {code} {reason}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

async fn serve_ws(hub: Arc<Hub>, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("[host] ws://127.0.0.1:{port}/ (pty bridge)");
    loop {
        let (stream, addr) = listener.accept().await?;
        tokio::spawn(handle_ws(stream, addr, hub.clone()));
    }
}

async fn handle_ws(stream: TcpStream, addr: SocketAddr, hub: Arc<Hub>) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[host] ws handshake failed from {addr}: {e}");
            return;
        }
    };
    eprintln!("[host] ws client {addr} connected");
    let (mut sink, mut source) = ws.split();

    let mut out_rx = hub.pty_out.subscribe();
    let mut ev_rx = hub.events.subscribe();
    let mut info_rx = hub.info.subscribe();

    loop {
        tokio::select! {
            out = out_rx.recv() => {
                match out {
                    Ok(bytes) => {
                        if sink.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            ev = ev_rx.recv() => {
                match ev {
                    Ok(s) => {
                        if sink.send(Message::Text(s.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            info = info_rx.recv() => {
                match info {
                    Ok(s) => {
                        if sink.send(Message::Text(s.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = source.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => hub.write_input(&bytes),
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(txt.as_str()) {
                            let t = v.get("t").and_then(|t| t.as_str()).unwrap_or("");
                            let cols = v.get("cols").and_then(|c| c.as_u64()).unwrap_or(80) as u16;
                            let rows = v.get("rows").and_then(|r| r.as_u64()).unwrap_or(24) as u16;
                            match t {
                                "hello" | "resize" => hub.resize(cols, rows),
                                _ => eprintln!("[host] unknown control message: {txt}"),
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        eprintln!("[host] ws error from {addr}: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    eprintln!("[host] ws client {addr} disconnected");
}

/// ISO-8601 UTC timestamp without external crates.
fn iso_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let millis = d.subsec_millis();
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

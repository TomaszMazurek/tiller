// Isolation test: spawn a process in a ConPTY and read raw output.
// Answers the ConPTY startup DSR cursor-position request (ESC[6n) with
// ESC[1;1R, the way a real terminal would.
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let program = args.first().cloned().unwrap_or_else(|| "cmd.exe".into());
    let rest: Vec<&str> = args.iter().skip(1).map(|s| s.as_str()).collect();

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system.openpty(portable_pty::PtySize {
        rows: 24, cols: 80, pixel_width: 0, pixel_height: 0,
    })?;
    let mut cmd = portable_pty::CommandBuilder::new(&program);
    if !rest.is_empty() { cmd.args(rest); }
    let mut child = pair.slave.spawn_command(cmd)?;
    let _keep_slave = pair.slave; // keep alive: dropping closed ConPTY handles under the child
    std::thread::spawn(move || {
        let t = std::time::Instant::now();
        let st = child.wait();
        eprintln!("[smoke] child exited after {:?}: {:?}", t.elapsed(), st);
    });
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    use std::io::{Read, Write};
    let mut buf = [0u8; 4096];
    let mut total = 0usize;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match reader.read(&mut buf) {
            Ok(0) => { eprintln!("[smoke] EOF"); break; }
            Ok(n) => {
                total += n;
                let s = String::from_utf8_lossy(&buf[..n]);
                eprintln!("[smoke] read {} bytes: {:?}", n, &s[..s.len().min(300)]);
                eprintln!("[smoke] hex: {:02x?}", &buf[..n]);
                if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                    eprintln!("[smoke] -> answering DSR with ESC[1;1R");
                    writer.write_all(b"\x1b[1;1R")?;
                    writer.flush()?;
                }
            }
            Err(e) => { eprintln!("[smoke] err: {e}"); break; }
        }
    }
    eprintln!("[smoke] total bytes: {}", total);
    Ok(())
}

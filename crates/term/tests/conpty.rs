//! ConPTY integration tests (Windows only; unit tests run on all platforms).
//!
//! Ports the guarantees of Spike 1's `examples/pty_smoke.rs` and e2e driver:
//! echo roundtrip, session surviving a resize with serialized input, and the
//! DSR probe being answered so a program that blocks on `ESC[6n` proceeds.
//! Every test bounds its runtime and the session kills the child on drop, so
//! a failing assertion cannot leak processes.

#![cfg(windows)]

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tiller_term::{OutputReceiver, PtySession, SpawnConfig};

fn cmd_cfg(args: &[&str]) -> SpawnConfig {
    SpawnConfig {
        program: "cmd.exe".into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        env: vec![],
        cwd: None,
        cols: 80,
        rows: 24,
    }
}

/// Accumulate output until `needle` appears, the stream closes, or the
/// deadline passes (panic with what was collected so far).
fn read_until(output: &OutputReceiver, needle: &[u8], timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut acc = Vec::new();
    loop {
        if acc.windows(needle.len().max(1)).any(|w| w == needle) {
            return acc;
        }
        let Some(remain) = deadline.checked_duration_since(Instant::now()) else {
            panic!(
                "timed out after {timeout:?} waiting for {:?}; stream so far: {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&acc)
            );
        };
        match output.recv_timeout(remain) {
            Ok(bytes) => acc.extend_from_slice(&bytes),
            Err(RecvTimeoutError::Timeout) => {
                panic!(
                    "timed out after {timeout:?} waiting for {:?}; stream so far: {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&acc)
                );
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!(
                    "output closed before {:?} appeared; stream: {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&acc)
                );
            }
        }
    }
}

/// Drain the output stream until the pump closes it (program exit, EOF).
fn drain_to_close(output: &OutputReceiver, timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut acc = Vec::new();
    loop {
        let Some(remain) = deadline.checked_duration_since(Instant::now()) else {
            panic!(
                "timed out draining output; stream so far: {:?}",
                String::from_utf8_lossy(&acc)
            );
        };
        match output.recv_timeout(remain) {
            Ok(bytes) => acc.extend_from_slice(&bytes),
            Err(RecvTimeoutError::Disconnected) => return acc,
            Err(RecvTimeoutError::Timeout) => {
                panic!(
                    "timed out draining output; stream so far: {:?}",
                    String::from_utf8_lossy(&acc)
                );
            }
        }
    }
}

/// Port of the spike's smoke test: spawn, marker roundtrip, clean exit.
/// The marker is checked on the *combined* stream (pre-exit reads +
/// post-exit drain) — closing the master at exit must not lose any output
/// that was already in flight.
#[test]
fn echo_roundtrip_and_exit_status() {
    let session = PtySession::spawn(cmd_cfg(&["/c", "echo TILLER-TERM-ECHO-OK"]))
        .expect("spawn cmd.exe /c echo");
    let out = session.output();
    let combined = {
        let mut combined = read_until(out, b"TILLER-TERM-ECHO-OK", Duration::from_secs(30));
        combined.extend_from_slice(&drain_to_close(out, Duration::from_secs(30)));
        combined
    };
    assert!(
        combined
            .windows(b"TILLER-TERM-ECHO-OK".len())
            .any(|w| w == b"TILLER-TERM-ECHO-OK"),
        "marker survived to stream end"
    );
    let status = session.wait();
    assert!(
        status.success(),
        "cmd echo exits 0, got code {}",
        status.exit_code()
    );
}

/// The DSR-block regression: a program that prints `ESC[6n` and blocks until
/// an answer arrives must proceed (Spike 1: pi's TUI hung with CPU=0 without
/// an answer). PowerShell stands in for the TUI: it clears the console's
/// line-input mode first (a DSR reply `ESC[1;1R` carries no CR, so a
/// line-mode reader would wait for an Enter that never comes — that pitfall
/// cost the previous slice a hanging test), writes the probe, reads the
/// reply char-by-char, and only then prints the marker.
#[test]
fn dsr_probe_is_answered_and_blocking_program_proceeds() {
    // Raw-mode stdin via SetConsoleMode (clear ENABLE_LINE_INPUT), then:
    // write ESC[6n, read chars until the reply's final 'R', print OK/FAIL.
    let script = r#"$k='[DllImport("kernel32.dll",SetLastError=true)]public static extern IntPtr GetStdHandle(int s);[DllImport("kernel32.dll",SetLastError=true)]public static extern bool GetConsoleMode(IntPtr h,out int m);[DllImport("kernel32.dll",SetLastError=true)]public static extern bool SetConsoleMode(IntPtr h,int m);'
Add-Type -MemberDefinition $k -Name K -Namespace TillerTerm
$h=[TillerTerm.K]::GetStdHandle(-10)
$m=0
[void][TillerTerm.K]::GetConsoleMode($h,[ref]$m)
[void][TillerTerm.K]::SetConsoleMode($h,($m -band (-bnot 2)))
[Console]::Out.Write([string][char]27+'[6n')
$s=''
while(-not $s.EndsWith('R')){$c=[Console]::In.Read();if($c -lt 0){break};$s+=[char]$c}
if($s -match '^\x1b\[\d+;\d+R$'){'TILLER-DSR-OK'}else{'TILLER-DSR-FAIL:'+$s}"#;
    let session = PtySession::spawn(SpawnConfig {
        program: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.into(),
        ],
        env: vec![],
        cwd: None,
        cols: 80,
        rows: 24,
    })
    .expect("spawn powershell probe script");
    let stream = read_until(session.output(), b"TILLER-DSR", Duration::from_secs(60));
    let text = String::from_utf8_lossy(&stream);
    assert!(
        text.contains("TILLER-DSR-OK"),
        "probe answered, got: {text:?}"
    );
    assert!(session.dsr_responses_sent() >= 1, "the reply was counted");
    assert!(session.wait().success());
}

/// The DSR-starvation regression (P2 review blocker): the program sends its
/// probe and keeps a DSR *deadline*; meanwhile the host floods the input
/// with several times the pipe capacity. The reply must overtake the bulk
/// input — the probe's sender may be the only party able to drain that
/// input, so a reply queued behind it starves (pre-fix: FIFO put the reply
/// after the whole burst → deadline blown; the owner answers pending
/// replies before and between 64 KiB input chunks, so the reply lands early
/// in the stream). The program reads continuously (consuming the flood),
/// reports when the reply arrived (wall-clock ms + stream offset), and only
/// then succeeds; a starved reply or truncated input fails the test. No
/// side can deadlock: the program always reads, the host write always
/// drains.
#[test]
fn dsr_reply_overtakes_bulk_input() {
    const PAYLOAD: usize = 2 * 1024 * 1024; // 2 MiB of 'A', several pipes' worth
    const REPLY_DEADLINE_MS: i64 = 5_000; // generous vs the ~1 s fixed path
    const REPLY_MAX_OFFSET: usize = PAYLOAD / 4 * 3; // ¾ of the payload: the reply must land well before the burst's tail
    let script = format!(
        r#"$k='[DllImport("kernel32.dll",SetLastError=true)]public static extern IntPtr GetStdHandle(int s);[DllImport("kernel32.dll",SetLastError=true)]public static extern bool GetConsoleMode(IntPtr h,out int m);[DllImport("kernel32.dll",SetLastError=true)]public static extern bool SetConsoleMode(IntPtr h,int m);'
Add-Type -MemberDefinition $k -Name K -Namespace TillerTerm
$h=[TillerTerm.K]::GetStdHandle(-10)
$m=0
[void][TillerTerm.K]::GetConsoleMode($h,[ref]$m)
[void][TillerTerm.K]::SetConsoleMode($h,($m -band (-bnot 2)))
[Console]::Out.Write([string][char]27+'[6n')
[Console]::Out.Flush()
$sw=[Diagnostics.Stopwatch]::StartNew()
$target={PAYLOAD}
$buf=New-Object char[] 65536
$total=0
$replyAt=-1
$replyMs=-1L
while($true){{
  if($replyAt -lt 0 -and $sw.ElapsedMilliseconds -gt {REPLY_DEADLINE_MS}){{
    [Console]::Out.Write("TILLER-DONE-STARVED:total=$total");[Console]::Out.Flush();exit 9
  }}
  if($replyAt -ge 0 -and $total -ge ($target+6)){{break}}
  $n=[Console]::In.Read($buf,0,$buf.Length)
  if($n -le 0){{[Console]::Out.Write("TILLER-DONE-EOF:total=$total replyAt=$replyAt");[Console]::Out.Flush();exit 8}}
  if($replyAt -lt 0){{
    $i=[Array]::IndexOf($buf,[char]27,0,$n)
    if($i -ge 0){{
      $replyAt=$total+$i
      $replyMs=$sw.ElapsedMilliseconds
      [Console]::Out.Write("TILLER-REPLY-MS:$replyMs|AT:$replyAt")
      [Console]::Out.Flush()
    }}
  }}
  $total+=$n
}}
[Console]::Out.Write("TILLER-DONE-OK:total=$total")
[Console]::Out.Flush()
exit 0"#
    );
    let started = Instant::now();
    let session = PtySession::spawn(SpawnConfig {
        program: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
        env: vec![],
        cwd: None,
        cols: 80,
        rows: 24,
    })
    .expect("spawn powershell flood script");

    // Flood the input from a side thread while the main thread watches the
    // markers. The write must drain completely (no loss, no deadlock).
    let writer_session = std::sync::Arc::new(session);
    let reader_session = std::sync::Arc::clone(&writer_session);
    let writer = std::thread::spawn(move || writer_session.write(&vec![b'A'; PAYLOAD]));

    let stream = read_until(
        reader_session.output(),
        b"TILLER-DONE",
        Duration::from_secs(15),
    );
    let text = String::from_utf8_lossy(&stream).to_string();
    let reply_ms: i64 = text
        .split("TILLER-REPLY-MS:")
        .nth(1)
        .and_then(|rest| rest.split('|').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(-1);
    let reply_at: i64 = text
        .split("|AT:")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(-1);
    assert!(
        reply_ms >= 0,
        "DSR reply arrived (ms={reply_ms}); stream: {text:?}"
    );
    assert!(
        reply_ms < REPLY_DEADLINE_MS,
        "DSR reply starved behind bulk input: {reply_ms} ms; stream: {text:?}"
    );
    assert!(
        reply_at >= 0 && (reply_at as usize) < REPLY_MAX_OFFSET,
        "DSR reply must land before the burst's tail (at={reply_at}); stream: {text:?}"
    );
    assert!(
        text.contains("TILLER-DONE-OK"),
        "input complete, no deadlock; stream: {text:?}"
    );
    writer
        .join()
        .expect("writer thread")
        .expect("write drained");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "bounded runtime"
    );
    assert!(reader_session.dsr_responses_sent() >= 1);
    assert!(reader_session.wait().success(), "script exited 0");
}

/// Port of the spike's resize criteria: resize accepted by the PTY (real
/// result, not fire-and-forget), then input still reaches the program and
/// the session exits with the code the input requested.
#[test]
fn resize_is_acked_and_session_survives_with_input() {
    let session = PtySession::spawn(cmd_cfg(&[])).expect("spawn interactive cmd.exe");
    let out = session.output();
    {
        read_until(out, b"Microsoft", Duration::from_secs(60));
    }
    session.resize(100, 30).expect("resize accepted by the PTY");
    session
        .write(b"exit 7\r\n")
        .expect("input written after resize");
    let status = session.wait();
    assert_eq!(status.exit_code(), 7, "input reached cmd after the resize");
    {
        let stream = drain_to_close(out, Duration::from_secs(30));
        assert!(!stream.is_empty(), "output flowed until the end");
    }
}

/// Kill terminates the hosted program, unblocks the output reader and the
/// reaper reports a status. Same mechanism as EOF-after-exit: the reaper
/// closes the master once the child is reaped, which is what unblocks a
/// reader parked in ConPTY `read()`.
#[test]
fn kill_terminates_unblocks_reader_and_reports_status() {
    let session = PtySession::spawn(SpawnConfig {
        program: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 120".into(),
        ],
        env: vec![],
        cwd: None,
        cols: 80,
        rows: 24,
    })
    .expect("spawn powershell sleep");
    {
        read_until(session.output(), b"\x1b", Duration::from_secs(60));
    }
    session.kill().expect("kill");
    // One budget for the whole kill contract — status AND reader EOF.
    // 10 s = the 2 s escalation grace (Unix) plus reap/master-close
    // slack, with ~5x margin; far below CI's per-test patience.
    const KILL_BUDGET: Duration = Duration::from_secs(10);
    let killed_at = Instant::now();
    let status = loop {
        if let Some(status) = session.try_wait() {
            break status;
        }
        assert!(
            Instant::now() < killed_at + KILL_BUDGET,
            "killed program did not report a status within the kill budget"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success(), "killed program reports failure");
    // The reader must unblock: the output stream disconnects (EOF) without
    // anyone dropping the session. Same budget as the status — one kill,
    // one deadline; real, not assumed immediacy.
    {
        let remain = (killed_at + KILL_BUDGET).saturating_duration_since(Instant::now());
        drain_to_close(session.output(), remain);
    }
    assert!(
        killed_at.elapsed() < KILL_BUDGET,
        "status and EOF within the same kill budget"
    );
}

/// The reaper's contract: once the child exits, the output stream reaches
/// EOF (stream disconnect) in a bounded time *while the session lives*.
/// ConPTY keeps the output pipe open as long as the pseudoconsole exists,
/// so nothing less than closing the master delivers EOF — this test is the
/// regression lock for that (bisect: control/writer-close hang forever;
/// master-close EOFs ~20ms after exit with the full output intact).
///
/// No-loss is asserted on the *whole* expected stream: a first marker, a
/// middle body, and a final marker printed right before exit — the
/// post-exit drain must deliver the tail bytes after the final marker
/// (line endings and all), not just the part read before the exit.
#[test]
fn reader_reaches_eof_soon_after_child_exit() {
    let session = PtySession::spawn(cmd_cfg(&[
        "/c",
        "echo TILLER-NOLOSS-FIRST& (for /L %i in (1,1,200) do @echo noloss-mid-%i)& echo TILLER-NOLOSS-LAST",
    ]))
    .expect("spawn cmd.exe /c echo sequence");
    let out = session.output();
    let pre_exit = read_until(out, b"TILLER-NOLOSS-FIRST", Duration::from_secs(30));
    // Observe the exit as early as possible; the clock starts there.
    let deadline = Instant::now() + Duration::from_secs(30);
    while session.try_wait().is_none() {
        assert!(Instant::now() < deadline, "child did not exit in time");
        std::thread::sleep(Duration::from_millis(10));
    }
    let exited_at = Instant::now();
    let drained = drain_to_close(out, Duration::from_secs(5));
    let eof_after_exit = exited_at.elapsed();
    assert!(
        eof_after_exit < Duration::from_secs(5),
        "EOF came {eof_after_exit:?} after the observed exit; the reaper must close the master"
    );
    // No-loss on the combined stream: first marker (read pre-exit) + the
    // whole middle + the final marker written just before exit.
    let mut combined = pre_exit;
    combined.extend_from_slice(&drained);
    let text = String::from_utf8_lossy(&combined);
    let pos = |needle: &str| text.find(needle);
    let first = pos("TILLER-NOLOSS-FIRST").expect("first marker present");
    let mid = pos("noloss-mid-100").expect("middle body present");
    let last = pos("TILLER-NOLOSS-LAST").expect("final marker survived the master close");
    assert!(first < mid && mid < last, "stream order intact");
    assert_eq!(
        text.matches("noloss-mid-").count(),
        200,
        "every middle line survived"
    );
    // Bytes *after* the final marker (its line ending) must have arrived:
    // the tail has to be a non-empty run of terminators — `.all` on an
    // empty tail could not fail, so the end of the stream is truly seen.
    let tail = &combined[last + b"TILLER-NOLOSS-LAST".len()..];
    assert!(
        !tail.is_empty() && tail.iter().all(|&b| matches!(b, b'\r' | b'\n' | b' ')),
        "no data lost after the final marker: the tail must be a non-empty run of terminators; tail: {:?}",
        String::from_utf8_lossy(tail)
    );
}

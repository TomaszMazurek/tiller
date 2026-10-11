//! PtySession smoke check: spawn a hosted program, write input to it,
//! collect the session's output, report the exit status.
//!
//! This exercises the production seam exactly as a consumer would (the
//! old version drove portable-pty directly, `mem::forget`-ed the slave,
//! and its deadline could not interrupt a blocking read — P2 review).
//! The session owns DSR answering, the bounded output buffer, and
//! teardown; the example only adds a real time budget: if the program
//! stays quiet until the budget is spent, the session is killed and the
//! stream drains to EOF instead of hanging.
//!
//! Usage: `cargo run -p tiller-term --example conpty_smoke -- [program args...]`
//! Default: an echo roundtrip (`powershell.exe` on Windows, `sh` elsewhere).

use std::error::Error;
use std::io::Write;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tiller_term::{PtySession, SpawnConfig};

/// Overall budget for a quiet-but-alive program before the smoke kills it.
const BUDGET: Duration = Duration::from_secs(10);

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (program, args, roundtrip): (String, Vec<String>, bool) = if args.is_empty() {
        // One echo roundtrip, then exit: read the line the smoke writes,
        // print it back, terminate by itself.
        if cfg!(windows) {
            (
                "powershell.exe".into(),
                vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    "$l=[Console]::In.ReadLine(); 'smoke-read:'+$l".into(),
                ],
                true,
            )
        } else {
            (
                "sh".into(),
                vec!["-c".into(), "read l; echo smoke-read:$l".into()],
                true,
            )
        }
    } else {
        // Custom program: input protocol unknown — observe only.
        (args[0].clone(), args[1..].to_vec(), false)
    };

    let session = PtySession::spawn(SpawnConfig {
        program,
        args,
        env: vec![],
        cwd: None,
        cols: 80,
        rows: 24,
    })?;
    if roundtrip {
        session.write(b"smoke-in\r\n")?;
    }

    let started = Instant::now();
    let mut total = 0usize;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        let remain = BUDGET.saturating_sub(started.elapsed());
        match session.output().recv_timeout(remain) {
            Ok(bytes) => {
                total += bytes.len();
                out.write_all(&bytes)?;
                out.flush()?;
            }
            Err(RecvTimeoutError::Disconnected) => break,
            // Quiet for the whole budget: kill so the teardown (exit →
            // master close) delivers EOF instead of parking this loop.
            Err(RecvTimeoutError::Timeout) => {
                eprintln!("[smoke] budget spent; killing the session");
                session.kill()?;
                break;
            }
        }
    }
    // Drain what the teardown still flushes (kill → exit → master close).
    while let Ok(bytes) = session.output().recv_timeout(Duration::from_secs(5)) {
        total += bytes.len();
        out.write_all(&bytes)?;
    }
    drop(out);
    let status = session.wait();
    println!(
        "[smoke] {total} bytes, dsr replies: {}, status: {:?}, elapsed: {:?}",
        session.dsr_responses_sent(),
        status,
        started.elapsed()
    );
    if let Some(err) = session.output_error() {
        eprintln!("[smoke] output error: {err}");
    }
    Ok(())
}

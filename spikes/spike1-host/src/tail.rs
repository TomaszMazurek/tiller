//! Incremental JSONL tailing for the pi session directory.
//!
//! The host spawns pi with `--session-dir <spike>/.pi-run/sessions`, so the
//! session JSONL pi writes is exactly the file `PI_SESSION_FILE` points at for
//! its tool children. We poll for appended bytes; partial lines are buffered
//! until a newline arrives.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Tails a single file from a byte offset, yielding complete lines.
pub struct FileTail {
    path: PathBuf,
    offset: u64,
    buf: Vec<u8>,
}

impl FileTail {
    pub fn new(path: PathBuf, start_offset: u64) -> Self {
        Self { path, offset: start_offset, buf: Vec::new() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read appended bytes and return lines completed since the last poll.
    pub fn poll(&mut self) -> io::Result<Vec<String>> {
        let mut f = match OpenOptions::new().read(true).open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let len = f.metadata()?.len();
        if len <= self.offset {
            return Ok(vec![]);
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let mut chunk = Vec::with_capacity((len - self.offset) as usize);
        let n = f.read_to_end(&mut chunk)?;
        self.offset += n as u64;
        self.buf.extend_from_slice(&chunk);

        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(String::from_utf8_lossy(&line).into_owned());
        }
        Ok(lines)
    }
}

/// Watches a directory for session `*.jsonl` files.
///
/// Files that already exist when the watcher is created are tailed from their
/// current end (baseline); files that appear later are tailed from byte 0 so a
/// freshly created session reports its full history.
pub struct SessionDirWatcher {
    dir: PathBuf,
    known: Vec<PathBuf>,
    tails: Vec<FileTail>,
}

impl SessionDirWatcher {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, known: vec![], tails: vec![] }
    }

    /// Discover new *.jsonl files and collect newly appended lines from all.
    pub fn poll(&mut self) -> io::Result<Vec<(PathBuf, String)>> {
        let mut discovered: Vec<PathBuf> = vec![];
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    discovered.push(p);
                }
            }
        }
        discovered.sort();
        for p in discovered {
            if !self.known.contains(&p) {
                self.known.push(p.clone());
                // New files (not registered by `baseline_existing`) are tailed
                // from byte 0 so a fresh session reports its full history.
                self.tails.push(FileTail::new(p, 0));
            }
        }

        let mut out = vec![];
        for t in self.tails.iter_mut() {
            for line in t.poll()? {
                out.push((t.path().to_path_buf(), line));
            }
        }
        Ok(out)
    }
}

impl SessionDirWatcher {
    /// Variant of the constructor that baselines files present right now.
    pub fn baseline_existing(dir: PathBuf) -> io::Result<Self> {
        let mut w = SessionDirWatcher::new(dir.clone());
        if let Ok(rd) = fs::read_dir(&dir) {
            let mut present: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .collect();
            present.sort();
            for p in present {
                let len = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                w.known.push(p.clone());
                w.tails.push(FileTail::new(p, len));
            }
        }
        Ok(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_jsonl(tag: &str) -> PathBuf {
        let uniq = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("spike1-tail-{}-{}.jsonl", tag, uniq));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn yields_lines_and_buffers_partial() {
        let p = temp_jsonl("partial");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"type":"a"}}"#).unwrap();
        writeln!(f, r#"{{"type":"b"}}"#).unwrap();

        let mut t = FileTail::new(p.clone(), 0);
        let lines = t.poll().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], r#"{"type":"a"}"#);

        // append half a line, then complete it
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, r#"{{"type":"c""#).unwrap();
        assert_eq!(t.poll().unwrap().len(), 0, "partial line must not be yielded");
        write!(f, r#","x":1}}"#).unwrap();
        writeln!(f).unwrap();
        let lines = t.poll().unwrap();
        assert_eq!(lines, vec![r#"{"type":"c","x":1}"#.to_string()]);

        // no new data -> no lines, no error
        assert!(t.poll().unwrap().is_empty());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn crlf_lines_are_normalized() {
        let p = temp_jsonl("crlf");
        let mut f = fs::File::create(&p).unwrap();
        write!(f, "{{\"type\":\"a\"}}\r\n").unwrap();
        let mut t = FileTail::new(p.clone(), 0);
        let lines = t.poll().unwrap();
        assert_eq!(lines, vec!["{\"type\":\"a\"}".to_string()]);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn start_offset_skips_existing_content() {
        let p = temp_jsonl("offset");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"type":"old"}}"#).unwrap();
        let len = fs::metadata(&p).unwrap().len();
        let mut t = FileTail::new(p.clone(), len);
        assert!(t.poll().unwrap().is_empty());
        writeln!(f, r#"{{"type":"new"}}"#).unwrap();
        assert_eq!(t.poll().unwrap(), vec![r#"{"type":"new"}"#.to_string()]);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn watcher_picks_up_new_file_from_zero() {
        let dir = std::env::temp_dir().join(format!(
            "spike1-watch-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut w = SessionDirWatcher::baseline_existing(dir.clone()).unwrap();
        assert!(w.poll().unwrap().is_empty());

        let sess = dir.join("20260101_spikedemo.jsonl");
        let mut f = fs::File::create(&sess).unwrap();
        writeln!(f, r#"{{"type":"session","id":"s1"}}"#).unwrap();
        let got = w.poll().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, sess);
        assert!(got[0].1.contains("\"session\""));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn watcher_ignores_non_jsonl() {
        let dir = std::env::temp_dir().join(format!(
            "spike1-watch2-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut w = SessionDirWatcher::baseline_existing(dir.clone()).unwrap();
        fs::write(dir.join("notes.txt"), "hello").unwrap();
        assert!(w.poll().unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}

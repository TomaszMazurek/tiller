//! Session timeline: tails the pi session JSONL of a hosted sandbox and
//! republishes its entries as `session:entry` notifications.
//!
//! Brief §5 rule 2: the cockpit does not render its own agent chat — the
//! timeline is a *derived* status/event view built from observing the
//! session file pi itself writes (`PI_CODING_AGENT_DIR/sessions/…`).
//! The privacy boundary is structural: an entry carries only roles,
//! tool names, block counts and lengths — never message text, tool
//! arguments or result content. Those can carry credentials, and no
//! preview is short enough to be safe by construction; statistics are.
//!
//! Discovery (empirically verified against pi 1.1.0, see the probe notes
//! in the P4 report): pi writes session files under
//! `{agent_dir}/sessions/{project-slug}/{timestamp}_{uuid}.jsonl` (older
//! pis kept them flat in `sessions/` — the spike 1 samples are flat), and
//! **lazily**: a quiet boot creates only the empty project directory; the
//! JSONL appears with the `session`/`model_change`/`thinking_level_change`
//! header lines on the first submitted message. Discovery therefore walks
//! the whole `sessions` subtree and the tailer simply keeps polling until
//! a file shows up.
//!
//! The tailer polls every [`TAIL_POLL`] instead of using a file-watch
//! crate (notify): the entries are human-paced, one file per session, and
//! polling avoids a new dependency plus notify's platform quirks — a
//! 250 ms cadence is invisible next to an agent turn.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::term_host::{EventSink, SessionId};

/// Tauri event carrying one derived timeline entry: `{session_id, entry}`.
pub const EVENT_ENTRY: &str = "session:entry";

/// Tailer poll cadence (see the module docs for why polling).
const TAIL_POLL: Duration = Duration::from_millis(250);

/// One derived timeline entry. Wire shape: internally tagged with `kind`,
/// snake_case everywhere (consistent with the `term.*` surface).
///
/// Every variant is a statistic, never content (see the module docs):
/// message text is reduced to `chars`/`blocks`, tool arguments to
/// `args_chars`, results to the outcome and duration.
///
/// `id` is unique per session (the JSONL entry id, or the tool-call id),
/// which lets the UI dedupe live events against a snapshot and key list
/// rows stably. `ts` is pi's ISO-8601 entry timestamp. `tool_call_id` is
/// the correlation seam: pi echoes it (together with `tool_name`) on the
/// matching result, so a result row can point back at its call — on a
/// [`TimelineEntry::ToolCall`] it duplicates `id` (the tool-call block id
/// serves both as the dedup key and the correlation key).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineEntry {
    Message {
        id: String,
        role: MessageRole,
        ts: String,
        /// Total content length in `char`s (all text blocks summed).
        chars: usize,
        /// Number of content blocks of any type (text, toolCall, image, …).
        blocks: usize,
    },
    ToolCall {
        id: String,
        name: String,
        ts: String,
        tool_call_id: String,
        /// Length of the arguments' compact JSON, in `char`s.
        args_chars: usize,
    },
    ToolResult {
        id: String,
        ts: String,
        tool_call_id: String,
        tool_name: String,
        ok: bool,
        duration_ms: u64,
    },
}

/// The message roles the timeline shows; pi's `system` preamble (and any
/// future role) is deliberately not observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

/// Parse one JSONL line into timeline entries.
///
/// One line can yield several entries (an assistant message may carry
/// text *and* multiple tool calls in one content list), which is why the
/// answer is a `Vec` rather than an `Option`. Anything unrecognized — a
/// mangled line, a non-`message` entry type (session header,
/// `model_change`, …), a `system` message, a future role — maps to an
/// empty vec: unknown shapes are skipped, never fatal (forward-compat).
pub fn parse_entry(line: &str) -> Vec<TimelineEntry> {
    let entry: RawEntry = match serde_json::from_str(line) {
        Ok(entry) => entry,
        Err(_) => return Vec::new(),
    };
    let Some(message) = entry.message else {
        return Vec::new();
    };
    let ts = entry.timestamp;
    match message.role.as_str() {
        "user" => {
            // Stats only: a blank message (pi's empty turn) observes as
            // nothing, same as before the stats redesign.
            let (chars, visible) = message.content.text_stats();
            if !visible {
                Vec::new()
            } else {
                vec![TimelineEntry::Message {
                    id: entry.id,
                    role: MessageRole::User,
                    ts,
                    chars,
                    blocks: message.content.block_count(),
                }]
            }
        }
        "assistant" => {
            let (chars, visible) = message.content.text_stats();
            let mut entries = Vec::new();
            if visible {
                entries.push(TimelineEntry::Message {
                    id: entry.id.clone(),
                    role: MessageRole::Assistant,
                    ts: ts.clone(),
                    chars,
                    // Counts every block (tool calls and images included),
                    // so one line's message stat covers its whole content.
                    blocks: message.content.block_count(),
                });
            }
            entries.extend(message.content.parts().filter_map(|part| match part {
                RawPart::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some(TimelineEntry::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    ts: ts.clone(),
                    tool_call_id: id.clone(),
                    args_chars: json_chars(arguments),
                }),
                // Image blocks and any future block type contribute
                // nothing beyond the message's `blocks` count.
                _ => None,
            }));
            entries
        }
        "toolResult" => vec![TimelineEntry::ToolResult {
            id: entry.id,
            ts,
            tool_call_id: message.tool_call_id,
            tool_name: message.tool_name,
            ok: !message.is_error,
            duration_ms: message.duration_ms,
        }],
        _ => Vec::new(),
    }
}

/// Length of the arguments' compact JSON in `char`s — the only thing
/// that ever crosses the wire about the arguments.
fn json_chars(value: &serde_json::Value) -> usize {
    serde_json::to_string(value)
        .map(|json| json.chars().count())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Raw JSONL shapes (pi session format v3; only the fields the timeline
// observes are declared — serde ignores the rest, so richer real entries
// keep parsing).
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    message: Option<RawMessage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMessage {
    role: String,
    #[serde(default)]
    content: RawContent,
    #[serde(default)]
    is_error: bool,
    /// toolResult only: pi echoes the call's id (suffixed with the
    /// provider's) and the tool name — the correlation pair.
    #[serde(default)]
    tool_call_id: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    duration_ms: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawContent {
    Text(String),
    Parts(Vec<RawPart>),
}

impl Default for RawContent {
    fn default() -> Self {
        RawContent::Text(String::new())
    }
}

impl RawContent {
    /// `(total text length in chars, any non-whitespace text?)` —
    /// computed block by block; the text itself never leaves here.
    fn text_stats(&self) -> (usize, bool) {
        let one = |text: &str| (text.chars().count(), !text.trim().is_empty());
        match self {
            RawContent::Text(text) => {
                let (chars, visible) = one(text);
                (chars, visible)
            }
            RawContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    RawPart::Text { text } => Some(one(text)),
                    _ => None,
                })
                .fold((0, false), |(chars, visible), (c, v)| {
                    (chars + c, visible || v)
                }),
        }
    }

    /// Number of content blocks of any type (text, toolCall, image, …).
    fn block_count(&self) -> usize {
        match self {
            RawContent::Text(_) => 1,
            RawContent::Parts(parts) => parts.len(),
        }
    }

    fn parts(&self) -> impl Iterator<Item = &RawPart> {
        match self {
            RawContent::Parts(parts) => parts.iter(),
            RawContent::Text(_) => [].iter(),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum RawPart {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// Image blocks and any future block type.
    #[serde(other)]
    Other,
}

// ---------------------------------------------------------------------------
// Discovery and tailing.
// ---------------------------------------------------------------------------

/// The newest `*.jsonl` anywhere under `{sandbox}/sessions` (newest mtime
/// wins). `None` while pi has not created a session yet — pi 1.1.0 does
/// that lazily, on the first submitted message.
pub fn discover_session_file(sandbox: &Path) -> Option<PathBuf> {
    newest_jsonl(&sandbox.join("sessions"))
}

fn newest_jsonl(dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let candidate = if path.is_dir() {
            // pi 1.1.0 groups sessions per project slug; older pis kept
            // files flat — both (and deeper nests) are searched.
            newest_jsonl(&path).map(|found| (mtime(&found), found))
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            Some((mtime(&path), path))
        } else {
            None
        };
        if let Some((time, found)) = candidate {
            if newest
                .as_ref()
                .map(|(best, _)| time > *best)
                .unwrap_or(true)
            {
                newest = Some((time, found));
            }
        }
    }
    newest.map(|(_, found)| found)
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(UNIX_EPOCH)
}

/// Parse the whole current session file (newest wins). No file yet → an
/// empty timeline, not an error — the session simply has not started.
pub fn snapshot(sandbox: &Path) -> Vec<TimelineEntry> {
    discover_session_file(sandbox)
        .and_then(|file| fs::read_to_string(file).ok())
        .map(|text| text.lines().flat_map(parse_entry).collect())
        .unwrap_or_default()
}

/// Bookkeeping of one tailing session; [`TailerState::poll_once`] is the
/// (deterministic, test-drivable) single step of [`run_tailer`].
pub(crate) struct TailerState {
    sandbox: PathBuf,
    /// The tracked session file; switching files (a `/new` inside pi)
    /// restarts from offset 0.
    file: Option<PathBuf>,
    /// Bytes consumed through the last complete line of the tracked file.
    offset: u64,
    /// Raw bytes after the last complete newline — a half-written line
    /// pi has not finished yet. Kept as *bytes* so a multi-byte UTF-8
    /// sequence split across reads survives (decoding happens per
    /// complete line, never per read).
    partial: Vec<u8>,
}

impl TailerState {
    pub(crate) fn new(sandbox: PathBuf) -> Self {
        Self {
            sandbox,
            file: None,
            offset: 0,
            partial: Vec::new(),
        }
    }

    /// One poll cycle: (re-)discover the session file, read new bytes,
    /// emit one `session:entry` per parsed entry. Returns how many
    /// entries were emitted. Never panics on a missing/vanished file —
    /// a vanished file just resets tracking for the next cycle.
    pub(crate) fn poll_once(&mut self, session_id: SessionId, sink: &dyn EventSink) -> usize {
        // Discovery runs every cycle: the file appears lazily (first
        // message) and a newer session file may replace the tracked one.
        let Some(latest) = discover_session_file(&self.sandbox) else {
            // No session file anywhere — pi has not written one yet, or
            // the tracked one was deleted. Drop all tracking: a file
            // recreated under the same path must be read from offset 0
            // (the stale offset would skip a same-length file entirely
            // and read a longer one from the middle).
            self.file = None;
            self.offset = 0;
            self.partial.clear();
            return 0;
        };
        if self.file.as_deref() != Some(latest.as_path()) {
            self.file = Some(latest);
            self.offset = 0;
            self.partial.clear();
        }
        let file = self.file.clone().expect("file just set");
        match read_new_bytes(&file, self.offset) {
            Ok(Some(bytes)) => {
                self.offset += bytes.len() as u64;
                self.partial.extend_from_slice(&bytes);
                let mut emitted = 0;
                while let Some(nl) = self.partial.iter().position(|b| *b == b'\n') {
                    let line_bytes: Vec<u8> = self.partial.drain(..=nl).collect();
                    // A complete line decodes as UTF-8; lossy decoding is
                    // only the last-resort fallback for a mangled line
                    // (which parse_entry skips anyway).
                    let line = match String::from_utf8(line_bytes) {
                        Ok(line) => line,
                        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
                    };
                    for entry in parse_entry(line.trim()) {
                        sink.emit(
                            EVENT_ENTRY,
                            serde_json::json!({ "session_id": session_id, "entry": entry }),
                        );
                        emitted += 1;
                    }
                }
                emitted
            }
            // Nothing new since the last poll.
            Ok(None) => 0,
            // Open/seek failed (file deleted mid-cycle) or the file
            // shrank (replaced): drop tracking, rediscover next cycle.
            Err(_) => {
                self.file = None;
                self.offset = 0;
                self.partial.clear();
                0
            }
        }
    }
}

/// Bytes of `path` after `offset`; `Ok(None)` when nothing new. An error
/// means the file vanished or shrank (was replaced) — the caller resets.
fn read_new_bytes(path: &Path, offset: u64) -> io::Result<Option<Vec<u8>>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < offset {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file shrank under the tail offset",
        ));
    }
    if len == offset {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::with_capacity((len - offset) as usize);
    file.read_to_end(&mut buffer)?;
    Ok(Some(buffer))
}

/// The tailer thread body for one observed session: poll, emit, sleep,
/// until `stop` is set — then one final drain, because the flag is only
/// set after the hosted program exited, so the file is final and lines
/// written since the last poll still deserve a look.
pub(crate) fn run_tailer(
    sink: Arc<dyn EventSink>,
    session_id: SessionId,
    sandbox: PathBuf,
    stop: Arc<AtomicBool>,
) {
    let mut state = TailerState::new(sandbox);
    while !stop.load(Ordering::Relaxed) {
        state.poll_once(session_id, sink.as_ref());
        std::thread::sleep(TAIL_POLL);
    }
    state.poll_once(session_id, sink.as_ref());
}

/// Handle pair the session actor uses to wind the tailer down at teardown.
pub(crate) type TailerHandle = (Arc<AtomicBool>, JoinHandle<()>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term_host::test_util::RecordingSink;
    use std::sync::mpsc;
    use std::time::Instant;

    // Real lines from the spike 1 pi samples (pi session format v3),
    // verbatim — the parser contract is pi's on-disk format, not a
    // paraphrase of it.

    #[test]
    fn non_message_entry_types_are_ignored() {
        let session = r#"{"type":"session","version":3,"id":"01a12afb-1028-72e9-b27e-b581003005aa","timestamp":"2026-10-11T12:40:59.177Z","cwd":"C:\\Repos\\MyIDE\\spikes\\spike1-host"}"#;
        let model_change = r#"{"type":"model_change","id":"b7a3ccd9","parentId":null,"timestamp":"2026-10-11T12:40:59.299Z","provider":"openai-codex","modelId":"gpt-6.1-sol"}"#;
        let thinking = r#"{"type":"thinking_level_change","id":"2aaa1ef8","parentId":"b7a3ccd9","timestamp":"2026-10-11T12:40:59.299Z","thinkingLevel":"medium"}"#;
        for line in [session, model_change, thinking] {
            assert!(
                parse_entry(line).is_empty(),
                "non-message entry must be ignored: {line}"
            );
        }
    }

    #[test]
    fn user_message_maps_to_stats_entry() {
        let text = "Reply with exactly this single token and nothing else: SANITY-OK-REPLY";
        let line = r#"{"type":"message","id":"938cb619","parentId":"50a7a513","timestamp":"2026-10-11T12:42:05.554Z","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly this single token and nothing else: SANITY-OK-REPLY"}],"timestamp":1791722525551}}"#;
        assert_eq!(
            parse_entry(line),
            vec![TimelineEntry::Message {
                id: "938cb619".to_string(),
                role: MessageRole::User,
                ts: "2026-10-11T12:42:05.554Z".to_string(),
                chars: text.chars().count(),
                blocks: 1,
            }]
        );
    }

    #[test]
    fn assistant_text_message_maps_to_stats_entry() {
        let line = r#"{"type":"message","id":"40052345","parentId":"938cb619","timestamp":"2026-10-11T12:42:07.828Z","message":{"role":"assistant","content":[{"type":"text","text":"SANITY-OK-REPLY","textSignature":"{\"v\":1,\"id\":\"msg_01a0cbb6de49d3ae016acb841e645c87d2ab3ba78566c45f55\",\"phase\":\"final_answer\"}"}],"api":"openai-codex-responses","provider":"openai-codex","model":"gpt-6.1-sol","usage":{"input":1264,"output":11,"totalTokens":1275},"stopReason":"stop","timestamp":1791722525569,"responseId":"resp_01a0cbb6de49d3ae016acb841d0cf087d28cc6dca23961c23d","durationMs":2259,"thinkingLevel":"medium"}}"#;
        assert_eq!(
            parse_entry(line),
            vec![TimelineEntry::Message {
                id: "40052345".to_string(),
                role: MessageRole::Assistant,
                ts: "2026-10-11T12:42:07.828Z".to_string(),
                chars: "SANITY-OK-REPLY".chars().count(),
                blocks: 1,
            }]
        );
    }

    #[test]
    fn tool_calls_carry_name_and_args_length_only() {
        // bash: only the name and the JSON length of the arguments.
        let bash = r#"{"type":"message","id":"d00eef39","parentId":"5e60f988","timestamp":"2026-10-11T12:44:03.682Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_7aznF5Jwd9h6McZuec8JDYgI","name":"bash","arguments":{"command":"env | grep ^PI_SESSION | sort","timeout":null}}],"timestamp":1791722643682}}"#;
        let bash_args =
            serde_json::json!({"command":"env | grep ^PI_SESSION | sort","timeout":null});
        assert_eq!(
            parse_entry(bash),
            vec![TimelineEntry::ToolCall {
                id: "call_7aznF5Jwd9h6McZuec8JDYgI".to_string(),
                name: "bash".to_string(),
                ts: "2026-10-11T12:44:03.682Z".to_string(),
                tool_call_id: "call_7aznF5Jwd9h6McZuec8JDYgI".to_string(),
                args_chars: json_chars(&bash_args),
            }]
        );
        // read: same shape, different arguments.
        let read = r#"{"type":"message","id":"39f7ff0c","parentId":"7bdb37f0","timestamp":"2026-10-11T12:45:24.392Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_oLi9s59","name":"read","arguments":{"path":"spool/spike1-icon.png","offset":null,"limit":null}}],"timestamp":1791722724392}}"#;
        let read_args =
            serde_json::json!({"path":"spool/spike1-icon.png","offset":null,"limit":null});
        assert!(matches!(
            &parse_entry(read)[0],
            TimelineEntry::ToolCall { id, name, args_chars, .. }
                if id == "call_oLi9s59"
                    && name == "read"
                    && *args_chars == json_chars(&read_args)
        ));
        // No arguments at all (e.g. an MCP ping): "{}" is 2 chars.
        let ping = r#"{"type":"message","id":"aa","parentId":null,"timestamp":"2026-10-11T12:44:09.100Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_yKvPON9","name":"mcp__spike1_tools__spike1_ping","arguments":{}}],"timestamp":1}}"#;
        assert!(matches!(
            &parse_entry(ping)[0],
            TimelineEntry::ToolCall { args_chars, .. } if *args_chars == 2
        ));
    }

    #[test]
    fn one_assistant_line_with_two_tool_calls_yields_both_entries() {
        // Real shape from the spike sample (two reads in one message):
        // one message stat covering all three blocks + one entry per call.
        let line = r#"{"type":"message","id":"abc12345","parentId":"x","timestamp":"2026-10-11T12:45:24.400Z","message":{"role":"assistant","content":[{"type":"text","text":"Reading both files."},{"type":"toolCall","id":"call_A","name":"read","arguments":{"path":"a.png","offset":null,"limit":null}},{"type":"toolCall","id":"call_B","name":"read","arguments":{"path":"b.svg","offset":null,"limit":null}}],"timestamp":2}}"#;
        let entries = parse_entry(line);
        assert_eq!(entries.len(), 3, "text + two tool calls: {entries:?}");
        assert_eq!(
            entries[0],
            TimelineEntry::Message {
                id: "abc12345".to_string(),
                role: MessageRole::Assistant,
                ts: "2026-10-11T12:45:24.400Z".to_string(),
                chars: "Reading both files.".chars().count(),
                blocks: 3,
            }
        );
        assert!(matches!(
            &entries[1],
            TimelineEntry::ToolCall { id, name, tool_call_id, .. }
                if id == "call_A" && tool_call_id == "call_A" && name == "read"
        ));
        assert!(matches!(
            &entries[2],
            TimelineEntry::ToolCall { id, name, tool_call_id, .. }
                if id == "call_B" && tool_call_id == "call_B" && name == "read"
        ));
    }

    #[test]
    fn tool_results_carry_correlation_and_duration() {
        let ok = r#"{"type":"message","id":"6d9371f9","parentId":"d00eef39","timestamp":"2026-10-11T12:44:06.084Z","message":{"role":"toolResult","toolCallId":"call_7aznF5Jwd9h6McZuec8JDYgI|fc_01a0cbb6de49d3ae016acb84944c1c87d28cc6dca23961c23d","toolName":"bash","content":[{"type":"text","text":"PI_SESSION_FILE=C:\\Repos\\MyIDE\\spikes\\spike1-host\\.pi-run\\sessions\\2026-10-11T12-40-59-177Z_01a12afb-1028-72e9-b27e-b581003005aa.jsonl\nPI_SESSION_ID=01a12afb-1028-72e9-b27e-b581003005aa\n"}],"isError":false,"durationMs":92,"timestamp":1791722646083}}"#;
        // The result's own correlation pair + outcome + duration.
        assert_eq!(
            parse_entry(ok),
            vec![TimelineEntry::ToolResult {
                id: "6d9371f9".to_string(),
                ts: "2026-10-11T12:44:06.084Z".to_string(),
                tool_call_id: "call_7aznF5Jwd9h6McZuec8JDYgI|fc_01a0cbb6de49d3ae016acb84944c1c87d28cc6dca23961c23d".to_string(),
                tool_name: "bash".to_string(),
                ok: true,
                duration_ms: 92,
            }]
        );
        // An error result without durationMs: ok=false, duration 0.
        let error = r#"{"type":"message","id":"ef4049c3","parentId":"d5b98281","timestamp":"2026-10-11T12:44:17.133Z","message":{"role":"toolResult","toolCallId":"call_5BBgDMdeDXYFuAwhMSByzBun|fc_01a0cbb6de49d3ae016acb849f871087d2b4ef18e5cdafe0b4","toolName":"bash","content":[{"type":"text","text":"[spike1-gate] blocked by spike1 test rule: command contains 'spike1-deny-me'"}],"details":{},"isError":true,"timestamp":1791722657133}}"#;
        assert_eq!(
            parse_entry(error),
            vec![TimelineEntry::ToolResult {
                id: "ef4049c3".to_string(),
                ts: "2026-10-11T12:44:17.133Z".to_string(),
                tool_call_id: "call_5BBgDMdeDXYFuAwhMSByzBun|fc_01a0cbb6de49d3ae016acb849f871087d2b4ef18e5cdafe0b4".to_string(),
                tool_name: "bash".to_string(),
                ok: false,
                duration_ms: 0,
            }]
        );
    }

    #[test]
    fn a_tool_result_row_points_back_at_its_call() {
        // P2-2: two tool calls in one line, a result for the second one.
        // pi's toolCallId is the call id suffixed with the provider id,
        // so the pairing is by prefix — and tool_name must agree with
        // the paired call's name.
        let line = r#"{"type":"message","id":"abc","parentId":null,"timestamp":"2026-10-11T12:45:24.400Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_A","name":"bash","arguments":{"command":"ls"}},{"type":"toolCall","id":"call_B","name":"read","arguments":{"path":"x"}}],"timestamp":2}}"#;
        let result = r#"{"type":"message","id":"res1","parentId":"abc","timestamp":"2026-10-11T12:45:25.000Z","message":{"role":"toolResult","toolCallId":"call_B|fc_01","toolName":"read","content":[{"type":"text","text":"bytes"}],"isError":false,"durationMs":5,"timestamp":3}}"#;
        let calls: Vec<_> = parse_entry(line)
            .into_iter()
            .filter_map(|entry| match entry {
                TimelineEntry::ToolCall { id, name, .. } => Some((id, name)),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        let TimelineEntry::ToolResult {
            tool_call_id,
            tool_name,
            ..
        } = &parse_entry(result)[0]
        else {
            panic!("expected a tool result");
        };
        let paired: Vec<_> = calls
            .iter()
            .filter(|(id, _)| tool_call_id.starts_with(id.as_str()))
            .collect();
        assert_eq!(paired.len(), 1, "the pairing is unambiguous");
        assert_eq!(paired[0].1, "read");
        assert_eq!(tool_name, "read", "result names the paired tool");
    }

    #[test]
    fn system_message_is_not_observed() {
        // The system preamble carries the whole prompt (huge, and nothing
        // the operator needs in a timeline).
        let line = r#"{"type":"message","id":"ee9db85c","parentId":"2dfd301d","timestamp":"2026-10-11T12:37:34.457Z","message":{"role":"system","content":"","sections":{"preamble":"You are an expert coding assistant…"}}}"#;
        assert!(parse_entry(line).is_empty());
    }

    #[test]
    fn mangled_lines_and_unknown_shapes_are_skipped() {
        assert!(parse_entry("not json at all").is_empty());
        assert!(
            parse_entry("{\"type\":\"message\"}").is_empty(),
            "no message"
        );
        assert!(
            parse_entry(r#"{"type":"future_entry","id":"x","timestamp":"t"}"#).is_empty(),
            "unknown entry type"
        );
        assert!(
            parse_entry(
                r#"{"type":"message","id":"x","timestamp":"t","message":{"role":"hookAgent","content":[{"type":"text","text":"hi"}]}}"#
            )
            .is_empty(),
            "unknown role"
        );
    }

    #[test]
    fn wire_entries_carry_stats_only_never_content() {
        // The structural privacy boundary (P1-3): credentials in the
        // message text *and* the tool arguments must reduce to stats on
        // the wire — nothing else. No sanitizer needed: the data is
        // never extracted.
        let secret = "Authorization: Bearer xyz";
        let text = format!("the password is hunter2 and the header is {secret}");
        let line = format!(
            r#"{{"type":"message","id":"1","timestamp":"t","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        );
        let message = serde_json::to_value(&parse_entry(&line)[0]).unwrap();
        let wire = message.to_string();
        assert!(!wire.contains("hunter2"), "no message text: {wire}");
        assert!(!wire.contains("Authorization"), "no headers: {wire}");
        let mut keys: Vec<&str> = message
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["blocks", "chars", "id", "kind", "role", "ts"]);

        let tool = format!(
            r#"{{"type":"message","id":"2","timestamp":"t","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"call_s","name":"bash","arguments":{{"command":"curl -H '{secret}' https://api"}}}}]}}}}"#
        );
        let entries = parse_entry(&tool);
        let TimelineEntry::ToolCall { args_chars, .. } = &entries[0] else {
            panic!("expected a tool call: {entries:?}");
        };
        assert_eq!(
            *args_chars,
            "{\"command\":\"curl -H 'Authorization: Bearer xyz' https://api\"}"
                .chars()
                .count()
        );
        let tool_wire = serde_json::to_string(&entries[0]).unwrap();
        assert!(!tool_wire.contains("Authorization"), "no args: {tool_wire}");
        assert!(!tool_wire.contains("curl"), "no command: {tool_wire}");
        let tool_value = serde_json::to_value(&entries[0]).unwrap();
        let mut tool_keys: Vec<&str> = tool_value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        tool_keys.sort_unstable();
        assert_eq!(
            tool_keys,
            vec!["args_chars", "id", "kind", "name", "tool_call_id", "ts"]
        );

        let result =
            r#"{"type":"message","id":"3","timestamp":"t","message":{"role":"toolResult","toolCallId":"call_s|fc_1","toolName":"bash","content":[{"type":"text","text":"output with hunter2 inside"}],"isError":false,"durationMs":7}}"#.to_string();
        let result_wire = serde_json::to_string(&parse_entry(&result)[0]).unwrap();
        assert!(
            !result_wire.contains("hunter2"),
            "no result text: {result_wire}"
        );
        let result_value = serde_json::to_value(&parse_entry(&result)[0]).unwrap();
        let mut result_keys: Vec<&str> = result_value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        result_keys.sort_unstable();
        assert_eq!(
            result_keys,
            vec![
                "duration_ms",
                "id",
                "kind",
                "ok",
                "tool_call_id",
                "tool_name",
                "ts"
            ]
        );
    }

    // ---------------------------------------------------------------
    // Discovery
    // ---------------------------------------------------------------

    fn write_file(path: &Path, contents: &str, mtime: SystemTime) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
        let file = File::options().write(true).open(path).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_accessed(mtime)
                .set_modified(mtime),
        )
        .unwrap();
    }

    fn sandbox_root(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tiller-tail-test-{}-{}-{tag}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A one-block user message line, the shortest observable entry.
    fn user_line(id: &str, text: &str) -> String {
        format!(
            r#"{{"type":"message","id":"{id}","timestamp":"t{id}","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    #[test]
    fn discovery_finds_newest_jsonl_across_layouts() {
        let root = sandbox_root("discover");
        // Flat (older pi) and slug-subdir (pi 1.1.0) layouts side by side;
        // newest mtime wins regardless of depth.
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        write_file(&root.join("sessions").join("old.jsonl"), "{}", base);
        write_file(
            &root
                .join("sessions")
                .join("--C--repo--")
                .join("newest.jsonl"),
            "{}",
            base + Duration::from_secs(10),
        );
        write_file(
            &root
                .join("sessions")
                .join("--C--repo--")
                .join("middle.jsonl"),
            "{}",
            base + Duration::from_secs(5),
        );
        // Not a session file: ignored.
        write_file(&root.join("sessions").join("notes.txt"), "", base);
        assert_eq!(
            discover_session_file(&root),
            Some(
                root.join("sessions")
                    .join("--C--repo--")
                    .join("newest.jsonl")
            )
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discovery_without_session_file_waits_as_none() {
        let root = sandbox_root("empty");
        assert_eq!(discover_session_file(&root), None);
        fs::create_dir_all(root.join("sessions").join("--slug--")).unwrap();
        assert_eq!(
            discover_session_file(&root),
            None,
            "empty slug dir (pi booted, no message yet) is still no session"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn snapshot_parses_whole_file_and_tolerates_no_file() {
        let root = sandbox_root("snapshot");
        assert!(snapshot(&root).is_empty(), "no sandbox at all");
        write_file(
            &root.join("sessions").join("s.jsonl"),
            concat!(
                r#"{"type":"session","version":3,"id":"u","timestamp":"t"}"#,
                "\n",
                r#"{"type":"message","id":"938cb619","timestamp":"2026-10-11T12:42:05.554Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
                "\n",
                "this line is mangled", // skipped, not fatal
                "\n",
            ),
            SystemTime::now(),
        );
        assert_eq!(
            snapshot(&root),
            vec![TimelineEntry::Message {
                id: "938cb619".to_string(),
                role: MessageRole::User,
                ts: "2026-10-11T12:42:05.554Z".to_string(),
                chars: "hello".chars().count(),
                blocks: 1,
            }]
        );
        fs::remove_dir_all(&root).ok();
    }

    // ---------------------------------------------------------------
    // Tailing
    // ---------------------------------------------------------------

    #[test]
    fn poll_once_emits_entries_in_order_and_handles_partial_lines() {
        let root = sandbox_root("poll");
        let sink = RecordingSink::new();
        let mut tailer = TailerState::new(root.clone());

        // No file yet: a quiet poll is a no-op, not a panic.
        assert_eq!(tailer.poll_once(7, sink.as_ref()), 0);

        let session_file = root.join("sessions").join("--slug--").join("s.jsonl");
        write_file(&session_file, "", SystemTime::now());

        // A half-written line (no newline yet) must not emit.
        let user_line = r#"{"type":"message","id":"1","timestamp":"t1","message":{"role":"user","content":[{"type":"text","text":"first"}]}}"#;
        {
            let mut file = File::options().append(true).open(&session_file).unwrap();
            use std::io::Write;
            write!(file, "{user_line}").unwrap();
        }
        assert_eq!(tailer.poll_once(7, sink.as_ref()), 0, "partial: no emit");

        // Completed by the newline: one entry, correct payload shape.
        {
            let mut file = File::options().append(true).open(&session_file).unwrap();
            use std::io::Write;
            writeln!(file).unwrap();
            writeln!(
                file,
                r#"{{"type":"message","id":"2","timestamp":"t2","message":{{"role":"assistant","content":[{{"type":"text","text":"second"}}]}}}}"#
            )
            .unwrap();
        }
        assert_eq!(tailer.poll_once(7, sink.as_ref()), 2);
        {
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 2, "{events:?}");
            assert_eq!(events[0].0, EVENT_ENTRY);
            assert_eq!(events[0].1["session_id"], 7);
            assert_eq!(events[0].1["entry"]["kind"], "message");
            assert_eq!(events[0].1["entry"]["role"], "user");
            assert_eq!(events[0].1["entry"]["chars"], "first".chars().count());
            assert_eq!(events[1].1["entry"]["id"], "2");
            assert_eq!(events[1].1["entry"]["role"], "assistant");
        }

        // Nothing new: quiet poll emits nothing.
        assert_eq!(tailer.poll_once(7, sink.as_ref()), 0);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn poll_once_survives_file_removal_and_switches_to_newer_file() {
        let root = sandbox_root("switch");
        let sink = RecordingSink::new();
        let mut tailer = TailerState::new(root.clone());

        let first = root.join("sessions").join("a.jsonl");
        write_file(
            &first,
            &format!(
                "{}\n",
                r#"{"type":"message","id":"1","timestamp":"t1","message":{"role":"user","content":[{"type":"text","text":"old file"}]}}"#
            ),
            SystemTime::now(),
        );
        assert_eq!(tailer.poll_once(1, sink.as_ref()), 1);

        // File vanishes under the tailer: no panic, no emit; and a new
        // session file (a `/new` inside pi) is picked up from offset 0.
        fs::remove_file(&first).unwrap();
        assert_eq!(tailer.poll_once(1, sink.as_ref()), 0);
        let second = root.join("sessions").join("--slug--").join("b.jsonl");
        write_file(
            &second,
            &format!(
                "{}\n{}\n",
                r#"{"type":"message","id":"2","timestamp":"t2","message":{"role":"user","content":[{"type":"text","text":"new file"}]}}"#,
                r#"{"type":"message","id":"3","timestamp":"t3","message":{"role":"user","content":[{"type":"text","text":"new file 2"}]}}"#
            ),
            SystemTime::now(),
        );
        assert_eq!(tailer.poll_once(1, sink.as_ref()), 2, "fresh file from 0");
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(events[1].1["entry"]["chars"], "new file".chars().count());
        assert_eq!(events[2].1["entry"]["chars"], "new file 2".chars().count());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_multibyte_char_split_across_reads_decodes_once_complete() {
        // P2-1: the first write ends mid-'ż' (inside its 2-byte UTF-8
        // sequence), the second completes it and the newline. The line
        // must decode exactly once complete — the `chars` stat of the
        // emitted entry proves it (a lossy per-read decode would turn
        // 'ż' into two U+FFFD and inflate the count).
        let root = sandbox_root("utf8-split");
        let sink = RecordingSink::new();
        let mut tailer = TailerState::new(root.clone());
        let session_file = root.join("sessions").join("s.jsonl");
        write_file(&session_file, "", SystemTime::now());

        let text = "żółw"; // 4 chars, 7 UTF-8 bytes
        let line = user_line("1", text);
        let bytes = line.as_bytes();
        let cut = line.find('ż').unwrap() + 1; // split inside 'ż'
        {
            use std::io::Write;
            let mut file = File::options().append(true).open(&session_file).unwrap();
            file.write_all(&bytes[..cut]).unwrap();
        }
        assert_eq!(
            tailer.poll_once(1, sink.as_ref()),
            0,
            "partial char: no emit"
        );

        {
            use std::io::Write;
            let mut file = File::options().append(true).open(&session_file).unwrap();
            file.write_all(&bytes[cut..]).unwrap();
            writeln!(file).unwrap();
        }
        assert_eq!(tailer.poll_once(1, sink.as_ref()), 1);
        {
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1, "{events:?}");
            assert_eq!(
                events[0].1["entry"]["chars"],
                text.chars().count(),
                "the multibyte char decoded whole: {events:?}"
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_recreated_session_file_is_read_from_zero() {
        // P1-2: read a file, delete it, recreate it at the SAME path —
        // shorter, then longer than the old offset. Everything must be
        // re-read from offset 0 (the stale offset would read a longer
        // file from the middle and skip a same-length file entirely).
        let root = sandbox_root("recreate");
        let sink = RecordingSink::new();
        let mut tailer = TailerState::new(root.clone());
        let session_file = root.join("sessions").join("s.jsonl");

        let first = user_line("1", "a considerably long first line of session text");
        write_file(&session_file, &format!("{first}\n"), SystemTime::now());
        assert_eq!(tailer.poll_once(1, sink.as_ref()), 1);

        // The file vanishes: quiet poll, and tracking must reset so a
        // file recreated under the same path starts from scratch.
        fs::remove_file(&session_file).unwrap();
        assert_eq!(tailer.poll_once(1, sink.as_ref()), 0);

        // Recreated shorter than the old offset: still read from zero.
        let shorter = user_line("2", "hi");
        write_file(&session_file, &format!("{shorter}\n"), SystemTime::now());
        assert_eq!(
            tailer.poll_once(1, sink.as_ref()),
            1,
            "a shorter recreation is a fresh file, not a shrink error"
        );

        // Recreated longer than the old offset: from zero as well, not
        // from the middle.
        fs::remove_file(&session_file).unwrap();
        assert_eq!(tailer.poll_once(1, sink.as_ref()), 0);
        let longer = user_line(
            "3",
            "a much longer replacement line that clearly exceeds the previous file length",
        );
        write_file(&session_file, &format!("{longer}\n"), SystemTime::now());
        assert_eq!(
            tailer.poll_once(1, sink.as_ref()),
            1,
            "a longer recreation is read from zero, not from the stale offset"
        );
        {
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 3, "{events:?}");
            assert_eq!(events[2].1["entry"]["id"], "3");
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn run_tailer_streams_until_stopped() {
        // The thread loop itself: entries flow live, the thread ends soon
        // after the stop flag (the actor's join must not stall).
        let root = sandbox_root("thread");
        let sink = RecordingSink::new();
        let session_file = root.join("sessions").join("s.jsonl");
        write_file(
            &session_file,
            &format!(
                "{}\n",
                r#"{"type":"message","id":"1","timestamp":"t1","message":{"role":"user","content":[{"type":"text","text":"live"}]}}"#
            ),
            SystemTime::now(),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let tailer_sink = sink.clone();
        let tailer_root = root.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let (stop_flag, handle) = {
            let stop_for_thread = Arc::clone(&stop);
            let handle = std::thread::Builder::new()
                .name("test-tailer".to_string())
                .spawn(move || {
                    run_tailer(tailer_sink, 3, tailer_root, stop_for_thread);
                    done_tx.send(()).expect("test alive");
                })
                .expect("spawn test tailer");
            (stop, handle)
        };

        sink.wait_for(Duration::from_secs(2), |event, _| event == EVENT_ENTRY);
        // Late append after the first poll: still streamed (the loop keeps
        // polling, not just the first read).
        {
            use std::io::Write;
            let mut file = File::options().append(true).open(&session_file).unwrap();
            writeln!(
                file,
                r#"{{"type":"message","id":"2","timestamp":"t2","message":{{"role":"assistant","content":[{{"type":"text","text":"late"}}]}}}}"#
            )
            .unwrap();
        }
        sink.wait_for(Duration::from_secs(2), |event, payload| {
            event == EVENT_ENTRY && payload["entry"]["id"] == "2"
        });

        stop_flag.store(true, Ordering::Relaxed);
        let started = Instant::now();
        handle.join().expect("tailer thread ended cleanly");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("thread body completed");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "tailer stops within ~one poll after the flag"
        );
        {
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 2, "no duplicates, in order: {events:?}");
        }
        fs::remove_dir_all(&root).ok();
    }
}

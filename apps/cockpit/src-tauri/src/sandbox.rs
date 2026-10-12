//! pi agent-home sandbox (Spike 1 pattern, ported for the cockpit host).
//!
//! When the cockpit hosts `pi`, pi's persistent state must not land in the
//! user's global agent dir: the spike solved project-trust/MCP approvals
//! for a *process* (not a user) by pointing `PI_CODING_AGENT_DIR` at a
//! throwaway home seeded with the minimum pi needs to boot. This is state
//! isolation, not an access barrier — pi still runs with the host user's
//! normal file access.
//!
//! Seeded per session under `{sandbox_root}/agent-home/{session_id}-{random}/`
//! (fresh, unpredictable path — see [`prepare_pi_sandbox`]) and deleted
//! when the session ends (see [`cleanup_dir`]; leftovers from crashed runs
//! are swept at manager start, see [`sweep_agent_homes`]):
//!
//! - `settings.json` — minimal (no prompts on boot),
//! - `trust.json` — the requested cwd pre-trusted so pi does not block on
//!   an interactive approval,
//! - `auth.json`, `models.json`, `models-store.json` — copied verbatim
//!   from the real agent home when present (`auth.json` is a *secrets*
//!   file: it is copied, never logged, never printed),
//! - `bin/rg.exe` + `bin/fd.exe` — copied from the Spike 1 seed
//!   (`spikes/spike1-host/.pi-run/agent-home/bin`) when the repo checkout
//!   is reachable from the app's working directory.
//!
//! Which of these pi actually needs is *measured* (pi 1.1.0, via the
//! smoke example with `PI_SMOKE_AGENT_DIR` pointing at hand-built homes):
//! booting into the chat TUI requires **nothing** — an empty
//! `PI_CODING_AGENT_DIR` renders the full TUI (title, help bar, context)
//! in ~40 ms with the DSR probe answered, with no trust prompt and no
//! onboarding wizard. The seeded files are therefore not boot
//! requirements but session-quality ones: `settings.json` suppresses the
//! one-time changelog screen, `trust.json` pre-approves the cwd (pi does
//! not block at boot on an untrusted cwd), `auth.json`/`models*.json`
//! carry the credentials/models the chat needs to be *usable*, and
//! `bin/` keeps pi's built-in grep/find working offline.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::term_host::SessionId;

/// The pi file names we sandbox (by heuristic: file name of the program).
const PI_FILE_NAMES: [&str; 3] = ["pi", "pi.cmd", "pi.exe"];

/// Minimal settings: just silence the changelog screen. The pinned version
/// mirrors the operator's real `lastChangelogVersion`; a mismatch at worst
/// shows the changelog once.
const MINIMAL_SETTINGS: &str = "{\"lastChangelogVersion\":\"1.1.0\"}";

/// Secret/model files seeded from the real agent home (verbatim copies).
const SEEDED_FILES: [&str; 3] = ["auth.json", "models.json", "models-store.json"];

/// Tools the sandbox ships so pi's built-in tool calls work offline.
const SEEDED_BINS: [&str; 2] = ["rg.exe", "fd.exe"];

/// Does this program look like pi (file name heuristic, case-insensitive)?
pub fn is_pi_program(program: &str) -> bool {
    let file_name = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    let lower = file_name.to_ascii_lowercase();
    PI_FILE_NAMES.contains(&lower.as_str())
}

/// Resolve the program to run for a pi spawn: PATH lookup plus the `.cmd`
/// shim translation (Spike 1 pitfall 2 — CreateProcess cannot run npm's
/// `pi.cmd` shims, so they are translated to `node <cli.js>`).
pub fn resolve_pi_program(program: &str) -> (String, Vec<String>) {
    resolve_pi_program_in(&std::env::var("PATH").unwrap_or_default(), program)
}

fn resolve_pi_program_in(path_var: &str, program: &str) -> (String, Vec<String>) {
    let bare = !program.contains('/') && !program.contains('\\');
    let resolved = if bare {
        which_in(path_var, program)
    } else {
        PathBuf::from(program).canonicalize().ok().map(plain_path)
    };
    let Some(found) = resolved else {
        // Not on PATH: hand the program through and let the spawn fail with
        // the platform's own error — clearer than guessing here.
        return (program.to_string(), Vec::new());
    };
    if found.extension().and_then(|e| e.to_str()) == Some("cmd") {
        if let Some(cli) = cli_js_next_to(&found) {
            return ("node".to_string(), vec![cli]);
        }
    }
    (found.to_string_lossy().into_owned(), Vec::new())
}

/// Strip the `\\?\` verbatim prefix `canonicalize` adds on Windows: the
/// child (node) should see an ordinary drive path — verbatim paths skip
/// the Win32 path normalization later layers (spawn, node fs) can choke
/// on, and bare-name resolution never produces them.
fn plain_path(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    let stripped = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        return path;
    };
    PathBuf::from(stripped)
}

/// The npm CLI bundle a `pi.cmd` shim lives next to (Spike 1 convention).
fn cli_js_next_to(shim: &Path) -> Option<String> {
    let cli = shim
        .parent()?
        .join("node_modules")
        .join("@earendil-works")
        .join("pi-coding-agent")
        .join("dist")
        .join("bundle")
        .join("cli.js");
    cli.is_file().then(|| cli.to_string_lossy().into_owned())
}

/// Tiny which(1) over an explicit PATH (testable without env mutation).
fn which_in(path_var: &str, name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["exe", "cmd", "bat", ""]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(path_var) {
        for ext in exts {
            let cand = if ext.is_empty() {
                dir.join(name)
            } else {
                dir.join(format!("{name}.{ext}"))
            };
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Prepare the sandbox home for `session_id`; returns the directory (the
/// value the caller sets `PI_CODING_AGENT_DIR` to).
///
/// The directory name carries a random tag: session ids restart at 1 in
/// every app run, so a bare `{session_id}` path would be predictable — and
/// a predictable path hands any local process the location of the live
/// `auth.json` copy (naming hygiene, not a security boundary).
pub fn prepare_pi_sandbox(
    root: &Path,
    session_id: SessionId,
    cwd: Option<&Path>,
) -> io::Result<PathBuf> {
    prepare_with_home(root, session_id, cwd, real_agent_home())
}

/// Testable core of [`prepare_pi_sandbox`]: the home to seed from is
/// injected, because the real one is read from process env (not
/// isolatable in tests).
pub(crate) fn prepare_with_home(
    root: &Path,
    session_id: SessionId,
    cwd: Option<&Path>,
    home: Option<PathBuf>,
) -> io::Result<PathBuf> {
    let base = root.join("agent-home");
    fs::create_dir_all(&base)?;
    let dir = fresh_session_dir(&base, session_id)?;
    // Seed inside a closure: a failure after the dir exists must not
    // leave the half-prepared (potentially secret-bearing) dir behind.
    let seeded = (|| -> io::Result<()> {
        fs::write(dir.join("settings.json"), MINIMAL_SETTINGS)?;
        fs::write(dir.join("trust.json"), trust_json(cwd))?;

        if let Some(home) = home {
            for name in SEEDED_FILES {
                let source = home.join(name);
                if source.is_file() {
                    // Verbatim copy, including auth.json (secrets: copy only,
                    // never log). A failing copy must not block the spawn —
                    // pi decides what is actually fatal.
                    let _ = fs::copy(&source, dir.join(name));
                }
            }
        }

        if let Some(seed_bin) = find_seed_bin_dir() {
            let bin_dir = dir.join("bin");
            for name in SEEDED_BINS {
                let source = seed_bin.join(name);
                if source.is_file() {
                    fs::create_dir_all(&bin_dir)?;
                    let _ = fs::copy(&source, bin_dir.join(name));
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = seeded {
        cleanup_dir(&dir);
        return Err(error);
    }
    Ok(dir)
}

/// A fresh, never-reused session directory: `{session_id}-{random tag}`.
/// `create_dir` (not `_all`) fails on collision, so races with another
/// preparer cannot silently share a directory.
fn fresh_session_dir(base: &Path, session_id: SessionId) -> io::Result<PathBuf> {
    let mut collision: Option<io::Error> = None;
    for _ in 0..8 {
        let candidate = base.join(format!("{session_id}-{}", random_tag()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                collision = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(collision
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "sandbox dir collision")))
}

/// Short unpredictable tag for sandbox dir names: `RandomState` hash keys
/// are per-process random, mixed with a timestamp — enough unpredictability
/// for temp-dir naming without a new dependency.
fn random_tag() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(std::process::id() as u64);
    hasher.write_u64(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    format!("{:016x}", hasher.finish())
}

/// Best-effort recursive delete of a finished session's sandbox — the
/// `auth.json` copy must not outlive its session. Deliberately
/// best-effort: on Windows the just-reaped child can hold file locks for
/// a moment, and sandbox removal must never panic the session actor;
/// leftovers are removed by [`sweep_agent_homes`] at the next start.
pub fn cleanup_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// Remove every agent-home from previous runs. Called at manager
/// construction, when this manager owns the root and has no live sessions
/// yet — so clearing the whole `agent-home` subtree is simpler and safer
/// than tracking liveness: a crashed run's sandbox (with its `auth.json`
/// copy and its reused-from-1 session ids) dies with the run.
pub fn sweep_agent_homes(root: &Path) {
    let _ = fs::remove_dir_all(root.join("agent-home"));
}

/// `trust.json` marking `cwd` as trusted. Keys are Windows-style paths
/// (pi compares against its own cwd, which the OS reports with `\`).
pub(crate) fn trust_json(cwd: Option<&Path>) -> String {
    let Some(cwd) = cwd else {
        return "{}".to_string();
    };
    let key = if cfg!(windows) {
        cwd.to_string_lossy().replace('/', "\\")
    } else {
        cwd.to_string_lossy().into_owned()
    };
    serde_json::json!({ key: true }).to_string()
}

/// The real agent home to seed from: `$USERPROFILE/.pi/agent` (this pi
/// layout) with a `$USERPROFILE/.pi` fallback.
fn real_agent_home() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    let agent = home.join(".pi").join("agent");
    if agent.is_dir() {
        return Some(agent);
    }
    let dot_pi = home.join(".pi");
    dot_pi.is_dir().then_some(dot_pi)
}

/// The Spike 1 seed binary dir, found by walking up from the working
/// directory (the app runs from the repo during development; a bundled
/// install simply does not find it and the copy is skipped).
fn find_seed_bin_dir() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    for _ in 0..8 {
        let cand = dir
            .join("spikes")
            .join("spike1-host")
            .join(".pi-run")
            .join("agent-home")
            .join("bin");
        if cand.join("rg.exe").is_file() {
            return Some(cand);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tiller-sandbox-test-{}-{unique}-{tag}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pi_file_name_heuristic() {
        for program in ["pi", "pi.cmd", "pi.exe", "PI.CMD", "C:/tools/pi.exe"] {
            assert!(is_pi_program(program), "{program} should look like pi");
        }
        for program in ["cmd.exe", "powershell", "ping", "piano", "spike"] {
            assert!(!is_pi_program(program), "{program} should not look like pi");
        }
    }

    /// `x\PI` is a Windows-style path: on Unix `Path::file_name` does not
    /// treat `\` as a separator, so this recognition is Windows-only —
    /// the ubuntu CI leg must not depend on it.
    #[cfg(windows)]
    #[test]
    fn pi_file_name_heuristic_windows_paths() {
        for program in ["x\\PI", "C:\\tools\\pi.exe"] {
            assert!(is_pi_program(program), "{program} should look like pi");
        }
    }

    #[test]
    fn trust_json_marks_cwd_trusted_with_platform_separators() {
        let json = trust_json(Some(Path::new("C:/Repos/tiller")));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // pi compares the key against its own cwd as the OS reports it:
        // backslashes on Windows, the path verbatim elsewhere.
        let key = if cfg!(windows) {
            "C:\\Repos\\tiller"
        } else {
            "C:/Repos/tiller"
        };
        assert_eq!(parsed[key], true);
        assert_eq!(trust_json(None), "{}");
    }

    #[test]
    fn bare_program_resolves_to_first_path_hit() {
        // A bare `pi` file resolves on every platform (the extension
        // probes are Windows-only extras; the empty extension always runs).
        let root = temp_dir("which");
        let miss = root.join("miss");
        let hit = root.join("hit");
        fs::create_dir_all(&miss).unwrap();
        fs::create_dir_all(&hit).unwrap();
        fs::write(hit.join("pi"), "").unwrap();
        let sep = if cfg!(windows) { ';' } else { ':' };
        let path_var = format!("{}{sep}{}", miss.to_string_lossy(), hit.to_string_lossy());

        let (program, args) = resolve_pi_program_in(&path_var, "pi");
        assert!(program.ends_with("pi"), "resolved to {program}");
        assert!(args.is_empty());
        fs::remove_dir_all(&root).ok();
    }

    // Windows-specific resolution: `.cmd` shim translation, `.exe`
    // extension probes, and `\\?\` verbatim-path stripping only engage
    // under the Windows implementation of `which_in`/`canonicalize`.
    #[cfg(windows)]
    #[test]
    fn cmd_shim_translates_to_node_cli() {
        let root = temp_dir("shim");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let shim = bin.join("pi.cmd");
        fs::write(&shim, "@echo off\r\n").unwrap();
        let cli = bin
            .join("node_modules")
            .join("@earendil-works")
            .join("pi-coding-agent")
            .join("dist")
            .join("bundle")
            .join("cli.js");
        fs::create_dir_all(cli.parent().unwrap()).unwrap();
        fs::write(&cli, "").unwrap();

        let (program, args) = resolve_pi_program_in(&bin.to_string_lossy(), "pi");
        assert_eq!(program, "node");
        assert_eq!(args, vec![cli.to_string_lossy().into_owned()]);
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(windows)]
    #[test]
    fn plain_executable_resolves_to_full_path() {
        let root = temp_dir("exe");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("pi.exe");
        fs::write(&exe, "").unwrap();

        let (program, args) = resolve_pi_program_in(&bin.to_string_lossy(), "pi");
        assert!(
            program.to_lowercase().ends_with("pi.exe"),
            "resolved to {program}"
        );
        assert!(args.is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(windows)]
    #[test]
    fn full_path_resolution_yields_plain_drive_path() {
        let root = temp_dir("full");
        let exe = root.join("pi.exe");
        fs::write(&exe, "").unwrap();

        // Forward slashes on purpose: canonicalize normalizes them — the
        // answer must be the plain backslash path, not `\\?\...`.
        let given = exe.to_string_lossy().replace('/', "\\");
        let (program, args) = resolve_pi_program_in("Z:\\definitely\\not\\here", &given);
        assert!(program.ends_with("pi.exe"), "resolved to {program}");
        assert!(!program.starts_with(r"\\?\"), "verbatim prefix: {program}");
        assert!(args.is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unresolved_program_passes_through() {
        let miss = if cfg!(windows) {
            "Z:\\definitely\\not\\here"
        } else {
            "/definitely/not/here"
        };
        let (program, args) = resolve_pi_program_in(miss, "pi");
        assert_eq!(program, "pi");
        assert!(args.is_empty());
    }

    #[test]
    fn sandbox_seeds_minimal_home_and_conditional_files() {
        let root = temp_dir("home");
        let real_home = root.join("real-home");
        fs::create_dir_all(&real_home).unwrap();
        fs::write(real_home.join("models.json"), "{}").unwrap();
        // auth.json deliberately absent from the source: seeding is
        // conditional and secrets must not be conjured from nowhere.

        let dir = prepare_with_home(
            &root,
            42,
            Some(Path::new("C:/Repos/tiller")),
            Some(real_home),
        )
        .unwrap();

        // Unpredictable per-session dir: agent-home/{id}-{random tag}.
        // (String check on the file name: `Path::starts_with` compares
        // whole components and `42-…` is a single component.)
        let rest = dir
            .strip_prefix(root.join("agent-home"))
            .expect("under agent-home");
        assert!(rest.to_string_lossy().starts_with("42-"), "dir {dir:?}");
        assert_ne!(rest.to_string_lossy(), "42");

        let settings = fs::read_to_string(dir.join("settings.json")).unwrap();
        assert!(settings.contains("lastChangelogVersion"));
        let trust = fs::read_to_string(dir.join("trust.json")).unwrap();
        let trusted_key = if cfg!(windows) {
            "C:\\\\Repos\\\\tiller"
        } else {
            "C:/Repos/tiller"
        };
        assert!(trust.contains(trusted_key), "trust: {trust}");

        assert!(dir.join("models.json").is_file(), "in source → seeded");
        assert!(!dir.join("auth.json").exists(), "not in source → skipped");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn restart_after_source_auth_removal_seeds_no_auth() {
        let root = temp_dir("restart");
        let real_home = root.join("real-home");
        fs::create_dir_all(&real_home).unwrap();
        fs::write(real_home.join("auth.json"), "{}").unwrap();

        let first = prepare_with_home(&root, 1, None, Some(real_home.clone())).unwrap();
        assert!(first.join("auth.json").is_file());

        // The user logs out / deletes the source auth.json, then restarts
        // the app: session ids restart at 1, so the second run must NOT
        // land in the first run's directory and inherit the stale copy.
        fs::remove_file(real_home.join("auth.json")).unwrap();
        let second = prepare_with_home(&root, 1, None, Some(real_home)).unwrap();
        assert_ne!(first, second, "fresh dir per session, ids do not collide");
        assert!(!second.join("auth.json").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cleanup_dir_removes_a_finished_sandbox() {
        let root = temp_dir("cleanup");
        let dir = prepare_with_home(&root, 7, None, None).unwrap();
        assert!(dir.join("settings.json").is_file());
        cleanup_dir(&dir);
        assert!(!dir.exists());
        // Best-effort by design: cleaning a missing dir is fine too.
        cleanup_dir(&dir);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn startup_sweep_removes_orphaned_agent_homes() {
        let root = temp_dir("sweep");
        let orphan = root.join("agent-home").join("1-deadbeefdeadbeef");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("auth.json"), "stale secrets").unwrap();

        sweep_agent_homes(&root);

        assert!(!orphan.exists(), "orphaned sandbox swept at startup");
        assert!(!root.join("agent-home").exists());
        fs::remove_dir_all(&root).ok();
    }
}

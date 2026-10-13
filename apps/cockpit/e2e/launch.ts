import { execFileSync, spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import * as net from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

/** Release build of the cockpit shell — real WebView2, embedded frontend.
 *  Dev mode (`tauri dev`) is deliberately not used: too slow for a smoke. */
const COCKPIT_EXE = fileURLToPath(new URL('../../../target/release/cockpit.exe', import.meta.url));

/** Launch harness for the Playwright smoke (see e2e/README.md). */
export interface CockpitProcess {
  pid: number;
  /** Browser-level CDP WebSocket endpoint of the WebView2 host — feed it
   *  straight into `chromium.connectOverCDP`. */
  wsEndpoint: string;
  /** Kill the exact process tree we spawned: `taskkill /PID <pid> /T /F`
   *  (shell-out), then remove the isolated WebView2 user-data folder.
   *  Idempotent, best-effort. NEVER kill by image name — that kills the
   *  operator's other cockpit sessions (recorded L1 incident). */
  kill: () => void;
}

/** The `/json/version` answer (only the field we consume). */
export interface CdpVersionInfo {
  webSocketDebuggerUrl: string;
}

/** Failure surfaced by `launchCockpit`: carries the isolated WebView2
 *  user-data folder of the failed attempt (removed before the error
 *  propagates) so tests can verify nothing was left behind. */
export interface LaunchFailure extends Error {
  userDataDir?: string;
}

/** Bind-check a random port in `[rangeStart, rangeEnd]` before handing
 *  it to WebView2: a purely random pick can land on an occupied port, and
 *  `waitForCdp` would then happily answer a *foreign* debugger. The
 *  probe server is released before the port is returned — the residual
 *  race window is closed by the identity assertion after connectOverCDP
 *  (see smoke.spec.ts). Rejects when the whole range is occupied. */
export async function pickFreeCdpPort(rangeStart: number, rangeEnd: number): Promise<number> {
  const span = rangeEnd - rangeStart + 1;
  const tried = new Set<number>();
  for (;;) {
    const port = rangeStart + Math.floor(Math.random() * span);
    if (!tried.has(port)) {
      tried.add(port);
      const probe = net.createServer();
      const inUse = await new Promise<boolean>((resolve) => {
        probe.once('error', () => resolve(true)); // EADDRINUSE etc.
        probe.listen(port, '127.0.0.1', () => resolve(false));
      });
      await new Promise<void>((resolve) => probe.close(() => resolve()));
      if (!inUse) {
        return port;
      }
    }
    if (tried.size >= span) {
      throw new Error(`no free CDP port in ${rangeStart}-${rangeEnd} after ${tried.size} bind checks`);
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

/** Synchronous sleep (Atomics.wait is legal on Node's main thread) —
 *  cleanup retries need real spacing, not a busy loop. */
function syncSleep(ms: number): void {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

/** Poll the CDP HTTP endpoint until WebView2's debugger answers.
 *  Resolves with the parsed `/json/version`; rejects after `timeoutMs`
 *  (message carries the last failure for diagnostics). */
export async function waitForCdp(port: number, timeoutMs: number): Promise<CdpVersionInfo> {
  const deadline = Date.now() + timeoutMs;
  let lastError = 'unknown error';
  for (;;) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/json/version`);
      if (response.ok) {
        return (await response.json()) as CdpVersionInfo;
      }
      lastError = `HTTP ${response.status}`;
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    if (Date.now() >= deadline) {
      throw new Error(`CDP on 127.0.0.1:${port} not ready after ${timeoutMs} ms (last: ${lastError})`);
    }
    await sleep(250);
  }
}

/** Spawn the release cockpit.exe with WebView2 remote debugging on
 *  `cdpPort` and wait for the debugger to answer. The WebView2 user-data
 *  folder is an isolated temp dir: a shared (default) folder would make
 *  our exe join the operator's already-running browser process, where
 *  the debugging-port argument is silently ignored.
 *
 *  `exe` (default: the release build) exists so unit tests can drive the
 *  failure paths. On ANY failure — a spawn that never started (missing
 *  exe: Node answers with an async 'error' event, which without a
 *  handler would crash the test process), an early exit, a CDP timeout —
 *  the spawned tree is killed and the user-data folder removed BEFORE
 *  the error propagates: a failed launch never leaves an orphan window. */
export async function launchCockpit(
  cdpPort: number,
  timeoutMs = 30_000,
  exe: string = COCKPIT_EXE,
): Promise<CockpitProcess> {
  let userDataDir: string | null = null;
  let pid: number | undefined;
  let cleaned = false;
  const cleanup = (): void => {
    if (cleaned) return;
    cleaned = true;
    if (pid !== undefined) {
      try {
        // /T = the whole tree (cockpit.exe + its pi PTY children), by PID only.
        execFileSync('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' });
      } catch {
        // already gone — kill is best-effort by design
      }
    }
    if (userDataDir !== null) {
      // WebView2's browser processes release their file handles a beat
      // after the tree dies — space the removal retries out, don't spin.
      for (let attempt = 0; attempt < 8; attempt++) {
        try {
          rmSync(userDataDir, { recursive: true, force: true });
          break;
        } catch {
          syncSleep(250);
        }
      }
    }
  };

  try {
    userDataDir = mkdtempSync(join(tmpdir(), 'tiller-e2e-wv2-'));
    const child = spawn(exe, [], {
      stdio: 'ignore',
      env: {
        ...process.env,
        // WebView2 loader environment variables: append browser switches
        // (CDP port) and isolate the browser process (see comment above).
        WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
        WEBVIEW2_USER_DATA_FOLDER: userDataDir,
      },
    });
    pid = child.pid;

    let settled = false;
    // A spawn that cannot start answers with an async 'error' event; a
    // missing exe (ENOENT) is translated to the operator-facing hint.
    const failed = new Promise<never>((_, reject) => {
      child.once('error', (error) => {
        if (settled) return;
        reject(
          error instanceof Error && (error as NodeJS.ErrnoException).code === 'ENOENT'
            ? new Error(`release build not found: ${exe} — build the release exe first (see e2e/README.md)`)
            : new Error(`spawning ${exe} failed: ${error instanceof Error ? error.message : String(error)}`),
        );
      });
    });
    const exited = new Promise<never>((_, reject) => {
      child.once('exit', (code) => {
        if (!settled) {
          reject(new Error(`cockpit.exe exited before CDP was ready (code ${code})`));
        }
      });
    });
    // After a successful launch the exit/error is expected (afterAll kills
    // it) — swallow the late rejections so they cannot surface unhandled.
    exited.catch(() => undefined);
    failed.catch(() => undefined);

    const info = await Promise.race([waitForCdp(cdpPort, timeoutMs), exited, failed]);
    settled = true;
    // Success implies the spawn answered CDP, so a PID exists.
    return { pid: pid as number, wsEndpoint: info.webSocketDebuggerUrl, kill: cleanup };
  } catch (error) {
    cleanup();
    (error as LaunchFailure).userDataDir = userDataDir ?? undefined;
    throw error;
  }
}

/** Fixture repository handle. */
export interface FixtureRepo {
  /** Absolute path of the repo root (native separators). */
  path: string;
  /** Remove the fixture directory recursively. Best-effort, idempotent. */
  cleanup: () => void;
}

/** Commit identity for fixture git commands — explicit environment, so
 *  the suite never depends on the machine's global git config
 *  (user.name, signing, …). */
const GIT_IDENTITY_ENV = {
  GIT_AUTHOR_NAME: 'tiller-e2e',
  GIT_AUTHOR_EMAIL: 'e2e@tiller.local',
  GIT_COMMITTER_NAME: 'tiller-e2e',
  GIT_COMMITTER_EMAIL: 'e2e@tiller.local',
} as const;

function git(cwd: string, args: string[]): string {
  return execFileSync('git', args, {
    cwd,
    encoding: 'utf8',
    env: { ...process.env, ...GIT_IDENTITY_ENV },
  });
}

/** Fresh single-commit repository on branch `main` in the system temp
 *  dir — the minimum a task worktree can be cut from. Uses the git CLI
 *  (available everywhere we develop) with an explicit identity env. */
export function makeFixtureRepo(): FixtureRepo {
  const dir = mkdtempSync(join(tmpdir(), 'tiller-e2e-repo-'));
  git(dir, ['init', '-b', 'main']);
  writeFileSync(join(dir, 'README.md'), '# tiller e2e fixture\n');
  git(dir, ['add', '.']);
  git(dir, ['-c', 'commit.gpgsign=false', 'commit', '-m', 'e2e fixture: initial commit']);
  return {
    path: dir,
    cleanup: () => rmSync(dir, { recursive: true, force: true }),
  };
}

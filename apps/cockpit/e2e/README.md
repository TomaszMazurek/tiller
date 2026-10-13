# E2E smoke (Playwright over CDP)

`npm run e2e` launches the **release** `cockpit.exe`, attaches Playwright
to its WebView2 over the Chrome DevTools Protocol, and drives the real
window end-to-end: WebView2 → Tauri bridge → rpc → core → git worktrees →
pi session in a PTY.

## Why

Some bug classes are only visible in the live window — ACL/capability
rejections in the Tauri bridge, WebView2-specific behavior, frozen React
component factories (dockview), PTY/xterm rendering. jsdom unit tests and
the Rust suites each miss a different half of those; this smoke covers
the whole stack in one pass.

## Requirements

- **Release build** of the cockpit (the suite refuses to start without it):

  ```bash
  npm run build                       # frontend → dist/ (embedded by cargo)
  cargo build --release -p cockpit    # → target/release/cockpit.exe
  ```

  Rebuild whenever frontend source under `src/` changes — the exe embeds
  `dist/` at compile time.
- **Edge/WebView2 runtime** (standard on Windows). No Playwright browser
  download is needed: the suite attaches to the app's own WebView2 via CDP.
- `git` on PATH (fixture repositories), `pi` on PATH (auto-started terminal).

## What the smoke does

`e2e/launch.ts` (plain-node helpers, unit-tested in `launch.test.ts`):

- `launchCockpit(cdpPort, timeoutMs, exe?)` — spawns the release exe
  with `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<port>`
  and an isolated `WEBVIEW2_USER_DATA_FOLDER` (so a running operator
  cockpit cannot swallow the debugging port). Returns `{pid, wsEndpoint,
  kill}`; `kill` is `taskkill /PID <pid> /T /F` — **never kill by image
  name**, that kills other cockpit sessions. Any failure — including a
  spawn that never started (missing exe answers as an async `'error'`
  event, routed into the rejection) — cleans up (kill tree, remove the
  user-data dir) **before** the error propagates.
- `pickFreeCdpPort(rangeStart, rangeEnd)` — bind-checks a random port in
  the range before it is handed to WebView2 (a purely random pick can
  land on an occupied port and the smoke would attach to a *foreign*
  debugger).
- `waitForCdp(port, timeoutMs)` — polls `http://127.0.0.1:<port>/json/version`.
- `makeFixtureRepo()` — temp repo (`main`, one commit) with an explicit
  identity env; no dependence on global git config.

`e2e/smoke.spec.ts` (bind-checked port in 9300-9399, single worker, no retries):

1. fixture repo → pick port → launch → connect → grab the window page →
   **identity assert**: the page title is `Tiller Cockpit` (closes the
   pick/spawn race — never drive a foreign window);
2. the "Terminal" dock tab is visible (window alive);
3. `status-version` shows `core v…` (UI → rpc → core);
4. Review: repo path → Load → task id → Create, then
   - queue shows the task,
   - `{repo}/.tiller/worktrees/smoke-1` exists on disk,
   - the P8a handoff auto-started pi (status `running`, Kill enabled),
   - hard: a `.xterm-rows` row with real (non-whitespace) text within
     20 s (rows existing alone proves nothing — the DOM renderer creates
     empty rows on panel open; a blank first screen gets an Enter nudge
     after 8 s);
5. cleanup always runs: UI discard (best-effort), disconnect, kill by PID,
   remove the fixture.

## Adding steps

Extend `smoke.spec.ts` (or add a new `*.spec.ts` in `e2e/`) and reuse the
`launch.ts` helpers. Prefer existing `data-testid`s over CSS/text
selectors; add a testid in `src/` when a seam has none, then **rebuild
the release exe** — the suite runs the embedded frontend, not `dist/` on
disk. Helper logic goes in `launch.ts` with a vitest unit test
(`npm test` picks up `launch.test.ts`; Playwright only runs `*.spec.ts`).

Artifacts (`test-results/`, `playwright-report/`) are gitignored.

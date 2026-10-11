# Tiller Cockpit

Tauri 2 + React + dockview shell for the Tiller Agent Edition (see
`PROJECT_BRIEF_v4.md` §4). This is the P1 skeleton: an empty cockpit layout
whose status bar proves the UI → `rpc` → `core` seam by displaying the core
version fetched over JSON-RPC.

## Prerequisites

- Rust (stable)
- Node.js ≥ 24.15.0 (tested; jsdom 30 requires ^22.22.2 || ^24.15.0 || >=26) with npm
- Tauri 2 platform prerequisites for your OS
  (Windows: WebView2; Linux: `libwebkit2gtk-4.1-dev` and friends;
  macOS: Xcode command line tools). See the [Tauri docs](https://tauri.app/start/prerequisites/).

## Run in dev mode

```bash
npm install
npm run tauri dev
```

`tauri dev` starts the Vite dev server (port 1420) and launches the desktop
window. The status bar at the bottom should show `core v0.1.0` — that value
comes from `tiller-core` through the `rpc` Tauri command (`core.version`).

## Other commands

- `npm test` — vitest suite (rpc client, status bar)
- `npm run build` — type-check (`tsc --noEmit`) + production bundle
- `npm run tauri build` — packaged desktop app (not used in CI)

## Layout (brief §4)

- **Terminal** — placeholder; P2 hosts Pi in a PTY here
- **Timeline** — placeholder; P4 tails the agent's JSONL session
- **Diff** — placeholder; P6 adds linear + semantic diff
- **Status bar** — `core.version` over the narrow JSON-RPC protocol

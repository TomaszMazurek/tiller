# Tiller

Terminal-first IDE cockpit for AI coding agents. You steer — agents work — git anchors.

Tiller hosts coding agents (Pi, AGY, Codex, Claude Code) in real terminals, exposes IDE
tools to them over MCP, and gives the human a review-first cockpit: semantic diffs,
worktree transactions with checkpoints, verification gates, and an approval flow anchored
in git. No chat windows — the conversation lives in the terminal.

**Status: planning + validation (Etap 0).** Spike 1 (terminal hosting of the Pi agent on
Windows/ConPTY) is done — all criteria passed, see `spikes/spike1-host/README.md`.

## Documentation

| Document | What it is |
|---|---|
| [`PROJECT_CORE.md`](PROJECT_CORE.md) | Foundation shared by all product profiles: architecture, principles, closed decisions, fork model |
| [`PROJECT_BRIEF_v4.md`](PROJECT_BRIEF_v4.md) | **Agent Edition** (active profile): cockpit, transactions, security model, stage plan |
| [`PROJECT_BRIEF_v3.md`](PROJECT_BRIEF_v3.md) | Classic Edition (dormant profile): editor-first IDE |
| [`research/`](research/) | Spike recon reports (Pi capabilities, ACP/MCP ecosystem) |
| [`spikes/`](spikes/) | Validation spikes (Etap 0) |
| [`AGENTS.md`](AGENTS.md) | Working rules for AI agents contributing in this repo |

## Stack (planned)

Rust core (independent crates: `core`/`index`/`lsp`/`git`/`term`/`rpc`/`orch`/`agents`),
Tauri 2 shell, React + dockview cockpit, xterm.js terminals, tree-sitter + LSP for language
intelligence, MCP (rmcp) for agent tools, ACP for future non-Pi agents.

## License

[Apache-2.0](LICENSE)

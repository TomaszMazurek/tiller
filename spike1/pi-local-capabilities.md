# Spike 1: Możliwości Pi coding agent jako hostowanego agenta zewnętrznego

Źródło: dokumentacja pakietu `@earendil-works/pi-coding-agent` (lokalna instalacja, `C:/tools/nvm/v24.14.0/node_modules/@earendil-works/pi-coding-agent/`). Wszystkie ścieżki względem `docs/`.

## 1. Tryb headless / print

**Werdykt: AVAILABLE.**

- `-p` / `--print` — uruchom podane prompty, wypisz finalny tekst asystenta na stdout, wyjście. `--mode text` nie wymusza one-shot przy TTY; `--print` tak. Gdy stdin/stdout nie jest TTY, print mode jest domyślny (`cli.md:21-49`, `cli-integration.md:15-27`).
- `--mode json` — jeden przebieg, JSONL na stdout: nagłówek sesji (`{"type":"session",...}`) + zdarzenia; potem wyjście. Nie przyjmuje dalszych poleceń (`cli-integration.md:38-49`, `json.md`).
- `--mode rpc` — długotrwały podproces, JSONL komendy na stdin, odpowiedzi + zdarzenia na stdout (`rpc.md`).
- Nie ma flag `--input-format`/`--output-format` (to konwencja AGY/Claude-CLI); w Pi rolę pełni `--mode`. Stream jest ścisły NDJSON rozdzielany tylko LF — nie używać Node `readline` (rozdziela też U+2028/U+2029) (`json.md`, sekcja "Framing").

Kształt strumienia (`json.md`): `agent_start`, `turn_start/end`, `message_start/update/end`, `message_update.assistantMessageEvent` (`text_delta`, `toolcall_start/delta/end`, `thinking_delta`, `done`, `error`...), `tool_execution_start/update/end` (z `toolCallId`, `args`, `partialResult`, `result`, `isError`, `durationMs`), `queue_update`, `compaction_*`, `auto_retry_*`, `agent_end` (koniec jednego runu — mogą jeszcze nastąpić retry/steering), `agent_settled` (koniec całej pracy automatycznej) (`json.md`, "Event sequence" i tabele).

**Seam**: do prostego one-shotu wystarczy `--print`; do streamingu jednorazowego `--mode json`.

## 2. SDK embedding

**Werdykt: AVAILABLE (Node/Bun, in-process, TypeScript).**

- `createAgentSession()` → `AgentSession` z `prompt()`, `steer()`, `followUp()`, `abort()`, `waitForIdle()`, `dispose()`, `subscribe(handler)`, `getLastAssistantText()`, `session.messages/model/thinkingLevel/systemPrompt/getActiveToolNames()` (`sdk.md`, "Getting started", "Session lifecycle", "Prompting").
- Zdarzenia: `session.subscribe(e => ...)`; `e.type === "message_update"` z `e.assistantMessageEvent.type === "text_delta"` dla streamingu tekstu (`sdk.md`, "Subscribing to events"). Zdarzenia pokrywają message updates, tool execution, queues, compaction, retries, lifecycle; `message_end` = autorytatywna wiadomość, `agent_settled` = host wie, że Pi nie będzie kontynuować. Typy: eksport `AgentSessionEvent` oraz `JsonAgentSessionEvent` (`json.md`, "TypeScript types").
- Pełna kontrola: własne `modelRuntime`, `settingsManager`, `SessionManager.inMemory()`, `resourceLoader`, `tools/excludeTools/customTools`, inline extensions (`sdk.md`, "Configuring a session"); 14 przykładów w `examples/sdk/`.
- Ograniczenie dla nas: SDK wymaga procesu Node/Bun — rdzeń w Rust nie może używać go bezpośrednio (albo pomost Node, albo CLI/RPC).

**Seam**: dla hosta w Rust — nie bezpośrednio; ale eksportowane typy protokołu (`JsonAgentSessionEvent`, `rpc-types.ts`) są źródłem prawdy dla Rust-side parsera.

## 3. Steering i anulowanie

**Werdykt: AVAILABLE (RPC / SDK).**

- RPC: `steer` (w kolejce po bieżącej turze asystenta, przed następnym wywołaniem LLM), `follow_up` (po zakończeniu), `abort` (przerywa i czeka na idle), `clear_queue`, `abort_retry`, `abort_bash` (`rpc-commands.md:32-129, 434-521`). `prompt` w trakcie streamingu wymaga `streamingBehavior: "steer" | "followUp"` (`rpc-commands.md:23-31`).
- SDK: `session.steer()`, `session.followUp()`, `session.abort()`, `waitForIdle()` (`sdk.md`, "Prompting").
- Tryb pracy w tle: `--mode rpc` to de facto daemon — proces żyje, wymiana JSONL, domyślne zamknięcie przez zamknięcie stdin (`rpc.md`, "Shutdown").
- Uwaga: JSON mode (one-shot) nie przyjmuje wejścia w trakcie — steering tylko RPC/SDK.

**Seam**: RPC mode.

## 4. Approvals / permissions

**Werdykt: PARTIAL** — brak wbudowanego systemu zgód (Pi domyślnie „nie pyta przed tool callem", `security.md:3`), ale gating jest w pełni implementowalny i sterowalny z zewnątrz.

- Mechanizm: extension handler `pi.on("tool_call", ...)` może zwrócić `{ block: true, reason }`; wewnątrz `await ctx.ui.confirm(...)` (`extensions.md:166-176`). MCP tool annotations (`readOnlyHint`, `destructiveHint`...) są dostępne przez `pi.getAllTools()` i służą za kryteria decyzji (`mcp.md:250-254`).
- W trybie RPC dialogi `ctx.ui.confirm/select/input/editor` stają się podprotokołem: Pi wysyła `extension_ui_request` (z `id`, `method`, `title`, opcjonalnym `timeout` i auto-resolve) na stdout i blokuje do czasu, aż host odpisze `extension_ui_response` na stdin (`rpc-extension-ui.md:1-40`). To jest dokładnie seam do zdalnego zatwierdzania uprawnień z Rust hosta: piszemy własne rozszerzenie-permission-gate + odpowiadamy na `extension_ui_request`.
- Odpowiedzi: `{"type":"extension_ui_response","id":...,"confirmed":true|false}` (confirm) / `value` (select/input) / `cancelled:true` (`rpc-extension-ui.md`).

**Seam**: extension permission-gate + RPC `extension_ui_request/response`.

## 5. Klient MCP

**Werdykt: AVAILABLE.**

- Transporty: **stdio** (`command`, `args`, `env`, `cwd`) oraz **streamable HTTP** (`url`, `headers`, `oauth`). WebSocket i legacy SSE NIE są wspierane (`mcp.md:1`, `mcp.md:47-49`).
- Konfiguracja: `~/.pi/agent/mcp.json` (user) i `.pi/mcp.json` (projekt, po project trust); lub CLI `pi mcp add/list/login/logout`; lub programowo `pi.registerMcpServer(name, config)` z extension (`mcp.md:17-24`, `extensions.md`, sekcja "MCP servers").
- **Localhost z Rust: TAK** — stdio server spawnowany przez samego Pi (Pi jest rodzicem procesu), albo nasz serwer HTTP na `http://127.0.0.1:PORT/mcp`. Pi łączy wszystkie serwery w tle przy starcie sesji; w SDK trzeba `session.bindExtensions()` (`sdk.md`, "Codemode and MCP").
- Narzędzia MCP przechodzą przez ten sam pipeline co wbudowane (permission extensions działają, `mcp.md:250-254`); nazwy `mcp__<server>__<tool>`; exposure `direct`/`codemode`/`tool_search`/`hidden`.

**Seam**: dla hosta — albo HTTP server w/sobokom Rust (`url`), albo stdio komenda uruchamiana przez Pi.

## 6. ACP (Agent Client Protocol / Zed)

**Werdykt: ABSENT.** `grep -i "acp|agent client protocol"` po wszystkich plikach `docs/*.md` — zero trafień. Integracja ACP wymagałaby własnego mostka (adapter RPC↔ACP) albo rezygnacji na rzecz trybu RPC.

## 7. Inne materiaalne funkcje dla hosta

- **Resume/fork sesji**: flagi `-c/--continue`, `-r/--resume`, `--session <path|id>`, `--session-id`, `--session-dir`, `--no-session`, `--fork` (`cli.md:78-107`); sesje to JSONL (`session-format.md`, wersja 3); RPC: `new_session` (z `parentSession`), `switch_session`, `fork`, `clone`, `get_entries`, `get_tree`, `get_messages`, `get_last_assistant_text` (`rpc-commands.md`); SDK: `AgentSessionRuntime` z `newSession/switchSession/fork/importFromJsonl` (`sdk.md`).
- **Statystyki kosztów/użycia**: RPC `get_session_stats` (tokeny, koszt, wykorzystanie okna kontekstu), `usage` w `message_update` i w `compaction_end` (`rpc-commands.md:526-531`, `json.md`).
- **State inspection**: `get_state` — model, `isStreaming`, `isCompacting`, tryby steering/followUp, plik i ID sesji, liczba wiadomości i kolejki (`rpc-commands.md:131-158`).
- **Eksport**: `pi --export <input> [output]` do HTML (`cli.md:49`), RPC `export_html` (`rpc.md`).
- **Sterowanie modelem**: `set_model`, `cycle_model`, `get_available_models`, `set_thinking_level`, `set_auto_compaction`, `set_auto_retry`, `compact` (`rpc-commands.md`).
- **Bash z hosta**: RPC `bash`/`abort_bash` + `bash_execution_update` streaming wyjścia (`rpc-commands.md:444-521`, `json.md`).
- **Env vars**: `PI_SESSION_ID/FILE`, `PI_PROVIDER/MODEL`, `PI_REASONING_LEVEL` dla narzędzi; `PI_CODING_AGENT_DIR`, `PI_OFFLINE` itd. dla procesu (`environment-variables.md`).
- **Exit status**: print mode zwraca nonzero przy stop reason `error`/`aborted`; JSON mode — nie zawsze (trzeba czytać zdarzenia), nonzero tylko przy rzuceniu wyjątku (`cli-integration.md:22-24, 47-48`).
- **Gotowy klient referencyjny**: eksportowany `RpcClient` (TS) — spawn, korelacja ID, typed commands, eventy (`rpc.md`, `cli-integration.md`); przykład Python klienta w `rpc.md`.
- **Rebranding**: `package.json` → `piConfig.name/configDir` pozwala zmienić nazwę CLI i katalog konfiguracji (`cli-integration.md`, "Fork and rebrand") — przydatne, by nasza dystrybucja nie kolidowała z systemowym pi użytkownika.

## Synteza — rekomendowany seam integracji

Głównym seamem dla rdzenia w Rust powinien być **tryb RPC** (`pi --mode rpc [--no-session]`): długotrwały podproces, ścisły JSONL (stdin=komendy, stdout=odpowiedzi+zdarzenia), pełne pokrycie naszych potrzeb — prompt/steer/follow_up/abort, subskrypcja narzędzi i delt tekstu, `get_state`/`get_session_stats`, fork/rezume sesji, oraz — kluczowe — podprotokół `extension_ui_request/response`, który daje programowe bramkowanie wrażliwych narzędzi (własne rozszerzenie permission-gate + odpowiedzi z hosta). Adapter AGY (`--input-format/--output-format stream-json`) nie ma bezpośredniego odpowiednika flagowego w Pi; najbliższy odpowiednik to właśnie RPC mode (dla dwukierunkowej kontroli) lub `--mode json` (dla jednorazowego streamingu bez sterowania). SDK zostawić jako opcję przyszłościową (in-process, wymaga pomostu Node w Rust) i jako źródło typów (`rpc-types.ts`, `JsonAgentSessionEvent`) dla implementacji parsera w Rust. ACP: brak — gdyby był wymagany, trzeba napisać mostek RPC↔ACP. Wsparcie MCP: pełne (stdio + streamable HTTP na localhost), co pozwala nam serwować dodatkowe narzędzia IDE agentowi.

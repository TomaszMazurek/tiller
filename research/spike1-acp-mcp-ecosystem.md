# Research: Ekosystem ACP + MCP dla rdzenia IDE w Rust (kokpit agentów + serwer narzędzi)

## Summary (PL)
MCP jako warstwa "nasze IDE eksponuje narzędzia" jest w pełni wykonalne i tanie: oficjalny Rust SDK (`rmcp`, obecnie seria 3.x) jest dojrzały (Tier 1 wg własnej roadmapy, 100% konformancji serwera), ma stdio i Streamable HTTP (serwer i klient), a minimalny serwer narzędzi na tokio to kilka–kilkadziesiąt linii z makrami `#[tool]` — realnie godziny–1 dzień na prototyp, 2–4 dni z HTTP/sesjami. ACP (Agent Client Protocol od Zed) to obecnie najszerszy interoperacyjny standard "kokpit ↔ agent": JSON-RPC 2.0 po stdio (proces potomny), stabilne v1, oficjalny Rust SDK z rolą Client, dziesiątki wdrożonych agentów (Gemini CLI, Claude Agent, Codex, Copilot CLI w preview, Goose, OpenCode…) i klientów (Zed, JetBrains, VS Code, Neovim, Emacs…) — nadaje się jako warstwa prezentacji agentów w kokpicie, z zastrzeżeniem, że v2 to draft. Wzorzec hostingu u porównywalnych produktów to niemal uniwersalnie spawn procesu + strukturalny strumień zdarzeń (ACP stdio), a nie SDK in-process; główne pułapki to Windows (ConPTY, zabicie drzewa procesów, brak przenośnego `kill()`), czystość stdout (tylko protokół, logi na stderr) i anulowanie na poziomie protokołu (`session/cancel`), nie sygnałami.

---

## Pytanie 1: ACP — Agent Client Protocol (Zed)

### Status i dojrzałość
- **Protokół**: JSON-RPC 2.0; przepływ `initialize` → (`authenticate`) → `session/new`/`session/load` → `session/prompt` ze strumieniem powiadomień `session/update` (fragmenty wiadomości, tool cally, plany, tryby, kompakcja kontekstu) i anulowaniem przez `session/cancel`. Strona klienta: `session/request_permission`, opcjonalnie `fs/read_text_file`, `fs/write_text_file`, `terminal/*`, `elicitation/create`. Rozszerzalność przez `_meta` i metody z prefiksem `_`. Istnieje Test Compatibility Kit (TCK). — *dowód bezpośredni, specyfikacja oficjalna*.
  - Źródło: [ACP Overview](https://agentclientprotocol.com/protocol/overview), [ACP Transports](https://agentclientprotocol.com/protocol/transports)
- **Transport**: stdio jako jedyny obecnie zdefiniowany transport (klient uruchamia agenta jako podproces; wiadomości JSON-RPC rozdzielane `\n`, stdout zarezerwowany wyłącznie dla protokołu, logi na stderr). Streamable HTTP to *draft proposal in progress*. — *dowód bezpośredni*.
- **Wersje**: stabilne **v1**; **v2 = draft** (w Rust SDK za feature `unstable_protocol_v2`); MCP-over-ACP za feature `unstable_mcp_over_acp`. — *dowód bezpośredni z README oficjalnego Rust SDK*.
  - Źródło: [agentclientprotocol/rust-sdk README](https://github.com/agentclientprotocol/rust-sdk/blob/main/README.md)

### Rust SDK / crates
- Oficjalny Rust SDK: crate **`agent-client-protocol`** (Apache 2.0) — role Client, Agent, Proxy, Conductor; towarzyszące crates: `agent-client-protocol-http` (HTTP/SSE **i WebSocket**), `agent-client-protocol-rmcp` (integracja z rmcp), `-derive`, `-conductor` (orkiestracja łańcuchów proxy), `-polyfill`, `-test`, `-cookbook`, przykłady klienta i agenta. Wersja na docs.rs widoczna w wynikach: 3.3.0 (numer mogła już ulec zmianie — sprawdź crates.io przy adopcji). — *dowód bezpośredni (README), numer wersji: średnia pewność*.
  - Źródła: [docs.rs/agent-client-protocol](https://docs.rs/agent-client-protocol), [rust-sdk README](https://github.com/agentclientprotocol/rust-sdk/blob/main/README.md)
- Oficjalne SDK też w TypeScript (`@agentclientprotocol/sdk`) i Kotlin (`acp-kotlin`).

### Kto wdraża (fragment oficjalnych list, luty 2026+)
- **Klienci**: Zed, JetBrains (AI Assistant), VS Code (rozszerzenia ACP Client/ACP Pro/Multicoder), Visual Studio (Poolside), Neovim (CodeCompanion, avante.nvim…), Emacs (agent-shell), Pulsar, Qt Creator (wtyczka), Sublime, Obsidian (wiele wtyczek), Chrome, Unity, desktop/web/CLI/TUI (AionUi, GitKraken Kepler, Devin Desktop…). — *dowód bezpośredni: [Clients](https://agentclientprotocol.com/overview/clients)*
- **Agenci**: Gemini CLI, Claude Agent (przez adapter Zed), Codex CLI (przez adapter `codex-acp`), GitHub Copilot (public preview — [changelog GitHub 2026-01-28](https://github.blog/changelog/2026-01-28-acp-support-in-copilot-cli-is-now-in-public-preview/)), Cursor CLI, Cline, Goose, OpenCode, Junie (JetBrains), Kiro CLI, Kimi CLI, Qwen Code, Mistral Vibe, OpenHands, Factory Droid i in. — *dowód bezpośredni: [Agents](https://agentclientprotocol.com/overview/agents)*
- **Zed jako referencja**: External Agents to osobne procesy komunikujące się przez ACP; agent posiada własny runtime/auth/model/narzędzia; serwery MCP Zeda "mogą być przekazywane przez ACP"; instalacja z ACP Registry. — *dowód bezpośredni: [Zed External Agents](https://zed.dev/docs/ai/external-agents)*

### Pewność i implikacje
- **Pewność: WYSOKA** co do statusu (stabilne v1, stdio, szeroka adopcja, oficjalny Rust SDK z rolą Client), **ŚREDNIA** co do aktualnych numerów wersji crates.
- **Implikacja decyzyjna**: ACP jest projektowane dokładnie pod nasz przypadek "kokpit prezentuje hostowanych agentów" (rola Client, subprocesy, strumień `session/update`, permissions, terminale, elicitation). Dojrzałość: produkcyjna dla v1; unikać zależności od v2/MCP-over-ACP do czasu stabilizacji.

---

## Pytanie 2: MCP — oficjalny Rust SDK (`rmcp`)

### Status i dojrzałość
- `rmcp` (repo [modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)) to **oficjalny** Rust SDK, async na tokio. Implementuje stabilną specyfikację MCP **`2026-07-28`** i pozostaje w pełni kompatybilny z `2025-11-25` i wcześniejszymi. — *dowód bezpośredni (README)*.
- **ROADMAP**: wszystkie wymagania SEP-1730 **Tier 1 spełnione** — konformancja serwera 100% (30/30) i klienta 100% (39/39) dla obu suit datowanych; stabilne wydanie ≥1.0.0 (v3.0.0 wydane 2026-07-28 wg roadmapy), polityki VERSIONING/DEPENDENCY/SECURITY opublikowane, 48/48 funkcji nieeksperymentalnych udokumentowanych. — *dowód bezpośredni ([ROADMAP.md](https://github.com/modelcontextprotocol/rust-sdk/blob/main/ROADMAP.md))*. Uwaga: to oświadczenie samego projektu (patrz Sprzeczności — katalog SDK MCP może etykietować Rust jako Tier 2).

### Transporty (dowód bezpośredni z `crates/rmcp/Cargo.toml`)
- **stdio**: feature `transport-io` (serwer — `tokio::io::stdin/stdout`), `transport-child-process` (klient; spawn z `process-wrap`).
- **Streamable HTTP**: `transport-streamable-http-server` (z sesjami, integracja tower — `StreamableHttpService`), `transport-streamable-http-client(-reqwest)`; opcjonalny unix socket po stronie klienta; OAuth2/client-credentials za `auth`.
- **WebSocket**: **brak wbudowanego wsparcia** — w Cargo.toml feature `# transport-ws = ["transport-io", "dep:tokio-tungstenite"]` jest **zakomentowany** (plik `src/transport/ws.rs` istnieje w źródłach, ale nie jest wystawiany). Custom transport jest możliwy przez API `Transport`/`IntoTransport`.
- Domyślne feature'y: `["base64", "macros", "server"]` — serwer z makrami działa od razu po `cargo add rmcp`.

### Szacunek nakładu: mały serwer narzędzi MCP na tokio
- Wzorzec: struct z `#[tool_router]`, metody z `#[tool]` (opis + schemat argumentów z derive `JsonSchema`), `#[tool_handler]`, serwowanie przez `ServiceExt::serve(stdio())`. Wiele niezależnych tutoriali pokazuje działający serwer stdio w ~50–100 liniach. — *dowody z tutoriali + oficjalny README; szacunek godzin: **inferencja badacza***.
- Szacunkowy nakład (inferencja na bazie tutoriali i struktury SDK): **prototyp stdio: 0,5–1 dzień**; **wersja produkcyjna z Streamable HTTP (tower, sesje, auth), testami i CI: 2–4 dni**. Ryzyko niskie — SDK stabilne, conformance 100%, dokumentacja z przykładami.

### Pewność i implikacje
- **Pewność: WYSOKA** (status, feature'y transportów, brak WS, prostota serwera stdio); **ŚREDNIA** dla etykiety Tier 1 vs Tier 2 w katalogu MCP (patrz Sprzeczności).
- **Implikacja decyzyjna**: budujemy ekspozycję narzędzi IDE jako serwer MCP na `rmcp` (stdio dla lokalnych agentów; Streamable HTTP gdy zdalny/wiele klientów). Nie projektować pod WebSocket w rmcp — dopiero gdy feature wróci do Cargo.toml albo przez własny transport.

---

## Pytanie 3: Wzorce hostowania agentów CLI u porównywalnych produktów

### Zed (Agent Panel / External Agents) — spawn + strukturalne zdarzenia
- Zed uruchamia agenta jako **podproces** (konfiguracja `command`/`args`/`env` w `agent_servers`) i rozmawia przez **stdio** (ACP = JSON-RPC z rozdzielaczem `\n`). Agent posiada własny auth/model/narzędzia; Zed hostuje tylko UI wątku. Serwery MCP Zeda mogą być forwardowane do agenta po ACP. Instalacja agentów z ACP Registry. — *dowody bezpośrednie: [Zed External Agents](https://zed.dev/docs/ai/external-agents), [ACP Transports](https://agentclientprotocol.com/protocol/transports)*.
- Wymóg higieny strumieni: agent **MUSI** nie pisać nic poza wiadomościami ACP na stdout; logi na stderr. — *dowód bezpośredni (spec ACP)*.

### Continue (CLI `cn`) — subprocess + wyjście JSON, bez publicznego SDK in-process
- Tryb headless `cn -p "..."` dla skryptów/CI; konfiguracja przez `config.yaml` (modele, reguły, serwery MCP); dokumentowana ścieżka integracji to **wywołanie jako subprocess i konsumpcja wyjścia (w tym JSON)** — brak stabilnego, publicznego SDK do osadzenia in-process; `cn serve`/`cn remote` istnieją, ale ich status jako stabilnego API hostingu nie jest potwierdzony w źródłach. — *interpretacja dokumentacji GitHub Continue; pewność ŚREDNIA*.
  - Źródła: [CLI quickstart](https://github.com/continuedev/continue/blob/main/docs/cli/quickstart.mdx), [extensions/cli/README](https://github.com/continuedev/continue/blob/main/extensions/cli/README.md)

### VS Code / ogólne osadzanie CLI — pipes vs PTY, Windows, anulowanie
- **Pipes (spawn bez shell)** do sterowania maszynowego; **PTY tylko gdy CLI naprawdę wymaga terminala** (TUI, interaktywne prompty, kolory). Na Windows PTY = **ConPTY** (emulacja konsoli — dodatkowa złożoność: resize, wejście, zamykanie). — *dowody: [Microsoft Learn — Pseudoconsole](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session), [VS Code Task Provider](https://code.visualstudio.com/api/extension-guides/task-provider)*.
- **Windows / anulowanie**: `child.kill()` nie jest przenośnym graceful-cancel (sygnały emulowane jako twarde ubicie); ubicie rodzica nie zabija potomków — do niezawodnego czyszczenia drzewa procesów na Windows zalecane **Job Objects** (natywny helper/biblioteka). — *dowody: [Node.js process docs](https://nodejs.org/api/process.html), [Microsoft Learn — Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)*. Spójna praktyka: najpierw anulowanie na poziomie protokołu agenta (w ACP: `session/cancel`), potem timeout i ubicie całego drzewa. *(ostatnie zdanie: synteza badacza z powyższych źródeł + spec ACP)*
- Szeroki trend: klienci ACP (w tym rozszerzenia VS Code, fork Windows Terminal "Intelligent Terminal") napędzają CLI agentów przez **stdio**, nie przez parsowanie TTY. — *dowód: [listy klientów ACP](https://agentclientprotocol.com/overview/clients)*.

### Implikacja decyzyjna
Dla kokpitu w Rust: przyjmujemy wzorzec Zed — **spawn procesu + ACP stdio + strumień `session/update`**; nie budujemy własnego parsera TUI. Na Windows od pierwszego dnia: dedykowane zarządzanie drzewem procesów (job object / `taskkill /T` analog), normalizacja ścieżek/env, testy ConPTY tylko jeśli zdecydujemy się hostować TUI-owe CLI poza ACP.

---

## Contradictions (Sprzeczności)
1. **Tier rmcp**: ROADMAP `rmcp` twierdzi "all SEP-1730 Tier 1 requirements are met", natomiast wynik wyszukiwania sugerował, że katalog SDK na modelcontextprotocol.io etykietuje Rust jako **Tier 2**. Samego katalogu nie zweryfikowałem bezpośrednio. ROADMAP tłumaczy, że `tier-check` CLI może błędnie raportować `stable_release` przez prefiks tagu `rmcp-v`. → Traktować Tier 1 jako deklarację projektu; przy due diligence sprawdzić katalog na modelcontextprotocol.io. (Źródła: [ROADMAP.md](https://github.com/modelcontextprotocol/rust-sdk/blob/main/ROADMAP.md); wynik wyszukiwania — nieweryfikowalny bezpośrednio.)
2. **WebSocket w rmcp**: starsze materiały/tutorialne bywają niejednoznaczne (`ws.rs` w źródłach); Cargo.toml rozstrzyga — feature zakomentowany, brak wbudowanego WS. (Dowód bezpośredni.)
3. Brak sprzeczności co do statusu ACP v1/v2 — README Rust SDK i dokumentacja są spójne (v1 stabilny, v2 draft).

## Missing evidence (Luki / niska pewność)
- **Aktualny numer wersji** `agent-client-protocol` na crates.io (widziany 3.3.0 na docs.rs w wynikach; nie odświeżane w momencie pisania) — pewność ŚREDNIA.
- **Etykieta Tier dla Rust w katalogu MCP** (modelcontextprotocol.io) — niepotwierdzona bezpośrednio.
- **`cn serve` / `cn remote` w Continue** jako stabilne API hostingu — niepotwierdzone; integracja z Continue = subprocess + JSON out (ŚREDNIA).
- **Streamable HTTP w ACP** — tylko "draft proposal in progress" (spec); brak harmonogramu.
- **Wsparcie JetBrains ACP** — link do oficjalnej pomocy JetBrains widnieje na liście klientów ACP (pośredni dowód); szczegółów nie pogłębiałem.
- Szacunki nakładu pracy (godziny/dni) to **inferencja badacza** z tutoriali i struktury SDK, nie zmierzony benchmark.

## Sources
**Kept (źródła pierwotne/najmocniejsze):**
- ACP Overview — https://agentclientprotocol.com/protocol/overview (model protokołu, metody, anulowanie)
- ACP Transports — https://agentclientprotocol.com/protocol/transports (stdio, czystość stdout, HTTP draft)
- ACP Clients — https://agentclientprotocol.com/overview/clients (lista wdrożeń)
- ACP Agents — https://agentclientprotocol.com/overview/agents (lista agentów + changelog Copilot)
- agentclientprotocol/rust-sdk README — https://github.com/agentclientprotocol/rust-sdk/blob/main/README.md (oficjalne crates, v1/v2, MCP-over-ACP)
- docs.rs/agent-client-protocol — https://docs.rs/agent-client-protocol (API Rust)
- modelcontextprotocol/rust-sdk README — https://github.com/modelcontextprotocol/rust-sdk (spec 2026-07-28, feature'y, przykłady)
- modelcontextprotocol/rust-sdk ROADMAP — https://github.com/modelcontextprotocol/rust-sdk/blob/main/ROADMAP.md (Tier 1, konformancja 100%)
- crates/rmcp/Cargo.toml (wgląd w repo) — transporty; `transport-ws` zakomentowany
- Zed External Agents — https://zed.dev/docs/ai/external-agents (spawn + ACP, własność auth/konfiguracji, MCP forwarding)
- Microsoft Learn: Pseudoconsole (ConPTY) — https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session
- Microsoft Learn: Job Objects — https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
- Node.js process docs — https://nodejs.org/api/process.html (semantyka kill/sygnałów na Windows)
- VS Code Task Provider — https://code.visualstudio.com/api/extension-guides/task-provider (wzorzec anulowania w Pseudoterminal)
- Continue CLI docs — https://github.com/continuedev/continue/blob/main/docs/cli/quickstart.mdx i https://github.com/continuedev/continue/blob/main/extensions/cli/README.md

**Rejected/deprioritized:** blogi tutorialowe (rup12.net, Shuttle, MCPOrbit, AgentCat) — użyte tylko jako potwierdzenie prostoty serwera stdio w rmcp, nie jako dowód statusu; zed.dev/blog/acp-progress-report — użyte pośrednio, zastąpione oficjalnymi listami; leviath.dev/looprig — nieoficjalne kopie/opisy.

## Next steps
1. Spike kodowy: serwer MCP (rmcp, stdio) z 2–3 narzędziami IDE + Streamable HTTP variant — potwierdzi szacunek 2–4 dni.
2. Spike kodowy: klient ACP na `agent-client-protocol` (spawn Gemini CLI / `codex-acp`, render `session/update`, `session/cancel`, permissions) — weryfikacja dopasowania do UI kokpitu.
3. Sprawdzić aktualny katalog SDK na modelcontextprotocol.io (etykieta Tier dla Rust) oraz numer wersji `agent-client-protocol` na crates.io.
4. Prototyp zarządzania procesami na Windows (job object) dla drzewa procesów agenta — ryzyko platformowe nr 1.

---

## Bottom line (dla rdzenia Rust)
**Serwer MCP: TAK, niskim kosztem.** Używamy `rmcp` (oficjalny, tokio, serwer w domyślnych feature'ach, stdio + Streamable HTTP, conformance 100%, stabilne 3.x); minimalny serwer narzędzi to godziny, produkcyjna wersja z HTTP/auth/testami ~2–4 dni; nie zakładać WebSocket (feature wyłączony w Cargo.toml — własny transport dopiero gdy realnie potrzebny). **ACP: ADOPT (jako klient) dla warstwy prezentacji agentów w kokpicie** — protokół stabilny w v1, oficjalny Rust SDK z rolą Client/Proxy, stdio pod spawn podprocesów, największa w ekosystemie lista gotowych agentów (Gemini, Claude, Codex, Copilot, Goose, OpenCode…); pinujemy v1, traktujemy v2 i MCP-over-ACP jako watch-list, a własne, niestandardowe mechanizmy prezentacji (poza ACP) odkładamy do czasu, aż pokaże się realna luka — największym nieweryfikowalnym ryzykiem pozostaje platforma Windows (drzewo procesów, ConPTY), którą adresujemy osobnym spikiem, nie wyborem protokołu.
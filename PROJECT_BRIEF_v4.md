# Tiller — Agent Edition: brief projektu i stack (v4)

> Fork produktowy profilu **Agent Edition** (aktywny). Fundament wspólny, zasady pracy i decyzje zamknięte: **`PROJECT_CORE.md`**.

## 0. Relacja do v3

v4 nie unieważnia v3 — zachowuje bazowy stack i zasadę separacji rdzeń/UI, ale **rozszerza fundament** (protokoły agentowe MCP/ACP, transakcyjny model pracy, granica bezpieczeństwa wykonania, crate'y `orch`/`agents`) i rezygnuje z frontendowego tree-sittera. To, co się odwraca, to produkt: **człowiek przestaje być piszącym użytkownikiem edytora, a staje się nadzorcą i recenzentem pracy agentów**.

## 1. Cel projektu

IDE dla **agent-only coding**: kod piszą agenty AI, człowiek steruje, przegląda i zatwierdza. Produkt pełni dwie role naraz:

1. **Kokpit nadzoru** — misja-control: zadania, statusy agentów (pochodne z obserwacji sesji i gita), diffy, zatwierdzenia, rollback; **rozmowa z agentem żyje w terminalu**.
2. **Dostawca zmysłów i rąk** — rdzeń IDE wystawia agentom narzędzia, których samemu agentowi brakuje: indeks symboli, diagnostyki LSP, semantyczny diff, kontrolowane wykonywanie komend.

**Kluczowa obserwacja architektoniczna:** agent jest **drugim klientem rdzenia, równorzędnym z frontendem**. Rdzeń headless z wąskim protokołem i wymiennym transportem (v3) obsługuje to natywnie: do człowieka mówi kokpit (React UI przez JSON-RPC), do agenta mówi MCP (narzędzia) i ACP (prezentacja i sterowanie).

**Przewaga własna:** dogfoodujemy istniejący, działający ekosystem agentowy (Pi: orchestrator, subagenci, worktree, handoffy). To IDE nie wymyśla agent-stack od zera — jest cockpitem i warsztatem narzędziowym dla agentów, których już używamy codziennie.

## 2. Koncepcja: fundament i części wymienne

**Fundament (nieodwracalne, od dnia 1):**
- granice crate'ów rdzenia i trait narzędzi (wspólny mianownik dla UI, MCP i ewentualnego przyszłego własnego silnika),
- agent jako klient rdzenia: MCP + ACP jako standardowe szwy integracyjne,
- **transakcyjny model pracy**: worktree per zadanie, commit = checkpoint = jednostka review, **niezmienny kandydat review** (id zmiany + commit bazowy) powiązany z dowodami weryfikacji i zgodą człowieka, bramka zatwierdzenia na merge do głównego drzewa (integracja wymaga sprawdzenia aktualnej bazy),
- **granica bezpieczeństwa wykonania obejmująca cały proces agenta i jego potomków** (pliki, sieć, sekrety) — `term` jest kontrolowanym interfejsem, nie jedyną zaporą — plus model uprawnień komend, sandbox i audit; nie dodatek na później,
- ewaluacja: wzorcowy zestaw zadań i metryka skuteczności, na którym mierzymy każdą decyzję narzędziową.

**Części wymienne (odwracalne):**
- agenty (Pi nr 1; claude-code, codex później) — przez adaptery ACP/MCP/proces,
- modele LLM — żyją po stronie agentów, nie naszego IDE,
- kokpit UI (React + dockview), mechanizm diffu, podświetlanie (Lezer/Shiki),
- transport (Tauri IPC ↔ WebSocket).

## 3. Podjęte decyzje

| Obszar | Decyzja |
|---|---|
| Język rdzenia | **Rust** |
| Shell aplikacji | **Tauri 2** |
| UI | **React + dockview** — kokpit (terminal, diffy, review), nie edytor |
| Interakcja z agentem | **Terminal** — cała praca z agentem w oknie terminala; zero dedykowanych okien czatu/timeline; **wyjątek: okno komunikacji wizualnej** (wklejanie zrzutów/obrazów + szkicownik) |
| Przeglądanie kodu | **CodeMirror 6 read-only** (tryb review) + podświetlanie **Lezer/Shiki**; bez własnego mostu do tree-sitter |
| Diff | **Semantyczny w stylu difftastic** (tree-sitter, Rust) obok liniowego |
| Agenty | **Hostowane zewnętrznie w terminalu**: **Pi (nr 1)**, **AGY**, **Codex**, **Claude Code** (przyszłość); obserwacja sesji i wstrzykiwanie przez adaptery per agent |
| Własny silnik agenta | **Odroczony** — szew: trait narzędzi w rdzeniu; wraca tylko, jeśli adaptery przestaną wystarczać |
| Inteligencja języka | **LSP read-only** (diagnostyki, go-to-def, references w review) + **tree-sitter w rdzeniu** (indeks, kompresja kontekstu, diff) |
| Weryfikacja | **Verify loop**: `cargo check`/`test` w worktree (autorytatywna); **fast loop**: diagnostyki LSP (best-effort, przyszłe narzędzie MCP) |
| Transakcje | **Worktree per zadanie**, jeden pisarz na worktree; **checkpoint = commit**; merge po zatwierdzeniu |
| Bezpieczeństwo | **Granica wykonania na procesie agenta i potomkach** (fs, sieć, sekrety) + model uprawnień komend: allowlista / potwierdzenie / zakaz; sandbox (decyzja Windows w Etapie 0); audit log |
| Języki wspierane | **Rust nr 1** (MVP), C++ nr 2 |
| Platformy | **Windows-first**; CI od początku Windows/Linux/macOS |
| Tryb pracy | **Solo**, z granicami crate'ów gotowymi na zespół |

### Uzasadnienie (najważniejsze)

- **Hostowanie zamiast własnego silnika:** silnik agentowy (zarządzanie kontekstem, kompakcja, retry, steering w locie, uprawnienia) to najtrudniejsza i najszybciej starzejąca się warstwa; Devin i OpenHands poświęcili jej lata pracy dużych zespołów. Mamy działający harness, którego używamy codziennie — pionowy plaster „Pi + worktree + diff + approve" daje pierwszą wartość w tygodnie. Trait narzędzi w rdzeniu pozostawia szew pod własny silnik bez przepisywania.
- **Worktree zamiast bufora w pamięci jako ground truth:** kompilator i testy pracują na plikach; pełne błędy daje rustc, nie rust-analyzer. Agent pisze więc swobodnie **w swoim worktree**, a bramka jest na merge — nie na każdą edycję (bramkowanie per edycja zabija zmiany wieloetapowe). Overlay LSP (`didChange` bez zapisu) zostaje jako tani fast-loop, nie jako mechanizm transakcyjny.
- **ACP + MCP zamiast własnego protokołu adaptera — rozstrzygnięte rekonesansem (Spike 1, 2026-10-11):** role są różne, nie zamienne — **MCP**: rdzeń jest serwerem narzędzi, agent klientem; **ACP**: kokpit jest klientem, agent obsługuje protokół prezentacji; **PTY**: surowe wejście/wyjście bez strukturalnego sterowania. Werdykt rekonesansu: **Pi udostępnia natywny tryb RPC** (`--mode rpc`: dwukierunkowy JSONL — prompt/steer/follow_up/abort, zdarzenia strukturalne, statystyki sesji, oraz podprotokół `extension_ui_request/response` dający programowe bramkowanie zatwierdzeń z hosta) i **nie implementuje ACP** — więc adapter Pi używa natywnego RPC (pipes, bez PTY); **ACP (kokpit jako klient, oficjalny crate `agent-client-protocol`, pin v1) wchodzi dopiero z agentami niebędącymi Pi**; serwer MCP na oficjalnym **`rmcp`** (stdio + Streamable HTTP; WebSocket wyłączony w SDK — nie projektować pod WS). Szczegóły: `research/spike1-*.md`.
- **Semantyczny diff:** difftastic jest w Ruście i oparty o tree-sitter — możliwy do osadzenia lub wzorowania; realna przewaga review-UI nad diffem liniowym dla zmian refaktoryzacyjnych.
- **Ewaluacja przed budową:** bez wzorcowych zadań i metryki skuteczności łatwo optymalizować metryki pośrednie (tokeny) zamiast rezultatów.

## 4. Stack

### Rdzeń (Rust)

| Zadanie | Biblioteka |
|---|---|
| Async, procesy, I/O | `tokio` |
| Bufory, model workspace | `ropey` (widok/bufory UI, nie ground truth) |
| Parsowanie, indeks, kompresja AST, diff | `tree-sitter` (+ wzorce `difftastic`) |
| Klient LSP | `lsp-types` + własny klient |
| Git: worktree, checkpointy, timeline | `git2`/`gix` |
| PTY z modelem uprawnień | `portable-pty` + własna warstwa polityk |
| Obserwowanie plików | `notify` |
| Szukanie, indeks symboli | `ignore` + `tantivy` + `rusqlite` |
| Serwer MCP + klient ACP | własne, na `rpc` |
| Współbieżność CPU | `rayon` |

```
crates/
├─ core/     # model workspace, zdarzenia, TRAIT NARZĘDZI (szew pod wszystko)
├─ index/    # tree-sitter + SQLite + tantivy; kompresja kontekstu (AST skeleton)
├─ lsp/      # klient LSP: diagnostyki + nawigacja read-only (review + narzędzie MCP)
├─ git/      # worktree management, checkpointy, timeline, semantyczny diff
├─ term/     # PTY + model uprawnień (allowlista/potwierdzenie/zakaz) + sandbox
├─ orch/     # kolejka zadań, alokacja worktree, kolejka review, merge
├─ agents/   # adaptery per agent: Pi, AGY, Codex, Claude Code — hosting TUI, obserwacja sesji, wstrzykiwanie wizualne
├─ rpc/      # JSON-RPC dla kokpitu + serwer MCP + klient ACP
├─ dap/      # klient DAP (późny etap)
├─ agent-daemon/  # tryb headless (WebSocket), dziedziczony z v3
└─ src-tauri/ + ui/  # kokpit
```

### Frontend (TypeScript)

- **React + dockview**: kokpit — **praca z agentem odbywa się wyłącznie w terminalu**; pozostałe panele to powierzchnie przeglądu, nie rozmowy: drzewo plików ze statusami (zmienione / w edycji / błąd weryfikacji), Diff Inspector (liniowy + semantyczny) i podgląd read-only w centrum, kolejka review z „Approve & Merge" / „Rollback", bramki (`cargo check`/`test`) na dole. Status pracy agenta jest pochodny: tail pliku sesji (JSONL) + stan worktree/gita — nie okno czatu. **Jedyny wyjątek: okno komunikacji wizualnej** — terminal nie przyjmuje multimediów (szczegóły niżej).
- **CodeMirror 6** (tryb review) z podświetlaniem **Lezer/Shiki** + nawigacją LSP (go-to-def, references w diffie)
- **xterm.js**: **terminal — główne okno pracy z agentem** (hosting TUI agenta przez PTY) oraz podgląd bramek/procesów
- **Narzędzia prezentacji przez MCP** (propozycja, do walidacji w Etapie 2): agent otwiera widoki kokpitu — `ide_show_diff`, `ide_open_file` — wołając narzędzia IDE; bogata integracja bez okna czatu, a wywołania lądują w sesji JSONL (struktura zachowana)
- **Okno komunikacji wizualnej** — jedyne „mówienie" do agenta poza terminalem: (a) wklejanie/drop zrzutów i obrazów, (b) **szkicownik** na osadzonym **Excalidraw** (MIT, gotowy komponent React) — schematy, diagramy, wizualizacje koncepcji. Przepływ: eksport **PNG + SVG** → katalog spool per sesja → wstrzyknięcie do rozmowy przez adapter agenta. **SVG jest tekstem** — czyta go każdy agent nawet bez modelu wizyjnego; PNG wymaga modelu z vision (zależne od providera). Możliwości dziś: `read` w Pi przyjmuje obrazy jako załączniki, Codex i Claude Code obsługują obrazy natywnie; AGY — do zweryfikowania (spike). Okno jest **interfejsem wejściowym, nie czatem**: przetworzone obrazy pozostają w pamięci rozmowy agenta (sesji), odpowiedzi wracają w terminalu; okno nie renderuje historii ani odpowiedzi.

## 5. Architektura

```
┌───────────────────────────┐      ┌───────────────────────────┐
│  Kokpit (React + dockview)│      │  Agenty (Pi, claude-code…)│
└────────────┬──────────────┘      └────────────┬──────────────┘
             │ JSON-RPC                          │ ACP: prezentacja i sterowanie
             │ (Tauri IPC / WebSocket)           │ MCP: narzędzia (index, LSP, term)
┌────────────┴───────────────────────────────────┴────────────┐
│  Rdzeń: crate'y Rust niezależne od Tauri                    │
│  core · index · lsp · git · term · orch · agents · rpc      │
│  zdarzenia: plik zmieniony · diagnostyki · stan agenta ·    │
│  postęp bramek · checkpoint zmergowany                      │
└─────────────────────────────────────────────────────────────┘
```

**Zasady:**
1. Rdzeń w osobnych crate'ach, niezależnych od Tauri (dziedziczone z v3).
2. **Agent = drugi klient rdzenia, równorzędny z UI**; ten sam rdzeń, dwie twarze. **Terminal-first: praca z agentem odbywa się w terminalu** — interaktywny agent działa jako TUI w panelu terminala (PTY) i tam jest sterowany; kokpit nie renderuje własnego czatu ani timeline agenta — strukturę pracy czerpie z obserwacji (plik sesji JSONL, worktree/git). Tryb RPC agenta służy zadaniom inicjowanym przez IDE w tle, nie zastępuje terminala.
3. UI rozmawia własnym wąskim protokołem JSON-RPC; agenty — przez MCP/ACP; wspólny mianownik: **trait narzędzi w `core`**.
4. **Transakcje pracy:** zadanie → `orch` przydziela worktree → agent pracuje swobodnie (czyta przez narzędzia, pisze pliki, uruchamia weryfikację) → checkpointy jako commity → **niezmienny kandydat review** (id zmiany + commit bazowy) z diffem, transkryptem i wynikami bramek → człowiek approve/merge albo request changes. Zmiana kandydata **unieważnia zgodę**; merge sprawdza się względem **aktualnej bazy** (rebase + weryfikacja). Rollback = zatrzymanie pisarza, potem reset worktree wraz z plikami nieśledzonymi; skutki komend poza repo są poza gwarancją rollbacku (ale w audycie).
5. **Dwa wątki weryfikacji:** verify loop (cargo check/test, autorytatywny, w worktree) i fast loop (diagnostyki LSP, best-effort — docelowo narzędzie MCP).
6. **Bezpieczeństwo wykonania:** granica obejmuje **cały proces agenta i jego potomków** (dostęp do plików, sieci, sekretów), nie tylko wywołania przez `term` — agent nie może obejść polityki własnym shellem czy bezpośrednim zapisem plików. `term` jest kontrolowanym interfejsem; klasyfikacja komend (allowlista / potwierdzenie / zakaz), sandbox, sekrety poza środowiskiem agenta domyślnie, pełny audit. Spike 5 testuje próby obejścia.
7. Rdzeń publikuje zdarzenia; kokpit i moduły je subskrybują (dziedziczone z v3).
8. tree-sitter żyje **tylko w rdzeniu** (indeks, kompresja, diff); podświetlanie w UI przez Lezer/Shiki.

## 6. Bezpieczeństwo i ewaluacja

**Granica wykonania i model uprawnień** (fundament, decyzje w Etapie 0):
- Granica bezpieczeństwa = **proces agenta i wszystkie procesy potomne**: dostęp do plików, sieci i sekretów; `term` to kontrolowany interfejs, nie jedyna zapora (testy obejścia w Spike 5).
- allowlista bez pytania (czysto readonly: `rg`, `git status`, `git diff`…),
- wykonanie kodu projektu — osobna klasa: `cargo check`/`build`/`test` uruchamiają `build.rs` i makra proceduralne, więc **nie są readonly**; dopuszczalne tylko w sandbox albo za potwierdzeniem,
- potwierdzenie człowieka (instalacje, mutacje poza worktree, sieć),
- zakaz domyślny (kasowanie poza worktree, sekrety, zmiany konfiguracji IDE).
- Sandbox na Windows: WSL2 vs ograniczenia procesu — spike decyduje.
- Audit: każda komenda + transkrypt agenta tworzą timeline (co, dlaczego, z jakim skutkiem).

**Ewaluacja (Spike 0, przed budową):**
- zestaw 5–10 zadań wzorcowych na własnym repo,
- metryki: skuteczność wykonania, czas, liczba interwencji człowieka, koszt tokenów,
- każdą decyzję narzędziową (skeleton AST, fast-loop, adaptery) mierzymy na tym zestawie; porównujemy z sensownym baseline'em (np. wyszukiwanie na żądanie), nie ze słomianym (wklejanie całych plików).

## 7. Plan działania

**Etap 0: walidacja (spike'i, 2–3 tygodnie)**
- **Spike 0 — ewaluacja:** zadania wzorcowe + metryka + baseline.
- **Spike 1 — adapter Pi (warunek MVP, najwyższy priorytet):** uruchomienie zadania, zdarzenia strukturalne (komunikaty, wywołania narzędzi), anulowanie/steering, zatwierdzenia, zakończenie. Rozstrzyga hipotezę ACP/MCP/PTY i dokumentuje, co Pi faktycznie udostępnia.
  - **Rekonesans (2026-10-11) - rozstrzygnięty:** wszystkie zdolności AVAILABLE poza ACP (ABSENT w Pi) i approvals (PARTIAL → w pełni implementowalne przez rozszerzenie permission-gate + podprotokół `extension_ui_request/response`). Werdykt szwów: interaktywny Pi = **TUI w terminalu (PTY)** zgodnie z zasadą terminal-first (decyzja operatora, 2026-10-11); tryb RPC = zwalidowany, zarezerwowany dla zadań tła; przyszłe agenty = ACP-klient; narzędzia = rmcp. Ryzyko nr 1 na Windows: PTY/ConPTY dla TUI agenta + zarządzanie drzewem procesów (Job Objects). Pozostała część praktyczna (zadanie kodowe dla workera): hosting `pi` TUI w PTY (ConPTY) z pełną interakcją w terminalu, obserwacja sesji JSONL, mini-serwer MCP (rmcp, stdio) oraz weryfikacja, że rozszerzenie permission-gate egzekwuje polityki w sesji TUI; plus weryfikacja kanału wizualnego jako macierzy per agent (Pi: `read` z załącznikami + wstrzyknięcie przez rozszerzenie; Codex: natywne wsparcie obrazów; Claude Code: odczyt obrazów; AGY: do ustalenia) i przepływu Excalidraw → PNG/SVG → spool → rozmowa. Plan awaryjny gdyby ConTTY okazał się niewiarygodny: zewnętrzny terminal systemowy + IDE w roli obserwera (sesja JSONL + git).
- **Spike 2 — transakcje i integralność review:** worktree per zadanie, checkpointy jako commity, kandydat review (id + baza), unieważnienie zgody przy zmianie kandydata, merge względem aktualnej bazy, rollback ze zatrzymaniem pisarza i plikami nieśledzonymi.
- **Spike 3 — pętla naprawcza:** agent edytuje w worktree, `cargo check`/`test` jako bramka; realne czasy i częstotliwość wywołań.
- **Spike 4 — kontekst:** skeleton AST vs wyszukiwanie na żądanie, mierzone na zadaniach ze Spike'a 0.
- **Spike 5 — bezpieczeństwo:** granica wykonania na procesie agenta i potomkach (fs, sieć, sekrety), sandbox Windows (WSL2 vs restricted token), testy obejścia `term` natywnymi narzędziami agenta.

Równoległość agentów celowo **nie** jest walidowana w Etapie 0 — dopiero w Etapie 4, po ustabilizowanym pionowym plastrowym przepływie.

**Etap 1: szkielet kokpitu**
- Tauri 2 + React + dockview: layout kokpitu, streaming JSON-RPC, timeline agenta, panel sterowania.

**Etap 2: ręce (minimalny zestaw dla plastra)**
- Hosting Pi w terminalu (PTY + xterm.js, TUI agenta; steering i zatwierdzenia w terminalu, rozszerzenie permission-gate zarządzane przez IDE); minimalny serwer MCP na `rmcp`: odczyt plików, proste szukanie, `term` z polityką; worktree/checkpointy w `orch`/`git`; obserwacja pracy: tail sesji JSONL. Tryb RPC (pipes) dopiero dla zadań tło w późniejszym etapie. Indeks i LSP jako narzędzia MCP celowo dopiero w Etapie 4 — plaster ma być cienki.

**Etap 3: pionowy plaster end-to-end (MVP)**
- Zadanie → agent w worktree → diff + transkrypt + bramki → approve/merge albo rollback. Działa na zadaniach ze Spike'a 0.

**Etap 4: review dopracowany i pełne zmysły**
- Rozszerzenie MCP o `index`/`lsp` (symbole, diagnostyki, nawigacja), semantyczny diff (difftastic-style), nawigacja LSP w diffie, statusy plików; równoległość: dwa agenty w osobnych worktree, kolejka review, konflikty przy merge.

**Etap 5: skalowanie (opcjonalne)**
- Multi-agent, fast-loop LSP (streaming diagnostyk w trakcie edycji agenta — inny tryb niż narzędzia „na żądanie" z Etapu 4), DAP (agent bada crash/panic), C++ (clangd), ewentualnie własny silnik — tylko jeśli adaptery przestaną wystarczać i ewaluacja to udowodni.

## 8. Projekty referencyjne

| Projekt | Po co nam |
|---|---|
| **Pi + własny ekosystem** | Dogfooding: kokpit dla harnessu, którego używamy codziennie |
| **Zed (ACP, eksperymenty agent-driven)** | Standard ACP + wzorce shadow workspace (fast-loop LSP) |
| **MCP (spec + ekosystem)** | Standard narzędziowy dla agentów |
| **difftastic** | Semantyczny diff na tree-sitter; Rust, możliwy do osadzenia |
| **OpenHands, Devin** | Kontrast: pełne własne silniki — mapa kosztów drogi, której unikamy |
| **Aider, Goose** | Wzorce harnessów agentowych i ewaluacji |
| **Lapce, Helix** | Wzorce techniczne rdzenia (LSP, tree-sitter) — dziedziczone z v3 |

## 9. Do ustalenia

- Co dokładnie Pi udostępnia przez adapter (zdarzenia, sterowanie, zatwierdzenia) — rozstrzyga Spike 1.
- Poziom autonomii: bramki na każdej akcji poza allowlistą (rekomendacja) vs pełne auto z review po fakcie.
- Sandbox Windows: WSL2 vs ograniczenia procesu — rozstrzyga Spike 5.
- Moment włączenia C++ (clangd) i dodatkowych adapterów (claude-code, codex).

## 10. Alternatywy rozważone i odrzucone

| Opcja | Dlaczego nie |
|---|---|
| Własny silnik agenta (`engine/`, ReAct, trait na modele) od MVP | Najdroższa i najszybciej starzejąca się warstwa; konkurujemy z pi/claude-code zamiast je hostować; ReAct przestarzały wobec native tool callingu. Odroczone przez trait narzędzi |
| Shadow workspace w pamięci jako ground truth transakcji | Kompilator i testy pracują na plikach; pełne błędy daje rustc, nie LSP. Worktree + commit zamiast tego; overlay LSP tylko jako fast-loop |
| Bramkowanie „nie kompiluje się = nie zapisuje" per edycja | Zabija zmiany wieloetapowe i chwiejne buildy; bramka na merge/commit |
| Most CM6 ↔ web-tree-sitter | Niepotrzebny, gdy człowiek nie pisze; podświetlanie review przez Lezer/Shiki |
| Subagenci „architekt + koder + tester" na start | Często wypadają gorzej niż jeden agent z dobrymi narzędziami; równoległość dopiero po pionowym plastrem |
| Streaming-diff w dockview jako spike | Rutyna inżynierska, nie ryzyko; w Etapie 0 zastąpiony walidacją adaptera Pi, równoległość dopiero w Etapie 4 |
| Electron + Monaco; fork Code-OSS/Theia; natywne GUI (GPUI/Slint/egui); Kotlin/IntelliJ Platform; C++/Qt | Dziedziczone z v3 — bez zmian |

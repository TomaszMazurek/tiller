# MyIDE — dokument rdzeniowy (core)

> Jedno źródło prawdy dla fundamentu projektu. Profile produktowe (forki) odnoszą się do tego dokumentu i dodają wyłącznie warstwę produktową: UI, kolejność etapów, eksponowane narzędzia.
>
> Status: aktywny · Wersja: 1.0 · Powstał z briefów v1–v4 po pełnej pętli review (2026-10-11)

## 1. Misja

Własne narzędzia programistyczne z prawdziwym rdzeniem: niezależne od shell'a i UI, testowalne bez GUI, z językową inteligencją opartą na LSP i tree-sitter zamiast własnego silnika analizy. Punkt odniesienia jakości: IntelliJ/CLion; architektura: własna. Praca solo, z dogfoodingiem własnego ekosystemu agentowego (Pi).

## 2. Model forków

Jeden rdzeń, wiele profili produktowych. Fork **nie forkuje kodu** — forkuje warstwę produktową. Wszystko wspólne żyje tutaj i zmiana fundamentu wymaga świadomej decyzji zapisanej w §5.

| Profil | Status | Dokument | Produkt |
|---|---|---|---|
| **Agent Edition** | **aktywny** | `PROJECT_BRIEF_v4.md` | kokpit nadzoru + narzędzia dla agentów (mission control) |
| Classic Edition | uśpiony | `PROJECT_BRIEF_v3.md` | klasyczne IDE: edytor, drzewo projektu, zakładki |
| Daemon / PWA | przyszły | — | tryb headless przez `agent-daemon` + WebSocket |

## 3. Fundament techniczny (niezmienny między profilami)

### Rdzeń = crate'y Rust niezależne od Tauri

```
crates/
├─ core/          # model workspace, zdarzenia, TRAIT NARZĘDZI (szew pod wszystko)
├─ index/         # tree-sitter + SQLite + tantivy; kompresja kontekstu (AST skeleton)
├─ lsp/           # klient LSP (diagnostyki, nawigacja read-only)
├─ git/           # worktree, checkpointy, timeline, diff liniowy + semantyczny
├─ term/          # PTY + klasyfikacja komend + audit
├─ rpc/           # własny wąski JSON-RPC + serwer MCP + klient ACP
├─ orch/          # kolejki zadań, alokacja worktree, kolejka review (gł. profil agent)
├─ agents/        # adaptery agentów zewnętrznych (profil agent)
├─ dap/           # klient DAP (późny etap)
└─ agent-daemon/  # tryb headless, WebSocket (profil daemon)
```

### Dwaj klienci rdzenia (równorzędni)

- **Kokpit/UI** — własny, wąski protokół JSON-RPC; transport wymienny (Tauri IPC ↔ WebSocket).
- **Agent** — trzy nierównoważne szwy o różnych rolach:
  - **MCP**: rdzeń jest **serwerem narzędzi** (index, lsp, term), agent klientem;
  - **ACP**: kokpit jest **klientem prezentacji** agenta, agent obsługuje protokół;
  - **PTY**: surowe wejście/wyjście bez strukturalnego sterowania i zatwierdzeń.

### Zasady architektoniczne

1. Rdzeń nie zna Tauri ani UI; frontend rozmawia z rdzeniem wyłącznie przez `rpc`.
2. Wspólny mianownik wszystkich integracji: **trait narzędzi w `core`** — to szew, który odroczył decyzję o własnym silniku agenta bez zamykania do niego drogi.
3. Rdzeń publikuje **zdarzenia** (plik zmieniony, diagnostyki, stan procesu, postęp bramek); klienci subskrybują.
4. **tree-sitter wyłącznie w rdzeniu** (indeks, kompresja kontekstu, diff semantyczny); podświetlanie w UI przez Lezer/Shiki.
5. **Mechanizm transakcyjny jest w rdzeniu** (`git/`): worktree per zadanie, commit = checkpoint, niezmienny kandydat review (id zmiany + commit bazowy), merge względem aktualnej bazy. Egzekwowanie (bramki, zatwierdzenia) należy do profilu.
6. **Granica bezpieczeństwa wykonania = cały proces agenta i jego potomkowie** (pliki, sieć, sekrety). `term` jest kontrolowanym interfejsem, nie jedyną zaporą. `cargo check`/`build`/`test` **nie są readonly** (uruchamiają `build.rs` i makra proceduralne) — osobna klasa: sandbox albo potwierdzenie.

### Stack

| Zadanie | Biblioteka |
|---|---|
| Async, procesy, I/O | `tokio` |
| Bufory (widok, nie ground truth) | `ropey` |
| Parsowanie, indeks, kompresja AST, diff | `tree-sitter` (+ wzorce `difftastic`) |
| Klient LSP | `lsp-types` + własny klient |
| Git: worktree, checkpointy, timeline | `git2` / `gix` |
| PTY z klasyfikacją komend | `portable-pty` + własna warstwa polityk |
| Obserwowanie plików | `notify` |
| Szukanie, indeks symboli | `ignore` + `tantivy` + `rusqlite` |
| CPU | `rayon` |

Warstwa UI profili webowych: TypeScript + React + dockview (+ CodeMirror 6 w trybie read-only, xterm.js, podświetlanie Lezer/Shiki). Shell: Tauri 2.

## 4. Zasady pracy

- **Fundament vs części wymienne**: granice crate'ów, protokoły, model zdarzeń, bezpieczeństwo — projektowane od dnia 1; framework UI, biblioteka dokowania, edytor, transport, serwery LLM — wymienne bez przepisywania.
- **Ewaluacja przed budową**: zestaw zadań wzorcowych na własnym repo + metryka skuteczności (nie metryki pośrednie); każda decyzja narzędziowa porównywana z sensownym baseline'em.
- **TDD domyślnie**: rdzeń niezależny od UI jest projektowany pod testy; zmiana zachowania bez testu nie trafia do main.
- **Pionowy plaster przed rozszerzaniem**: end-to-end zanim poziomo.
- **Recenzje z fresh-context**: autor nie jest jedynym recenzentem własnej pracy.

## 5. Decyzje zamknięte (zmiana = świadomy pivot, zapisany tutaj)

| Decyzja | Uzasadnienie skrócone |
|---|---|
| Rdzeń w Ruście | wydajność LLVM bez GC, bezpieczeństwo pamięci, biblioteki do parsowania i współbieżności |
| Tauri 2 + web UI zamiast natywnego GUI | nie budujemy edytora/drkiew/zakładek/dokowania od zera; powrót tylko jeśli web będzie wąskim gardłem |
| Inteligencja języka = LSP + tree-sitter | własny silnik analizy to lata pracy; std. ekosystem zamiast tego |
| Hostowanie zewnętrznych agentów, nie własny silnik | silnik agentowy = najtrudniejsza, najszybciej starzejąca się warstwa; dogfoodujemy Pi; własny engine odroczony przez trait narzędzi |
| Windows-first, CI trójplatformowe od początku | środowisko pracy; specyfika Windows tania do wykrycia wcześnie |
| Praca solo, granice crate'ów zespołowo-gotowe | mniej abstrakcji teraz, przepisywanie później niepotrzebne |
| PWA odłożona, architektura wspiera za darmo | transport wymienny + agent-daemon |
| Monaco jako plan awaryjny edytora | gdyby customizacja CM6 okazała się droższa niż zakładamy |

## 6. Historia decyzji

| Wersja | Co się stało |
|---|---|
| v1 (2026-10-11) | brief klasycznego IDE: Tauri 2 + Rust + CodeMirror + LSP |
| v2 | recenzja: korekta „UI to nie mały problem", React+dockview, własny protokół, dwa miejsca tree-sitter |
| v3 | synteza: fundament vs części wymienne, Etap 0 walidacyjny, decyzje domknięte |
| v4 | pivot agent-only po 3 opiniach zewnętrznych: kokpit + narzędzia dla agentów, MCP/ACP, transakcje, bezpieczeństwo procesowe, ewaluacja |
| v4+review | pętla: oracle (Astra) → 5 znalezisk → remediacja → rewalidacja (Sol) → DOMKNIĘTE Z UWAGAMI → poprawki spójności |
| **core 1.0** | wyodrębnienie fundamentu wspólnego z v3+v4 jako baza dla forków |

## 7. Spis dokumentów

| Dokument | Rola |
|---|---|
| `PROJECT_CORE.md` | **ten dokument**: fundament, zasady, decyzje zamknięte, model forków |
| `PROJECT_BRIEF_v4.md` | profil Agent Edition — aktywny (etapy, spike'i, UI kokpitu) |
| `PROJECT_BRIEF_v3.md` | profil Classic Edition — uśpiony (etapy klasycznego IDE) |

Iteracje v1–v2 zostały wchłonięte przez §6 i usunięte z katalogu roboczego; pełna treść dostępna w historii gita.

## 8. Otwarte kwestie globalne

- Nazwa projektu (MyIDE tymczasowa).
- Poziom autonomii agentów: bramki vs review po fakcie (profil agent).
- Sandbox na Windows: WSL2 vs ograniczenia procesu (Spike 5 profilu agent).
- Moment włączenia C++ (clangd) i dodatkowych adapterów agentów.

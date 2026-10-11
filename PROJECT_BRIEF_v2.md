# IDE w stylu IntelliJ / CLion: brief projektu i stack

## 1. Cel projektu

Własne IDE w stylu IntelliJ/CLion: panele narzędziowe, drzewo projektu, zakładki, wyszukiwarka, podpowiedzi, nawigacja po kodzie, build, debugowanie i terminal.

**Kluczowa obserwacja:** najtrudniejsze jest wsparcie dla języków (parsowanie, indeksowanie, nawigacja, refaktoryzacje). Pisanie własnego silnika analizy kodu w stylu JetBrains od zera to praca na lata, więc inteligencję języka opieramy na gotowych serwerach LSP i tree-sitter.

**Korekta:** UI nie jest „małym" problemem. Sam edytor dostajemy z CodeMirror, ale to, co daje „feeling IntelliJ" (dokowanie paneli z resize i zagnieżdżaniem, zapis layoutów, zakładki z drag&drop, tool windows, system keymapów), to miesiące pracy. Dlatego wybór biblioteki dokowania poprzedza wybór frameworka UI.

## 2. Podjęte decyzje

| Obszar | Decyzja |
|---|---|
| Język rdzenia | **Rust** |
| Shell aplikacji | **Tauri 2** |
| Frontend | **TypeScript** + **React** |
| Dokowanie paneli | **dockview** (`dockview-react`; `dockview-core` jest framework-agnostic) |
| Edytor | **CodeMirror 6** |
| Inteligencja języka | **LSP** (np. clangd, rust-analyzer) + **tree-sitter** |
| Debugowanie | **DAP** (lldb-dap / codelldb) |
| Dystrybucja | Aplikacja desktopowa; później opcjonalnie PWA z lokalnym lub zdalnym backendem |

### Uzasadnienie

- **Rust vs C++:** wydajność jest porównywalna (oba przez LLVM, bez GC). O szybkości IDE decyduje architektura (przyrostowe parsowanie, indeksowanie w tle, wielowątkowość, renderowanie), a nie język. Rust daje bezpieczeństwo pamięci i dobre biblioteki do parsowania i współbieżności.
- **Tauri + web frontend zamiast natywnego GUI (GPUI/Slint/egui):** natywne GUI w Ruście dałoby lepsze renderowanie, ale trzeba by samemu zbudować edytor, drzewa, zakładki i dokowanie paneli. GPUI ma niestabilne API, egui i Slint nie są stworzone pod tak złożone IDE. Wracamy do tej opcji tylko wtedy, gdy webowy frontend okaże się wąskim gardłem.
- **Tauri zamiast Electrona:** lżejsze, backend w Ruście, mniejsze zużycie pamięci.
- **React + dockview zamiast SolidJS:** SolidJS jest lżejszy, ale przy IDE-owym chrome trzeba by samemu pisać widgety. React ma najwięcej gotowych komponentów (dockview-react, wrappery xterm). Szybkość dojścia do „wygląda jak JetBrains" wygrywa z wydajnością frameworka, który i tak nie będzie wąskim gardłem w tej skali.
- **CodeMirror 6 zamiast Monaco:** lżejszy i łatwiejszy do głębokiej customizacji. Monaco jest bliżej VS Code i ma więcej gotowych funkcji, więc pozostaje rozsądną alternatywą.
- **Czemu nie czyste PWA:** przeglądarka nie uruchomi lokalnych procesów (clangd, kompilator, CMake, debugger, terminal). PWA ma sens tylko z backendem zdalnym albo lokalnym agentem.

## 3. Stack

### Rdzeń (Rust)

| Zadanie | Biblioteka |
|---|---|
| Async, procesy, I/O | `tokio` |
| Parsowanie przyrostowe, podświetlanie | `tree-sitter` |
| Klient LSP | `lsp-types` + własny klient (`tower-lsp` tylko jeśli piszemy serwery) |
| Debugowanie | DAP przez `lldb-dap` / `codelldb` |
| Terminal (PTY) | `portable-pty` |
| Obserwowanie plików | `notify` |
| Przeszukiwanie projektu | `ignore` + `tantivy` |
| Indeks symboli, metadane | `rusqlite` (SQLite) |
| Git | `git2` lub `gix` |
| Współbieżność CPU | `rayon` |

### Frontend (TypeScript)

- **CodeMirror 6**: edytor
- **React + dockview**: dokowanie paneli, tool windows, zakładki, drzewo projektu
- **xterm.js**: terminal

## 4. Architektura

```
┌─────────────────────────────┐
│  Frontend (TS, CodeMirror)  │
└──────────────┬──────────────┘
               │ JSON-RPC (transport wymienny)
     ┌─────────┴──────────┐
     │ Tauri IPC          │ WebSocket (localhost / zdalny)
     └─────────┬──────────┘
┌──────────────┴──────────────┐
│  Rdzeń: osobne crate'y Rust │
│  (niezależne od Tauri)      │
│  indeks · LSP · DAP · PTY   │
│  fs-watch · git · search    │
└─────────────────────────────┘
```

**Zasady:**
1. Rdzeń w osobnych crate'ach, **niezależnych od Tauri**.
2. Komunikacja frontend ↔ rdzeń przez **JSON-RPC**, z wymiennym transportem (Tauri IPC lub WebSocket).
3. Frontend nie wie, czy rozmawia z Tauri, czy z lokalnym agentem. Ten sam kod może działać jako desktop i później jako PWA.
4. **Własny, wąski protokół frontend ↔ rdzeń** (open file, diagnostics, completion itd.), a nie surowy passthrough LSP. Inaczej zmiana we froncie wymaga znajomości trzech protokołów naraz (frontend → JSON-RPC → rdzeń → LSP → clangd). Wyjątek: rzeczy naturalnie per-language-server mogą iść jako półprzezroczysty passthrough.
5. **tree-sitter działa w dwóch miejscach, do dwóch celów:**
   - **UI:** `web-tree-sitter` (WASM) do natychmiastowego podświetlania w edytorze. CM6 natywnie używa Lezera, więc tree-sitter w UI to świadoma decyzja, żeby nie pisać gramatyk dwa razy.
   - **Rdzeń:** natywny tree-sitter + SQLite do indeksowania, symboli i wyszukiwania.
   - Wymóg: te same gramatyki, ta sama wersja, wspólne pliki query (highlights itd.).

Sugerowany podział workspace:

```
ide/
├─ crates/
│  ├─ core/        # model projektu, workspace, zdarzenia
│  ├─ index/       # tree-sitter, SQLite, tantivy
│  ├─ lsp/         # klient LSP
│  ├─ dap/         # klient DAP
│  ├─ term/        # PTY
│  ├─ rpc/         # JSON-RPC, transporty
│  └─ agent/       # standalone daemon (WebSocket)
├─ src-tauri/      # cienka warstwa Tauri
└─ ui/             # frontend TS
```

## 5. PWA: możliwości i ograniczenia

- **Działa:** edytor, tree-sitter w WASM, całe UI, offline (service worker), dostęp do plików przez File System Access API (tylko Chromium; w innych przeglądarkach OPFS).
- **Nie działa lokalnie:** uruchamianie clangd, kompilatora, CMake, debuggera, terminala.
- **Obejścia:**
  1. Backend zdalny (model Codespaces / vscode.dev).
  2. Lokalny agent (daemon w Ruście wystawiający LSP/DAP/terminal przez localhost WebSocket). Nasz crate `agent` to realizuje.
  3. WASM: ograniczone, niewystarczające dla prawdziwych projektów C++.

## 6. Plan działania (MVP)

**Etap 1: szkielet**
- Workspace Cargo + Tauri 2 + frontend TS (React)
- Dockview: podstawowy layout z panelami, zakładkami i tool windows (od razu, bo to decyduje o odczuciu IDE)
- JSON-RPC między frontendem a rdzeniem, z własnym wąskim API
- Otwieranie folderu, drzewo plików, zakładki, zapis plików

**Etap 2: edytor**
- CodeMirror 6 + podświetlanie przez tree-sitter
- Obserwowanie zmian w plikach (`notify`)

**Etap 3: inteligencja języka**
- Klient LSP do rust-analyzer jako język nr 1 (najlepiej zachowujący się serwer LSP w ekosystemie; dogfooding: IDE rośnie szybciej, gdy sami w nim pracujemy od etapu 2), clangd jako język nr 2: diagnostyki, autouzupełnianie, go-to-definition, hover, find references

**Etap 4: narzędzia**
- Terminal (portable-pty + xterm.js)
- Szukanie w projekcie (ignore + tantivy), indeks symboli (SQLite)
- Git

**Etap 5: build i debug**
- Integracja z CMake / Cargo, uruchamianie
- DAP (lldb-dap), breakpointy, podgląd zmiennych
- Uwaga: debugowanie na Windows to najsłabsze ogniwo planu. Dla C++ z toolchainem MSVC lldb-dap/codelldb działa, ale bywa wtórne wobec Visual Studio. Dla Rusta jest w porządku. Etap zostaje, ale z realistycznymi oczekiwaniami.

**Etap 6 (opcjonalnie)**
- Refaktoryzacje, dokowanie paneli w stylu JetBrains, agent + wersja PWA

## 7. Do ustalenia

- Docelowy język/języki wspierane przez IDE. Propozycja: Rust jako język nr 1 w MVP, C++ jako drugi. Do potwierdzenia, czy docelowo ma to być odpowiednik CLion (C++), czy uniwersalne IDE
- Szczegóły podziału tree-sitter między UI a rdzeń (wersje gramatyk, wspólne pliki query)
- Projekt solo czy zespołowy?
- Docelowe platformy (Windows / Linux / macOS)
- Wersja PWA/zdalna: czy w ogóle potrzebna, i kiedy

## 8. Alternatywy rozważone i odrzucone (na teraz)

| Opcja | Dlaczego nie |
|---|---|
| Kotlin na IntelliJ Platform | Najbliżej „prawdziwego" IntelliJ, ale duża złożoność API i uzależnienie od JetBrains |
| C++ + Qt (jak Qt Creator) | Dobry wybór, ale wolniejszy rozwój i mniej bezpieczny język |
| Electron + Monaco | Szybki start, ale cięższy niż Tauri |
| Fork Code-OSS / Eclipse Theia | Szybka droga do funkcjonalności, ale mniej własnej kontroli nad architekturą |
| Natywny Rust GUI (GPUI/Slint/egui) | Za dużo widgetów do zbudowania od zera; opcja na później, jeśli web UI będzie wąskim gardłem |

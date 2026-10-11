# IDE w stylu IntelliJ / CLion: brief projektu i stack

## 1. Cel projektu

Własne IDE w stylu IntelliJ/CLion: panele narzędziowe, drzewo projektu, zakładki, wyszukiwarka, podpowiedzi, nawigacja po kodzie, build, debugowanie i terminal.

**Kluczowa obserwacja:** UI jest mniejszym problemem. Najtrudniejsze jest wsparcie dla języków (parsowanie, indeksowanie, nawigacja, refaktoryzacje). Pisanie własnego silnika analizy kodu w stylu JetBrains od zera to praca na lata, więc inteligencję języka opieramy na gotowych serwerach LSP i tree-sitter.

## 2. Podjęte decyzje

| Obszar | Decyzja |
|---|---|
| Język rdzenia | **Rust** |
| Shell aplikacji | **Tauri 2** |
| Frontend | **TypeScript** (web UI) |
| Edytor | **CodeMirror 6** |
| Inteligencja języka | **LSP** (np. clangd, rust-analyzer) + **tree-sitter** |
| Debugowanie | **DAP** (lldb-dap / codelldb) |
| Dystrybucja | Aplikacja desktopowa; później opcjonalnie PWA z lokalnym lub zdalnym backendem |

### Uzasadnienie

- **Rust vs C++:** wydajność jest porównywalna (oba przez LLVM, bez GC). O szybkości IDE decyduje architektura (przyrostowe parsowanie, indeksowanie w tle, wielowątkowość, renderowanie), a nie język. Rust daje bezpieczeństwo pamięci i dobre biblioteki do parsowania i współbieżności.
- **Tauri + web frontend zamiast natywnego GUI (GPUI/Slint/egui):** natywne GUI w Ruście dałoby lepsze renderowanie, ale trzeba by samemu zbudować edytor, drzewa, zakładki i dokowanie paneli. GPUI ma niestabilne API, egui i Slint nie są stworzone pod tak złożone IDE. Wracamy do tej opcji tylko wtedy, gdy webowy frontend okaże się wąskim gardłem.
- **Tauri zamiast Electrona:** lżejsze, backend w Ruście, mniejsze zużycie pamięci.
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
- **SolidJS** (lub React): panele, drzewo projektu, zakładki
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
- Workspace Cargo + Tauri 2 + frontend TS
- JSON-RPC między frontendem a rdzeniem
- Otwieranie folderu, drzewo plików, zakładki, zapis plików

**Etap 2: edytor**
- CodeMirror 6 + podświetlanie przez tree-sitter
- Obserwowanie zmian w plikach (`notify`)

**Etap 3: inteligencja języka**
- Klient LSP do clangd (lub rust-analyzer): diagnostyki, autouzupełnianie, go-to-definition, hover, find references

**Etap 4: narzędzia**
- Terminal (portable-pty + xterm.js)
- Szukanie w projekcie (ignore + tantivy), indeks symboli (SQLite)
- Git

**Etap 5: build i debug**
- Integracja z CMake / Cargo, uruchamianie
- DAP (lldb-dap), breakpointy, podgląd zmiennych

**Etap 6 (opcjonalnie)**
- Refaktoryzacje, dokowanie paneli w stylu JetBrains, agent + wersja PWA

## 7. Do ustalenia

- Docelowy język/języki wspierane przez IDE (np. tylko C++ jak CLion, czy uniwersalne?)
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

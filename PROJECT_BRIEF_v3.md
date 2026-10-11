# IDE w stylu IntelliJ / CLion: brief projektu i stack (v3)

> Fork produktowy profilu **Classic Edition** (uśpiony). Fundament wspólny, zasady pracy i decyzje zamknięte: **`PROJECT_CORE.md`**.

## 1. Cel projektu

Własne IDE w stylu IntelliJ/CLion: panele narzędziowe, drzewo projektu, zakładki, wyszukiwarka, podpowiedzi, nawigacja po kodzie, build, debugowanie i terminal.

**Kluczowa obserwacja:** najtrudniejsze jest wsparcie dla języków (parsowanie, indeksowanie, nawigacja, refaktoryzacje). Pisanie własnego silnika analizy kodu w stylu JetBrains od zera to praca na lata, więc inteligencję języka opieramy na gotowych serwerach LSP i tree-sitter.

**Korekta:** UI nie jest „małym" problemem. Sam edytor dostajemy z CodeMirror, ale to, co daje „feeling IntelliJ" (dokowanie paneli z resize i zagnieżdżaniem, zapis layoutów, zakładki z drag&drop, tool windows, system keymapów), to miesiące pracy. Dlatego wybór biblioteki dokowania poprzedza wybór frameworka UI.

## 2. Koncepcja: fundament i części wymienne

Rozdzielamy decyzje nieodwracalne (fundament) od odwracalnych (części wymienne). Fundament musi być dobrze zaprojektowany od dnia 1, bo koszt jego zmiany rośnie z każdym etapem; części wymienne możemy podmienić później bez przepisywania projektu.

**Fundament:**
- granice crate'ów rdzenia i ich odpowiedzialność (`core`/`index`/`lsp`/`dap`/`term`/`rpc`/`agent`),
- własny, wąski protokół frontend ↔ rdzeń (JSON-RPC) i model zdarzeń publikowanych przez rdzeń,
- zasada: rdzeń nie zna Tauri ani UI; frontend komunikuje się z rdzeniem wyłącznie przez protokół `rpc`.

**Części wymienne:**
- framework UI (React), biblioteka dokowania (dockview), edytor (CodeMirror 6), terminal (xterm.js),
- transport (Tauri IPC ↔ WebSocket),
- serwery językowe (rust-analyzer, clangd, przyszłe).

## 3. Podjęte decyzje

| Obszar | Decyzja |
|---|---|
| Język rdzenia | **Rust** |
| Shell aplikacji | **Tauri 2** |
| Frontend | **TypeScript** + **React** |
| Dokowanie paneli | **dockview** (`dockview-react`; `dockview-core` jest framework-agnostic) |
| Edytor | **CodeMirror 6** (Monaco jako plan awaryjny) |
| Inteligencja języka | **LSP** (rust-analyzer, clangd) + **tree-sitter** |
| Debugowanie | **DAP** (lldb-dap / codelldb) |
| Języki wspierane | **Rust jako nr 1 w MVP, C++ jako nr 2**; architektura uniwersalna przez LSP |
| Platformy | **Windows-first**; CI od początku na Windows/Linux/macOS |
| Tryb pracy | **Solo**, z granicami crate'ów gotowymi na przyszły zespół |
| Dystrybucja | Aplikacja desktopowa; PWA odłożona — architektura wspiera ją za darmo |

### Uzasadnienie

- **Rust vs C++:** wydajność jest porównywalna (oba przez LLVM, bez GC). O szybkości IDE decyduje architektura (przyrostowe parsowanie, indeksowanie w tle, wielowątkowość, renderowanie), a nie język. Rust daje bezpieczeństwo pamięci i dobre biblioteki do parsowania i współbieżności.
- **Tauri + web frontend zamiast natywnego GUI (GPUI/Slint/egui):** natywne GUI w Ruście dałoby lepsze renderowanie, ale trzeba by samemu zbudować edytor, drzewa, zakładki i dokowanie paneli. GPUI ma niestabilne API, egui i Slint nie są stworzone pod tak złożone IDE. Wracamy do tej opcji tylko wtedy, gdy webowy frontend okaże się wąskim gardłem.
- **Tauri zamiast Electrona:** lżejsze, backend w Ruście, mniejsze zużycie pamięci.
- **React + dockview zamiast SolidJS:** SolidJS jest lżejszy, ale przy IDE-owym chrome trzeba by samemu pisać widgety. React ma najwięcej gotowych komponentów (dockview-react, wrappery xterm). Szybkość dojścia do „wygląda jak JetBrains" wygrywa z wydajnością frameworka, który i tak nie będzie wąskim gardłem w tej skali.
- **CodeMirror 6 zamiast Monaco:** lżejszy i łatwiejszy do głębokiej customizacji. Monaco jest bliżej VS Code i ma więcej gotowych funkcji, dlatego zostaje rozsądnym planem awaryjnym, gdyby customizacja CM6 okazała się droższa niż zakładamy.
- **Rust jako język nr 1:** dogfooding — IDE, w którym sami pracujemy od wczesnego etapu, rośnie szybciej; rust-analyzer to najlepiej zachowujący się serwer LSP w ekosystemie. C++ (clangd) jako drugi język potwierdza uniwersalność architektury i jest naturalnym celem, skoro punktem odniesienia jest CLion.
- **Windows-first:** to nasze środowisko pracy. CI trójplatformowe w Ruście jest tanie, a specyfika Windows (ConPTY, MSVC, ścieżki) uwiera najmniej, gdy testuje się ją od początku.
- **Solo, ale z granicami:** w trybie solo mniej abstrakcji i szybciej do MVP; stabilne API między crate'ami zostaje, więc ewentualne dołączenie osób później nie wymaga przepisywania.
- **Czemu nie czyste PWA:** przeglądarka nie uruchomi lokalnych procesów (clangd, kompilator, CMake, debugger, terminal). PWA ma sens tylko z backendem zdalnym albo lokalnym agentem.

## 4. Stack

### Rdzeń (Rust)

| Zadanie | Biblioteka |
|---|---|
| Async, procesy, I/O | `tokio` |
| Parsowanie przyrostowe | `tree-sitter` |
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
- **Most CM6 ↔ tree-sitter (własny kod):** nie ma oficjalnej integracji — piszemy własne rozszerzenie CM6 (`Language`) napędzane przyrostowym parserem `web-tree-sitter` (WASM). To znane, świadome ryzyko projektu: realnie tygodnie pracy, rozstrzygnięte spike'em w Etapie 0.
- **React + dockview**: dokowanie paneli, tool windows, zakładki, drzewo projektu
- **xterm.js**: terminal

## 5. Architektura

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
6. **Rdzeń publikuje zdarzenia** (plik zmieniony, diagnostyki, postęp indeksowania, stan serwera językowego); frontend subskrybuje je przez protokół. To zdarzeniowy szkielet daje „żywość" IDE bez ścisłego sprzężenia modułów.

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

## 6. PWA: możliwości i ograniczenia

- **Działa:** edytor, tree-sitter w WASM, całe UI, offline (service worker), dostęp do plików przez File System Access API (tylko Chromium; w innych przeglądarkach OPFS).
- **Nie działa lokalnie:** uruchamianie clangd, kompilatora, CMake, debuggera, terminala.
- **Obejścia:**
  1. Backend zdalny (model Codespaces / vscode.dev).
  2. Lokalny agent (daemon w Ruście wystawiający LSP/DAP/terminal przez localhost WebSocket). Nasz crate `agent` to realizuje.
  3. WASM: ograniczone, niewystarczające dla prawdziwych projektów C++.

## 7. Plan działania (MVP)

**Etap 0: walidacja techniczna (spike'i, łącznie 1–2 tygodnie)**

Każdy spike to samodzielny mini-projekt z wnioskiem: „nadaje się / pułapki / obejście". Żaden nie idzie do produkcji — wszystkie rozstrzygają ryzyka fundamentu.

- **Tauri 2 IPC:** JSON-RPC + streaming zdarzeń (Channel), przepustowość dla diagnostyk i podglądu plików.
- **dockview-react:** layout IDE (panel boczny, edytor, tool windows), persystencja layoutu.
- **portable-pty (ConPTY) + xterm.js:** działający terminal na Windows.
- **Most CM6 ↔ web-tree-sitter:** minimalne przyrostowe podświetlanie jednego języka — największe pojedyncze ryzyko projektu.
- **Minimalny klient LSP:** pełny cykl życia + diagnostyki z rust-analyzera — potwierdza, że własny klient (zamiast gotowca) jest wykonalny.

**Etap 1: szkielet**
- Workspace Cargo + Tauri 2 + frontend TS (React)
- Dockview: podstawowy layout z panelami, zakładkami i tool windows (od razu, bo to decyduje o odczuciu IDE)
- JSON-RPC między frontendem a rdzeniem, z własnym wąskim API
- Otwieranie folderu, drzewo plików, zakładki, zapis plików

**Etap 2: edytor**
- CodeMirror 6 + most tree-sitter (produkt spike'u z Etapu 0): podświetlanie, inkrementalne aktualizacje
- Obserwowanie zmian w plikach (`notify`)

**Etap 3: inteligencja języka**
- Klient LSP do rust-analyzer (język nr 1, dogfooding): diagnostyki, autouzupełnianie, go-to-definition, hover, find references
- clangd jako język nr 2 — potwierdzenie uniwersalności

**Etap 4: narzędzia**
- Terminal (portable-pty + xterm.js)
- Szukanie w projekcie (ignore + tantivy), indeks symboli (SQLite)
- Git

**Etap 5: build i debug**
- Integracja z CMake / Cargo, uruchamianie
- DAP (lldb-dap), breakpointy, podgląd zmiennych
- Uwaga: debugowanie na Windows to najsłabsze ogniwo planu. Dla C++ z toolchainem MSVC lldb-dap/codelldb działa, ale bywa wtórne wobec Visual Studio. Dla Rusta jest w porządku. Etap zostaje, ale z realistycznymi oczekiwaniami.

**Etap 6 (opcjonalnie)**
- Refaktoryzacje (LSP code actions)
- Perspektywy i zaawansowane zapisy layoutów w stylu JetBrains (podstawowe dokowanie jest już w Etapie 1)
- System keymapów w stylu JetBrains
- Agent + wersja PWA

## 8. Projekty referencyjne

| Projekt | Po co nam |
|---|---|
| **Lapce** | Najbliższy architektonicznie (Rust, tree-sitter, LSP). Historia ich problemów z tree-sitter w WASM to nasza lista znanych pułapek. |
| **Helix** | Czytelny, produkcyjny klient LSP w Ruście — wzorzec implementacji dla `crates/lsp`. |
| **Zed** | Referencja skalowania: indeksowanie, współbieżność, architektura rdzenia (GUI na GPUI, ale wzorce przenośne). |
| **dockview** | Dokumentacja i przykłady; potwierdzenie wyboru warstwy dokowania. |

## 9. Do ustalenia

- Szczegóły współdzielenia gramatyk tree-sitter między UI a rdzeniem (wersje gramatyk, wspólne pliki query) — rozstrzyga Etap 0/2.
- Moment włączenia C++ (clangd) — po ustabilizowaniu dogfoodingu na Ruście.

## 10. Alternatywy rozważone i odrzucone (na teraz)

| Opcja | Dlaczego nie |
|---|---|
| Kotlin na IntelliJ Platform | Najbliżej „prawdziwego" IntelliJ, ale duża złożoność API i uzależnienie od JetBrains |
| C++ + Qt (jak Qt Creator) | Dobry wybór, ale wolniejszy rozwój i mniej bezpieczny język |
| Electron + Monaco | Szybki start, ale cięższy niż Tauri |
| Fork Code-OSS / Eclipse Theia | Szybka droga do funkcjonalności, ale mniej własnej kontroli nad architekturą |
| Natywny Rust GUI (GPUI/Slint/egui) | Za dużo widgetów do zbudowania od zera; opcja na później, jeśli web UI będzie wąskim gardłem |

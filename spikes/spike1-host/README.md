# Spike 1 — Host: `pi` w PTY + obserwacja sesji (spike praktyczny)

> **Status: UKOŃCZONY — kryteria A–E potwierdzone** (spike praktyczny; dowody w `.pi-run/evidence/`).
> **Zastrzeżenie dowodowe:** automatyczny driver działa na poziomie **WebSocket (bez przeglądarki)** — A1/A2 są
> dowiedzione na poziomie WS/PTY; renderowanie w xterm.js to **obserwacja ręczna operatora**, nie dowód zautomatyzowany.
> Pełna lista zastrzeżeń: sekcja **„Znane ograniczenia dowodów”** na końcu pliku.
> Repozytorium: `C:/Repos/MyIDE`, wszystko w `spikes/spike1-host/`. Host = Windows + ConPTY.

## Cel

Praktycznie zweryfikować szwy integracji "embedded terminal host" dla agenta `pi`:

- **A** — host osadza TUI `pi` w ConPTY, mostkuje na WebSocket, renderuje w xterm.js (przeglądarka), resize nie psuje renderowania
  *(dowód: poziom WS/PTY — keystrokes, odpowiedź przez session-tail, resize PTY; render w przeglądarce = obserwacja ręczna).*
- **B** — obserwacja sesji: tail na live JSONL `pi` → ustrukturyzowane eventy (`message_end`, `tool_execution_*`) na żywo.
- **C** — MCP: `pi` woła `spike1_ping` (serwer rmcp stdio z `.pi/mcp.json`) → wywołanie widoczne w JSONL sesji, wynik wraca do agenta.
- **D** — bramka uprawnień: rozszerzenie `.pi/extensions/spike1-gate.js` blokuje komendę bash zawierającą `spike1-deny-me` z widocznym powodem.
- **E** — best-effort: `pi` czyta PNG/SVG ze `spool/` przez narzędzie `read` (read załącza obrazy) → PASS tylko jeśli agent odnosi się do treści.

## Tabela kryteriów (aktualizowana na bieżąco)

| Kryterium | Status | Czas | Dowód |
|---|---|---|---|
| A1: keystrokes→pi, odpowiedź zaobserwowana | ✅ PASS (poziom WS) | Enter→reply 2.3 s; pisanie→reply 2.9 s; start drivera→reply 8.4 s | e2e: `ev#2 message_end assistant stopReason=stop "SPIKE1-READY"`; echo promptu zweryfikowane w PTY; sanity-probe: odpowiedź w PTY po 2.2 s. Render w xterm.js = obserwacja ręczna |
| A2: resize mid-session, zaakceptowany przez PTY (ctl:ok), sesja przeżywa | ✅ PASS (poziom PTY) | resize w +3.0 s | resize 100x30 → `pty_master.resize()`, potwierdzony ctl-ackiem `{"t":"ctl","ok":true}` asertowanym przez drivera; wymiany B/C/D po resize z potwierdzonymi markerami (asercja ctl:ok dodana po review R2 — patrz „Znane ograniczenia dowodów” pkt 5). „Render w przeglądarce przeżył resize” — NIE twierdzone (obserwacja ręczna) |
| B1: `message_end` eventy live | ✅ PASS | eventy w +6.2…+24.8 s | e2e log: ev#1…ev#17 (user+assistant), host.stdout.log |
| B2: `tool_execution_start/end` eventy live | ✅ PASS | — | ev#5/6 (bash env), ev#10/11 (mcp ping), ev#15/16 (gate block) |
| B3: `PI_SESSION_FILE` widoczne w env dziecka (tool) | ✅ PASS | 5.5 s | ev#6: `PI_SESSION_FILE=…\.pi-run\sessions\…jsonl` wyechowane przez bash-tool |
| C1: `spike1_ping` przez MCP — wynik wraca | ✅ PASS | 4.4 s | ev#11: `mcp__spike1_tools__spike1_ping` → `{"pong":true,…,"spike":"spike1-host"}`; agent powtórzył wynik |
| D1: `spike1-deny-me` zablokowane z powodem | ✅ PASS | 4.7 s | ev#16: `[spike1-gate] blocked by spike1 test rule…`; agent: "blocked by the spike1 test rule" |
| E1: PNG — agent odnosi się do treści obrazu | ✅ PASS | ~8 s | osobny driver `e-vision.mjs` + pełny tekst w JSONL sesji: "black-bordered square… red left… blue right… white circle" (gpt-6.1-sol ma wizję) |
| E2: SVG — agent nazywa trzy boksy diagramu | ✅ PASS | j.w. | j.w. (JSONL): "three boxes are **BROWSER (xterm.js)**, **RUST HOST (WS + PTY)**, **PI (TUI)**"; `e-vision-result.json` ma `svg.pass=false` — caveat harnessu, patrz „Znane ograniczenia dowodów” |
| (pre) smoke: ConPTY DSR + echo roundtrip | ✅ PASS (poprzedni etap, commit 5ad5164) | — | `examples/pty_smoke.rs` |

## Jak uruchomić (instrukcje)

```bash
cd spikes/spike1-host
cargo build                                   # build incrementalny; binarki: spike1-host, spike1-mcp
# JEDNORAZOWO — sandbox stanu pi (zamiast globalnego ~/.pi/agent; NIE modyfikuje globalnego configu):
#   1) skopiuj z ~/.pi/agent do .pi-run/agent-home/: auth.json, models.json, models-store.json
#      (host utworzy katalog i ustawi PI_CODING_AGENT_DIR sam; settings.json minimalny: defaultModel)
#   2) trwałe zaufanie projektu (dla MCP z .pi/mcp.json): uruchom host z .pi-run/start-host-notrust.ps1
#      (pi bez -a), w TUI wybierz "Trust" (przez przeglądarkę xterm.js albo sonda WS);
#      zapis ląduje w .pi-run/agent-home/trust.json — od tej pory startuj normalnie.
./target/debug/spike1-host.exe                # spawnuje pi w PTY + HTTP 8791 + WS 8792
# w przeglądarce: http://127.0.0.1:8791/      # xterm.js podłączony do mostka WS
# alternatywnie zamiast pi: --prog "node -e setInterval(()=>console.log('tick'),1000)"
```

Automatyczny driver e2e (kryteria A–E przez WS, bez przeglądarki — symuluje terminal headless,
z parzystością xterm.js: hello+resize na open, odpowiedź na DSR, pisanie z weryfikacją echa):

```bash
node scripts/sanity-probe.mjs                # szybka sonda: 1 wymiana + eventy (A/B smoke)
node scripts/e2e-demo.mjs                    # A1/A2, B1–B3, C1, D1 (E ma osobny driver)
node scripts/e-vision.mjs                    # E1/E2 (pisanie porcjowe — patrz pułapka 9)
node scripts/probe-screen.mjs "" "MCP" 6000  # podgląd ekranu TUI / kontekst słowa kluczowego
node scripts/inspect-tui.mjs <capture.bin>   # striptowanie ANSI z surowego zapisu PTY
npm test                                      # testy jednostkowe drivera (node --test)
```

## Granica sandboxa: `PI_CODING_AGENT_DIR` (izolacja STANU, nie bariera dostępu)

- Domyślnie host ustawia pi `PI_CODING_AGENT_DIR` → `.pi-run/agent-home` (gitignored). To **izolacja stanu**
  (zaufanie projektowe, zatwierdzenia serwerów MCP, ustawienia, katalog modeli) od globalnego `~/.pi/agent` —
  spike nie pisze do globalnego configu. **To NIE jest bariera dostępu do plików**: pi działa z normalnymi
  uprawnieniami użytkownika hosta i widzi te same pliki co bez tej zmiennej (stan isolated, dostęp nie).
- `SPIKE1_AGENT_HOME=0` **świadomie** przełącza na globalny katalog pi (`~/.pi/agent`) — tryb wyjątku
  (np. porównanie konfiguracji); wtedy trust/ustawienia lądują globalnie.
- **Higiena:** w `.pi-run/agent-home` żyje kopia `auth.json` (dane uwierzytelniające). Katalog jest gitignored,
  ale **nigdy nie commitować ani nie archiwizować** tej kopii (również przy pakowaniu `evidence/`/`.pi-run`
  do archiwum).

Dostępne binarki/obiekty:

- `target/debug/spike1-host.exe` — host (PTY↔WS, HTTP static, tail JSONL→eventy).
- `target/debug/spike1-mcp.exe` — serwer MCP stdio (rmcp): `spike1_ping`, `spike1_list_spool`; dumpuje env do `.pi-run/mcp-env.json`.
- `.pi/mcp.json` — konfiguracja MCP (host przepisuje na ścieżki absolutne przy starcie).
- `.pi/extensions/spike1-gate.js` — bramka uprawnień (blokada `spike1-deny-me`; log do `.pi-run/gate-blocks.jsonl`).
- `src/tail.rs` + `src/events.rs` — watcher katalogu sesji → parsowanie JSONL → eventy.
- `src/dsr.rs` — reguła własności sondy DSR (kto odpowiada: host vs podłączony emulator; testy jednostkowe, w tym scenariusz split+attach-between-reads).
- `examples/pty_smoke.rs` — smoke ConPTY (DSR, echo).
- `spool/` — artefakty dla kryterium E (`spike1-icon.png`, `spike1-diagram.svg`).
- `.pi-run/` — logi runtime (gitignored): `host*.log`, `e2e-result.json`, `gate-blocks.jsonl`, `mcp-env.json`, `sessions/*.jsonl`.

## Dowody per kryterium

Wszystkie logi w `.pi-run/evidence/` (gitignored, na dysku). Model w finałowych biegach: `(openai-codex) gpt-6.1-sol • medium`
(sandbox `agent-home` zmienił domyślny model względem globalnego glm-5.3 — oba działają).

### A — hosting TUI w PTY + mostek WS (PASS na poziomie WS/PTY; render w xterm.js = obserwacja ręczna)

- `cargo build` → `./target/debug/spike1-host.exe` → spawn `pi` w ConPTY; `http://127.0.0.1:8791/` (xterm.js) ↔ `ws://127.0.0.1:8792`.
- Co dowodzi **automatyczny driver** (`e2e-demo.mjs`, klient WS z parzystością xterm.js: hello+resize na open,
  odpowiedź na DSR, pisanie porcjowane z weryfikacją echa):
  - keystrokes przez WS dochodzą do pi — echo promptu w strumieniu PTY jest **weryfikowane przed Enterem**
    (pętla retry z Ctrl+U);
  - odpowiedź asystenta zaobserwowana jako autorytatywny event session-tail: `ev#1 message_end user "Reply with…"
    (+6.2 s)`, `ev#2 message_end assistant stopReason=stop "SPIKE1-READY"` (+8.4 s od startu drivera; 2.3 s od Enter);
  - resize 100×30 mid-session (+3.0 s) **zaakceptowany na poziomie PTY** (`pty_master.resize()`;
    od R2 host potwierdza to ctl-ackiem `{"t":"ctl","ok":true}` asertowanym przez drivera — patrz
    „Znane ograniczenia dowodów” pkt 5) — sesja przeżyła: wymiany B/C/D po resize zakończyły się
    pełnym roundtripem z potwierdzonymi markerami.
- **Renderowanie w przeglądarce (xterm.js) — obserwacja ręczna operatora** (strona pod 8791 rysuje TUI pi, resize
  działa z ręki, licznik eventów rośnie). Driver nie otwiera przeglądarki, więc to **nie jest dowód zautomatyzowany**;
  warstwę wizualną xterm.js trzeba zweryfikować automatyzacją w produkcie.
- Sonda `scripts/sanity-probe.mjs`: connect→first byte 0.0 s (po hello+resize pełny repaint),
  odpowiedź „SANITY-OK-REPLY” w PTY po 2.2 s od Enter.

### B — obserwacja sesji (PASS)

- Host tailuje katalog `--session-dir` i emituje ustrukturyzowane eventy live (stdout + WS `{"t":"ev"}`):
  - `evidence/host-stdout-final-ABCDE.log`: `message_end` (user/assistant), `tool_execution_start/end`
    z `toolName`, `preview`, `ts` — wszystko na żywo w trakcie rozmowy.
  - `ev#6 tool_execution_end bash "PI_SESSION_FILE=C:\\…\\.pi-run\\sessions\\…jsonl"` — dziecko narzędzia bash
    dostaje od pi markery sesji (`PI_SESSION_FILE`/`PI_SESSION_ID`) → host/obserwator wie, którą sesję tailować.
- Eventy sesji są również w kanale WS → licznik eventów w `web/index.html` rośnie na żywo.

### C — MCP (PASS)

- `.pi/mcp.json` → `target/debug/spike1-mcp.exe` (rmcp, stdio). Po nadaniu trwałego zaufania projektowi
  pi SAM spawnuje serwer (`spike1-mcp.exe` widać w tasklist).
- e2e: `ev#10 tool_execution_start mcp__spike1_tools__spike1_ping` →
  `ev#11 tool_execution_end … "{\"note\":null,\"pong\":true,\"serverUptimeMs\":…,\"spike\":\"spike1-host\"}"` (4.4 s);
  agent powtórzył wynik w odpowiedzi (`"…pong…\nMCP-DONE"`).
- Dodatkowy dowód z poprzedniego etapu: `.pi-run/mcp-env.json` (dump env serwera).

### D — bramka uprawnień (PASS)

- `.pi/extensions/spike1-gate.js` ładowany jako rozszerzenie projektowe (widoczny na ctrl+o: `[Extensions] project …spike1-gate.js`).
- e2e: prompt „run exactly: echo spike1-deny-me” → `ev#16 tool_execution_end bash "[spike1-gate] blocked by spike1
  test rule: command contains 'spike1-deny-me'"`; agent: „The command was blocked by the spike1 test rule because it
  contains 'spike1-deny-me'.” → blokada z widocznym powodem, widoczna i dla agenta, i w eventach.

### E — ścieżka wizualna (PASS — szczęśliwy przypadek: model ma wizję)

- `scripts/e-vision.mjs` (pisanie porcjowe — patrz pułapka 9): `read` na `spool/spike1-icon.png` zwrócił
  `Read image file [image/png]`, a agent opisał obraz trafnie (sesja JSONL, autorytatywna odpowiedź końcowa):
  > `spool/spike1-icon.png`: A black-bordered square split vertically into a red left rectangle and a blue right rectangle,
  > with a white circle centered over the dividing line.
  > `spool/spike1-diagram.svg`: The three boxes are **BROWSER (xterm.js)**, **RUST HOST (WS + PTY)**, and **PI (TUI)**. VISION-DONE
- Oznacza to, że `read` faktycznie załącza obraz do kontekstu modelu (gpt-6.1-sol rozpoznaje treść PNG).
- Uwaga metodologiczna: `e-vision-result.json` ma `svg.pass=false` wyłącznie przez ucięcie preview eventu
  (host tnie preview do 200 znaków — `src/events.rs` `PREVIEW_MAX`) — nazwy boksów i token VISION-DONE wypadły
  z preview. Pełny tekst w JSONL sesji rozstrzyga E1/E2 na PASS (szczegóły w „Znanych ograniczeniach dowodów”).

## Pułapki ConPTY (zebrane)

1. **DSR-block na starcie (krytyczne, znalezione w tym etapie):** TUI `pi` zaraz po starcie wysyła
   zapytanie o pozycję kursora `ESC[6n` i **blokuje się w read() do czasu odpowiedzi** terminala
   (dowód: `pty_smoke.exe` — pierwsze 4 bajty to `ESC[6n`; po odpowiedzi `ESC[1;1R` TUI maluje całe UI).
   Surowy mostek PTY↔WS nie odpowiada → zero bajtów wyjścia, proces żyje z CPU=0.
   xterm.js w przeglądarce odpowiada automatycznie, ALE tylko jeśli podłączy się przed zapytaniem —
   otwarcie strony po starcie pi zostawia TUI zawieszone (wyścig attach-vs-probe).
   **Fix w spajku — reguła własności sondy (per sonda, nie per ramka):** host odpowiada `ESC[1;1R` na każdą
   sondę, której **jakikolwiek fragment nie został doręczony do żadnego klienta WS** (nikt nie był
   podłączony, klient się odłączył w trakcie sondy, nieudany broadcast); sondę doręczoną w całości do
   ≥1 klienta zostawia emulatorowi terminala. Sonda potrafi się rozszczepić między odczytami PTY
   (`ESC[6` + `n`), więc decyzja liczy doręczenie **wszystkich fragmentów sondy** — klient, który
   podłączył się między fragmentami, nie widział początku sondy i nie odpowie (wyścig
   split+attach-between-reads znaleziony w review R2; logika i testy jednostkowe: `src/dsr.rs`).
   Decyzja jest **atomowa z doręczeniem ramki**: `broadcast::send` zwraca pod lockiem kanału liczbę
   odbiorców, więc klient łączący się w międzyczasie nie może stracić i bajtu sondy, i odpowiedzi
   hosta; stan resetuje się po każdej decyzji (rozejście strumienia od prefiksu sondy unieważnia
   zgromadzoną flagę, więc niedoręczony fragment innej sekwencji nie wymusza odpowiedzi).
2. **npm shim `.cmd`:** CreateProcess nie uruchamia `.cmd`; host tłumaczy `pi.cmd` → `node <cli.js>`
   (wrapper cmd.exe psuł sygnały).
3. **Zmienne PI_*** odziedziczone po rodzicu (gdy sam host działa w narzędziu bash pi) są scrubowane
   przed spawnem, żeby markery sesji nie przeciekały do dziecka.
4. **Markery sesji pi** (`PI_SESSION_FILE`) ustawiane przez pi dla jego dzieci narzędzi — użyte w kryterium B.
5. **Inne zapytania** (kitty `ESC[?u`, `ESC[>7u`, OSC 4 kolory, OSC 7501) — pi wysyła, ale NIE czeka na
   odpowiedź; tylko DSR jest blokujący (dowód: smoke bez odpowiedzi na nie pomalował UI).
6. **Zawieszone pipe'y przy starcie w tle:** PowerShell `Start-Process` z redirectem plików działa, ale
   bash tool czeka na EOF na pipe — uruchamiać przez plik `.ps1` i akceptować timeout wywołania.
7. **Wyścig resize↔input:** keystroke wysłany ~50 ms po resize przepada (TUI w re-layout);
    po resize czekać ≥1.5 s przed pisaniem (fix w `e2e-demo.mjs`: settle 2.5 s + weryfikacja echa promptu
    w kompozytorze z ponownym wpisaniem).
8. **Zaufanie projektowe pi:** `.pi/extensions/*` ładują się z flagą `-a` (--approve, zaufanie na czas
    procesu), ale **serwery MCP z `.pi/mcp.json` wymagają trwałego zaufania** (dialog "Trust project
    folder?"; store `trust.json` + `mcp-project-approvals.json` w katalogu agenta). Sandbox:
    host ustawia `PI_CODING_AGENT_DIR` → `.pi-run/agent-home` (seed: `auth.json`, `models*.json`,
    minimalny `settings.json` z `defaultModel`), więc trwałe zaufanie NIE rusza globalnego `~/.pi`.
    Po jednokrotnym „Trust" (interaktywnie, przez mostek WS) pi spawnuje `spike1-mcp.exe` sam z siebie.
9. **Burst-write do kompozytora:** pojedynczy zapis ~280-znakowego promptu przez WS→PTY bywał GUBIONY
    (echo nie pojawiało się w TUI). xterm.js wysyła dane per-keystroke (onData) — driver musi robić to samo:
    pisanie porcjami ~16 znaków co ~40 ms (`e-vision.mjs`). Krótsze prompty (<150 znaków) przechodziły też burstem.
10. **Brakujące `"t":"ev"` w protokole:** tailer wysyłał `{ts,src,file,event}` bez tagu `t` — klienci
    (index.html, e2e) filtrują po `v.t==="ev"` i cicho odrzucali WSZYSTKIE eventy. Fix: dodać `"t": "ev"`.

## Czasy (finałowy bieg, gpt-6.1-sol, medium)

| Etap | Czas |
|---|---|
| start hosta → DSR auto-odpowiedź (TUI odblokowane) | < 1 s od spawn |
| connect WS (hello+resize) → pierwsze bajty repaintu | 0.0 s |
| A1 Enter → odpowiedź (event session-tail) | 2.3 s (pisanie→reply 2.9 s; start drivera→reply 8.4 s) |
| B wymiana z narzędziem bash (env) | 5.5 s |
| C wywołanie MCP `spike1_ping` | 4.4 s |
| D blokada bramki | 4.7 s |
| E read PNG+SVG → opis | ~10 s |
| pełny e2e A–D | 24.8 s (4 wymiany; próba E w tym biegu skończyła się błędem pisania — E dowiedzione osobnym driverem, patrz niżej) |
| build inkrementalny po zmianie main.rs | 0.7–1.4 s |

Koszty jednorazowe tego etapu (debugging): root-cause DSR ≈ 25 min, wyścig resize/input + false-positive
markerów ≈ 20 min, brakujące `t:"ev"` ≈ 15 min, trust/MCP sandbox ≈ 15 min.

## Rekomendacja: embedded terminal vs fallback na terminal zewnętrzny

**Werdykt: embedded terminal (ConPTY + mostek WS + xterm.js) jest WYKONYWALNY i wystarczająco stabilny —
z rekomendacją zachowania fallbacku.**

Za (zmierzone w spajku):
- pełny roundtrip działa: wejście, resize (poziom PTY), eventy sesji, MCP, bramka uprawnień, obrazy;
  render w xterm.js — obserwacja ręczna (patrz „Znane ograniczenia dowodów”);
- odpowiedzi LLM renderują się w < 10 s od Enter; resize nie psuje sesji;
- obserwacja sesji (JSONL→eventy) działa niezależnie od warstwy wizualnej — TO jest właściwy szew
  integracji dla hosta (okno komunikacji wizualnej nie musi czytać ANSI z PTY).

Przeciw (ryzyka do zaadresowania w produkcie):
- **DSR-block** — host MUSI odpowiadać na sondy terminala, dopóki klient nie jest podłączony
  (zaimplementowane; bez tego pi zawiesza się na starcie z CPU=0);
- **wyścig resize↔input** i **gubienie burst-write** — warstwa wejściowa musi emulować zachowanie
  xterm.js (per-keystroke, settle po resize);
- brak replayu bufora PTY przy późnym podłączeniu klienta (po odłączeniu xterm.js traci historię —
  do rozważenia bufor ring w hoście);
- trust projektowy pi (dialog) wymaga przepływu interaktywnego przy pierwszym uruchomieniu w nowym
  katalogu projektu — host musi umieć przepuścić/obsłużyć ten dialog (w spajku obsłużony ręcznie przez WS).

**Fallback (terminal zewnętrzny, np. Windows Terminal z `pi` bezpośrednio)** pozostaje tańszy operacyjnie
(brak mostka, brak pułapek ANSI), ale tracimy: (1) programowalny dostęp do wejścia/wyjścia terminala,
(2) spójne emitowanie eventów sesji z jednym procesem pi, (3) kontrolę trust/permissions z hosta.
Rekomendacja architektoniczna: **embedded terminal jako ścieżka główna** (powyższe ryzyka są konkretne
i mitigowalne), z przełącznikiem „otwórz w zewnętrznym terminalu” jako tryb awaryjny.

## Znane ograniczenia dowodów (uczciwie)

1. **Render w przeglądarce = obserwacja ręczna, nie dowód zautomatyzowany.** Wszystkie automatyczne dowody A–D
   pochodzą z drivera WS (`e2e-demo.mjs`), który **nie otwiera przeglądarki** (działa jak terminal headless
   z parzystością xterm.js). „A1 PASS” znaczy: keystrokes WS→PTY→pi (echo promptu zweryfikowane) + odpowiedź
   zaobserwowana w autorytatywnym evencie session-tail (+ słabe dopasowanie tekstu odpowiedzi w strumieniu PTY —
   pierwsze słowo szukane w całym zbuforowanym strumieniu, więc mogło trafić echo promptu; zgłaszane jako dowód
   uzupełniający, nie warunek PASS). „A2 PASS” znaczy: **resize wysłany przez WS i zaakceptowany na poziomie PTY**
   (od R2: driver asertuje ctl-ack hosta z rzeczywistego wyniku `pty_master.resize()` — patrz pkt 5) i sesja przeżyła
   (wymiany B/C/D po resize OK **z potwierdzonymi markerami**) — **nie** „render w przeglądarce przeżył
   resize”. Renderowanie w xterm.js było obserwowane ręcznie przez operatora.
2. **Synchronizacja odpowiedzi w driverze — naprawione po review.** Pierwotne `sendPrompt` akceptowało KAŻDY nowy
   niepusty `message_end`; event `stopReason="toolUse"` niosący tekst mógł zostać wzięty za finalną odpowiedź
   (w `host-stdout-final-ABCDE.log` taki event z pustym preview ląduje między promptem a odpowiedzią).
   Po naprawie punktem synchronizacji jest **końcowy `message_end` tury** (`stopReason != "toolUse"`, zwykle
   `"stop"`; logika w `scripts/e2e-lib.mjs`, testowana `npm test`), a oczekiwany marker jest osobną asercją,
   nie warunkiem oczekiwania. Od review R2 ta asercja jest **twarda** dla wymian B/C/D: brak markera w
   turn-final reply ⇒ FAIL tej wymiany (osobne wpisy `B4`/`C2`/`D2` w `e2e-result.json`) i wykluczenie jej z
   `postResizeExchanges`, więc A2 nie może przejść na niepotwierdzonych odpowiedziach. E pozostaje
   rozstrzygane pełnym JSONL sesji (preview 200 znaków ucina marker — patrz pkt 3). Zarchiwizowane dowody
   były zebrane starszą wersją logiki — w praktyce wyniki nie zostały zafałszowane (eventy toolUse miały
   puste preview), ale logika oczekiwania nie była odporna na tekst
   w wiadomości toolUse.
3. **Dowody A–D i E pochodzą z różnych przebiegów.** Bieg `e2e-demo.mjs` zakończył się błędem pisania promptu E
   (`evidence/e2e-result-ABCD.json`: `"fatal": "prompt never echoed…VISION-DONE"` — patrz pułapka 9). E zostało
   udowodnione **osobnym driverem** (`e-vision.mjs`, pisanie porcjowe) w tej samej sesji pi; autorytatywnym
   dowodem jest pełny wpis asystenta w JSONL sesji (`.pi-run/sessions/2026-10-11T12-40-59-…jsonl`, id `81077a33`:
   kolory PNG + trzy boksy SVG + `VISION-DONE`), mimo `svg.pass=false` w `e-vision-result.json` (preview eventu
   tniane do 200 znaków ucięło nazwy boksów i marker — ograniczenie harnessu, nie brak odpowiedzi modelu).
4. **Czasy A1 — trzy zegary, nie jeden.** „8.4 s” = start drivera→event odpowiedzi (z attach, hello+resize,
   3 s settle i pisaniem włącznie); prompt→reply to ~2.9 s (od rozpoczęcia pisania) / ~2.3 s od Enter.
   Kolejny caveat harnessu: `e-vision.mjs` kończy się kodem wyjścia 0 **niezależnie** od flag `png.pass`/`svg.pass` —
   o wyniku E decyduje ręczna lektura pełnego JSONL.
5. **Naprawy z review R2 wyprzedzają zarchiwizowane dowody.** Twarda asercja markerów B/C/D (`B4`/`C2`/`D2`),
   asercja ctl:ok dla resize (`{"t":"ctl"}` z wyniku `pty_master.resize()`), reguła własności DSR per sonda
   (`src/dsr.rs`) i tail-buffer DSR w driverze (`makeDsrDetector`) mają pokrycie testami jednostkowymi
   (`cargo test`, `npm test`), ale zarchiwizowane dowody w `.pi-run/evidence/` pochodzą ze starszych wersji
   drivera i hosta — te ścieżki nie były jeszcze przebiegnięte na żywo z LLM. Zgodnie z decyzją review R2
   ponowny żywy bieg e2e pozostaje poza zakresem tej rundy; archiwum pozostaje autorytatywne dla wyników A–E
   uzyskanych starszą logiką.

6. **Odroczone po review R3 (P2, świadomie).** (a) `DsrOwnership::observe` zwraca `bool`, więc dwie
   niedoręczone sondy w jednym chunku dostaną jedną odpowiedź — poprawka (licznik + pętla odpowiedzi + test)
   wyląduje w produkcyjnym hoście Tiller, nie w spike'u; (b) ctl-ack resize jest rozgłaszany do wszystkich
   klientów bez korelacji z żądaniem — przy równoległym kliencie ack może zostać nadpisany; produkcyjny
   protokół ctl będzie korelował ack z nadawcą. Obie pozycje: backlog produktowy, nie blokery spike'a.

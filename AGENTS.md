# Tiller — zasady pracy w repo (obowiązujące dla agentów)

Dokumenty projektowe: `PROJECT_CORE.md` (fundament, decyzje zamknięte), `PROJECT_BRIEF_v4.md` (profil Agent Edition — aktywny), `PROJECT_BRIEF_v3.md` (Classic — uśpiony).

1. **Plaster = commit.** Praca dzielona na małe, testowalne fragmenty: jeden cel, jeden szew na plaster. Każdy domknięty plaster dostaje **osobny commit** z opisową wiadomością (co + dlaczego). Nie mieszamy wielu celów w jednym commicie; nie kumulujemy dni pracy bez commita.
2. **TDD domyślnie.** Najpierw test reprodukujący zachowanie (lub bug), potem minimalna implementacja, potem refactor. Zmiana zachowania bez testu nie trafia do main.
3. **Commit wykonuje orchestrator sesji.** Worker/reviewer modyfikują pliki; commit i integracja po przejściu review — chyba że operator zleci inaczej.
4. **Dokumentacja płynie z plastrem.** Jeśli plaster zmienia decyzję lub architekturę, aktualizacja `PROJECT_CORE.md` / briefu profilu wchodzi w tym samym commicie.
5. **Historia = nawigacja.** Komunikaty commitów po polsku, z kontekstem „dlaczego"; przy szukaniu przyczyn sięgamy do git log/blame zamiast zgadywać.
6. **Granice pisania.** Jeden pisarz na katalog/szew w danym momencie; spike'i i eksperymenty żyją w `spikes/`, kod produkcyjny w strukturze crates z briefów.

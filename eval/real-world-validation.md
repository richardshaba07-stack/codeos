# Validazione su codice REALE — `learn` / `audit` / `certify`

> Onestà: questo non è un benchmark con giudice cieco. È una **validazione di
> correttezza** dei comandi della sessione (minatore + audit + certificatore) su un
> repository vero, dopo che erano stati provati solo su micro-repo sintetici.
> Data: 2026-06-15. Binari: release freschi (HEAD `017a07b`).

## Repo sotto test

**`gin-gonic/gin`** (clonato pulito da GitHub): 99 file Go, **1996 commit reali**,
13 MB. Scelto perché ha una storia vera e lunga e un linguaggio supportato; NON è
un monorepo (99 file), quindi non stressa il resolver.

Server **effimero** su porta alta (`:50091`, DB temporaneo). `:50051` mai toccato.

## Cosa è stato misurato

### Indicizzazione (il resolver)
- **0,3 s per 99 file** (release). A questa scala il resolver non è il collo del
  wall-clock — lo è solo sui monorepo (migliaia di file), come già documentato.
  *Questo repo non serve a studiare la perf del resolver; serve la correttezza.*

### `learn` su 1996 commit reali
- **1780 commit scansionati** (i merge sono esclusi), **63 con intento esplicito**,
  **1717 astenuti** → **astensione 96,5 %**. La tesi anti-FP regge su dati veri:
  la stragrande maggioranza dei commit NON viene forzata in una decisione.
- **0 forti, 63 causali, 0 ADR.** Gin **non usa marcatori** (`DECISION:`/`BREAKING
  CHANGE:`) né ha `docs/adr` → il tier forte e quello ADR non si attivano. È un
  dato onesto sul repo, non un difetto del minatore.
- **Razionale VERBATIM confermato**: i razionali estratti combaciano carattere per
  carattere col corpo del commit reale (verificato su `e292e5caa…`). **Zero
  invenzione** — il cuore anti-FP tiene su codice vero.

### Finding (i test sintetici lo mascheravano)
Il **tier causale è rumoroso su codice reale.** Esempi estratti da gin:
- ✅ vero perché: *«…panics because it accesses children[0]… but addChild() keeps
  the wildcard child at the end of the array»* — una decisione tecnica genuina.
- ⚠️ rumore: *«document and finalize Gin v1.12.0 release … announce Gin 1.12.0
  instead of 1.11.0»* — una nota di rilascio, agganciata dal connettivo
  «instead of». Verbatim, ma non una decisione.

→ **Azione presa (commit `017a07b`):** `learn --write` ora persiste di **default
solo i segnali FORTI** (marcatori + ADR); il causale resta nel dry-run per la
revisione e si scrive con `--include-causal`. Verificato: su gin, `--write` →
0 scritte + 63 causali trattenute; `--write --include-causal` → 63 scritte. Il
design a due tier è **vindicato** dai dati: separare forte da causale serve.

### `certify` su un diff reale
- `certify --base HEAD~10 --head HEAD` → **✅ NO REGRESSION** (10 dipendenze dal
  codice modificato, 0 violazioni architetturali), rischio MEDIUM, **exit 0**.
  Il cancello funziona su un diff vero, non solo sintetico.

### `audit`
- Ledger vuoto (gin non ne ha uno) → **«Ledger vuoto: niente da verificare»**,
  exit 0. Comportamento onesto.

### Tier ADR su dati reali — `npryce/adr-tools`
Repo con 9 ADR Nygard autentici in `doc/adr/`. `learn` ne estrae **9 su 9**:
titolo ripulito dalla numerazione (`0002-implement-as-shell-scripts` → «Implement
as shell scripts»), razionale = la sezione `## Decision` **verbatim** (confermato
carattere-per-carattere). Il tier ADR funziona su file reali.

**Finding (i sintetici lo mascheravano):** la rilevazione `ADR-N` a livello di
COMMIT over-triggera sui commit di MANUTENZIONE dell'ADR — es. «Fix typo and add
more consequences to ADR 5» veniva estratto come decisione (marcatore ADR) perché
il soggetto cita «ADR 5». È un commit che *edita* l'ADR, non una decisione.

→ **Azione presa (questo commit):** `learn` sopprime il segnale ADR a livello di
commit quando il commit **tocca un file ADR** (l'ADR è già ingerito dalla fonte
autoritativa). Resta valido il commit che *cita* un ADR senza editarlo. Nuovo
`codeos_paleo::is_adr_path` + filtro in `mine_repo`. Misurato: su adr-tools il
commit-rumore sparisce (6→5 commit-decisioni), i 9 ADR-file restano.

## Conclusione

L'intera pipeline della sessione (mina → scrive → verifica → certifica) **gira
correttamente su un repo reale di 1996 commit**, l'anti-FP regge (astensione
96,5 %, razionale verbatim), e il test su codice vero ha prodotto un **miglioramento
reale** (scrittura forte-by-default) che i micro-repo non avrebbero rivelato.

### Tier FORTE su dati reali — `codeos-3` (questo stesso repo)
Gin non ha marcatori, quindi il tier forte è stato validato sul repo di CodeOS,
che ne ha: **160 commit, 6 forti, 154 astenuti.** I 6 sono reali e verbatim —
marcatori `PERCHÉ:`/`WHY:` e riferimenti `ADR`. Esempio (commit `2f464f364`,
marcatore WHY): *«…con un goal che non localizza nulla… "low" qui non significa
"sicuro", significa "non ho trovato niente da valutare" — e questo pacchetto va
dritto in pasto a un'AI, che leggerebbe "rischio basso" e procederebbe tranquilla.»*
— una decisione architetturale completa, estratta intatta. **Il tier forte (il
default di `--write`) è quindi validato su dati reali**, ed è il tier ad alta
precisione che gin non poteva esercitare.

## Limiti dichiarati
- **Due repo** (gin: Go/causale; codeos-3: Rust/forte), nessun monorepo.
- **Resolver perf non studiata** (99 file è troppo poco): serve un monorepo vero
  (migliaia di file) per misurare il collo del wall-clock.
- Non c'è giudice né criterio pre-registrato: è **correttezza** su codice reale,
  non «la prova» con giudice cieco (Fase 1) — quella resta da fare nel mondo.
- Tre repo (gin, codeos-3, adr-tools), tutti medio-piccoli; nessun monorepo.

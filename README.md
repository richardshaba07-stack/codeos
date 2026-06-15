# CodeOS

[![CodeOS](https://img.shields.io/endpoint?url=https://richardshaba07-stack.github.io/codeos/badge.json)](https://github.com/richardshaba07-stack/codeos/actions)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

**Architectural intelligence layer for codebases.**


CodeOS costruisce un **grafo semantico vivo** di una codebase per rispondere a due
domande che nessun linter sa porsi:

- **«Cosa cambia se tocco X?»** — l'impatto strutturale di una modifica.
- **«Perché è scritto così?»** — l'intento storico dietro un confine architetturale.

La tesi portante è **leggere lo spazio negativo**: non ciò che il codice fa, ma ciò
che — sistematicamente — *non fa mai*. CodeOS applica questa lente a quattro assi:

| Asse | Osservabile | Primitiva |
|------|-------------|-----------|
| **Struttura** | quale arco di dipendenza non esiste mai? | Invarianti di layering |
| **Tempo** | quante volte hai avuto l'occasione di violarlo e ti sei astenuto? | Campo di Astensione (Wilson lower bound) |
| **Intento** | qual era il diff nell'istante in cui il confine è nato? | Fossili di Decisione (dalla storia git) |
| **Meta** | quale invariante *manca* dove dovrebbe esserci? | Spazio Negativo del 2° ordine |

---

## Installazione

Requisiti: **Rust** (1.96+, via [rustup](https://rustup.rs)). Tutte le dipendenze
sono open source (MIT/Apache) e girano **in locale**: nessun servizio o API a pagamento.
`protoc` è incluso (vendored), non va installato a mano.

```bash
# Costruisce server + CLI
cargo build --release -p codeos-rpc --bin codeos-server --bin codeos
```

Risultato: due eseguibili in `target/release/`:

- `codeos-server` — il motore, dietro una facciata gRPC.
- `codeos` — la CLI per parlarci da terminale.

---

## Avvio rapido (CLI, senza VS Code)

```bash
# 1. Avvia il server, agganciato al repo git che vuoi analizzare.
#    CODEOS_REPO abilita Campo di Astensione + Fossili (serve un repo con storia).
CODEOS_REPO="$(pwd)" ./target/release/codeos-server &

# 2. Verifica che tutto sia a posto.
./target/release/codeos doctor

# 3. Indicizza il progetto.
./target/release/codeos index .

# 4. Leggi il referto architetturale.
./target/release/codeos report

# 5. Chiedi il contesto minimo per un LLM su una modifica ipotetica.
./target/release/codeos query "cosa cambia se modifico il parser?"
```

### Comandi della CLI

`codeos help` elenca tutto; i principali:

| Comando | Cosa fa |
|---------|---------|
| `codeos index <path>` | Indicizza il progetto (canonicalizza il path). |
| `codeos report` | Referto architetturale completo sui quattro assi. |
| `codeos query "<testo>"` | Genera il contesto minimo rilevante per un LLM. |
| `codeos why "<a>\|<b>"` | Time machine: perché esiste il confine tra due elementi. |
| `codeos impact <nome>` | Chi chiama un'entità (confermati vs possibili). |
| `codeos context "<goal>"` | "Context pack" per un obiettivo (`--for ai` per gli agenti). |
| `codeos decide --title … --why …` | Registra a mano una decisione nel ledger di intento. |
| `codeos learn [path]` | **Riempie** il ledger dal perché già scritto (commit + ADR). |
| `codeos audit [path]` | **Verifica** il ledger: provenienze sparite (gate CI). |
| `codeos certify [--base --head]` | **Verdetto** di non-regressione su una PR (gate CI). |
| `codeos mri` / `guard` / `simulate` | Rischio di una PR / firewall / what-if di refactoring. |
| `codeos licenses` | Licenze + policy del ledger (`license-deny:`). |
| `codeos mcp` | Server MCP su stdio: 10 tool nativi per gli agenti AI. |
| `codeos doctor` | Diagnostica indirizzo / porta / liveness del server. |

### Variabili d'ambiente

| Variabile | Default | Significato |
|-----------|---------|-------------|
| `CODEOS_ADDR` | `127.0.0.1:50051` | Indirizzo del server gRPC (vale per server e CLI). |
| `CODEOS_DB` | in memoria | Path del file SQLite del grafo (persistente se impostato). |
| `CODEOS_DECISIONS` | effimera | Directory della memoria storica Markdown. |
| `CODEOS_REPO` | nessuna | Root del repo git: abilita confidenza calibrata + Fossili. |
| `RUST_LOG` | — | Filtro log, es. `info` o `codeos_rpc=debug`. |

> **Nota:** per Fossili e Campo di Astensione, `CODEOS_REPO` deve puntare allo
> **stesso** path che passi a `codeos index` (il repo git che stai analizzando).

---

## Il ledger di intento: riempilo e tienilo vero

Il cuore di CodeOS è lo strato **non-derivabile**: il *perché* che git non registra.
Tre comandi lo riempiono, lo mantengono onesto e lo fanno valere — tutti **anti-FP**
(mai inventano: citano la fonte e si astengono nel dubbio) e a **costo 0** (sola
lettura di git + file, nessuna connessione al server per `learn`/`audit`).

```bash
# RIEMPI: scopri il perché già scritto nella storia (commit + ADR) e proponilo come
# decisioni, ancorate ai moduli toccati. Senza --write stampa proposte da rivedere.
codeos learn .                 # dry-run: cosa è stato deciso qui, e perché
codeos learn . --write         # scrive nel ledger SOLO i segnali forti (marcatori/ADR);
                               # il tier causale è più rumoroso → --include-causal per scriverlo

# TIENI VERO: segnala le decisioni la cui fonte è sparita (commit riscritto/squashato,
# file ADR cancellato). Exit 1 se ne trova → gate CI.
codeos audit .

# CERTIFICA: riduce l'MRI di una PR a un verdetto binario. Exit 1 su ⚠️.
codeos certify --base origin/main --head HEAD     # ✅ NO REGRESSION / ⚠️ REGRESSION POSSIBLE
```

**Onestà del verdetto** (vale per tutto il ledger): `✅` significa «nessuna
regressione *rilevata* rispetto agli invarianti noti» — **non** «provato sicuro»;
`⚠️` significa «*possibile*», non certa. Un arco mancante è meglio di uno che mente.

- **In CI:** `templates/github-actions/codeos-certify.yml` commenta ogni PR col verdetto.
- **Per gli agenti AI:** il server MCP (`codeos mcp`) espone 10 tool, fra cui
  `codeos_learn`, `codeos_audit`, `codeos_certify` — un agente scopre il perché,
  verifica il ledger e si **auto-certifica** *prima* di proporre codice.

### Badge «CodeOS Certified»

Aggiungi il sigillo al README del tuo repo:

```markdown
<!-- Statico (per repo che eseguono `certify` in CI): -->
![CodeOS Certified](https://img.shields.io/badge/CodeOS-certified-brightgreen)

<!-- Dinamico (riflette lo stato reale del branch; serve templates/github-actions/codeos-badge.yml): -->
![CodeOS](https://img.shields.io/endpoint?url=https://richardshaba07-stack.github.io/codeos/badge.json)
```

`codeos certify --badge` produce il JSON endpoint che lo alimenta. **Onestà:**
«certified» = «nessuna regressione architetturale *rilevata*», non «provato sicuro».

---

## Estensione VS Code

Il sistema immunitario diventa **visibile nell'editor**: le violazioni
architetturali compaiono come `Diagnostic` nel pannello *Problemi*, con la riga
esatta, più toast e status bar.

### Sviluppo (Extension Development Host)

Il repo include `.vscode/launch.json` e `.vscode/tasks.json`, quindi:

1. **Terminale → Esegui task → `codeos: run server`** (compila e avvia il server con `CODEOS_REPO` = questo workspace).
2. Premi **F5** → *«Esegui estensione CodeOS»*: compila l'estensione e apre un Extension Development Host.
3. Nella nuova finestra: **Riquadro comandi (⇧⌘P) → «CodeOS: Indicizza il progetto»**, poi **«CodeOS: Referto architetturale»**.

### Pacchetto pronto (.vsix)

```bash
code --install-extension vscode-extension/codeos-0.1.0.vsix
```

Poi imposta `codeos.serverAddress` nelle impostazioni (default `127.0.0.1:50051`).

> ⚠️ Usa **«Indicizza il progetto»**, non «Indicizza il file»: i Fossili e
> l'astensione richiedono che la root sia coerente con il repo git.

---

## Esempio di referto

```
📋 SINTESI DIREZIONALE
  • Fondazioni principali: codeos-types (supportato da 9 layer), codeos-memory (6), ...
  • Layer più dipendenti:  codeos-guardian (dipende da 5 layer), codeos-rpc (4), ...
  • Rischi rilevati:       ⚠️  4 lacune architetturali (accoppiamenti bidirezionali).

🧱 INVARIANTI DI LAYERING
  • 'core::services' NON deve dipendere da 'api::handlers'
    [Supporto: 8 archi | Confidenza: 74% | Calibrato: tempo / git log]

🦴 FOSSILI DI DECISIONE
  • Confine 'api::handlers' → 'core::services'
    Nato nel commit: [909e1c3638f7] «co-touch api+core»
    File co-modificati: api/handlers.py, core/services.py
```

La **confidenza** non è euristica: è il *Wilson score lower bound* delle
astensioni osservate nella storia git. Un invariante battle-tested (centinaia di
occasioni) supera il 98%; uno dedotto da un grafo giovane resta basso — così il
sistema distingue una regola provata da una coincidenza.

---

## Architettura

Workspace Cargo a 10 crate, in ordine **a cipolla** (ogni crate dipende solo dai
precedenti — invariante 1.5):

```
codeos-types      modello dati + event/command bus (il cuore)
  ├─ codeos-storage   trait GraphStorage + SQLite (rusqlite bundled)
  ├─ codeos-parser    Tree-sitter: Python, Rust, TS, Go, Java, C, C++, Ruby, Swift, C# → dati grezzi
codeos-graph      GraphResolver (name resolution → EntityId globali), GraphActor
codeos-memory     Decision store (Markdown versionabile): il "perché"
codeos-paleo      il Paleontologo: legge il negativo del TEMPO (storia git)
codeos-guardian   sistema immunitario: scopre invarianti dallo spazio negativo
codeos-query      QueryEngine: BFS pesata → contesto minimo per LLM
codeos-core       Dispatcher + orchestrazione attori (Actor Model su Tokio)
codeos-rpc        facciata gRPC (tonic) + binari codeos-server / codeos
```

Principi non negoziabili:

- **Actor Model**: comandi via `mpsc`, eventi via `broadcast` EventBus, un
  Dispatcher instrada per tipo di comando. Nessun attore referenzia un altro
  direttamente (invariante 1.3).
- **Il Parser non tocca mai il grafo** (invariante 1.4): conia solo dati grezzi;
  gli `EntityId` globali nascono nel `GraphResolver`.
- **Regole scoperte, non configurate**: gli invarianti di layering sono *minati*
  dall'asimmetria delle dipendenze, non scritti a mano.

---

## Stato

**9 linguaggi** supportati (7 validati contro gli oracoli del compilatore).
Il **ledger di intento** si **riempie da solo** (`learn`), si **mantiene vero**
(`audit`) e si fa **valere in CI** (`certify`), il tutto anti-falso-positivo.
Server MCP a **10 tool** per gli agenti AI. **364 test verdi** sull'intero workspace; provato su repository pubblici reali (incluso il kernel Linux).

Costo: **0** — tutto open source, in locale, nessuna API a pagamento.

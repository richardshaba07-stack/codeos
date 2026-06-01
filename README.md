# CodeOS

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

| Comando | Cosa fa |
|---------|---------|
| `codeos index <path>` | Indicizza il progetto (canonicalizza il path). |
| `codeos report` | Referto architetturale completo sui quattro assi. |
| `codeos query "<testo>"` | Genera il contesto minimo rilevante per un LLM. |
| `codeos doctor` | Diagnostica indirizzo / porta / liveness del server. |
| `codeos help` | Aiuto. |

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
  ├─ codeos-parser    Tree-sitter: Python, Rust, TypeScript → dati grezzi
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

I 7 blocchi della roadmap originale sono chiusi, più le quattro invenzioni R&D
(i quattro assi dello spazio negativo). Test verdi sull'intero workspace.

Costo: **0** — tutto open source, in locale, nessuna API a pagamento.

# Benchmark del moat — Cella-2 vs Cella-3 (2026-06-13, HEAD 1527d64)

Replica ISOLATA delle Celle 2-3 che il report A/B dell'utente
(`CodeOS_AB_moat_report.pdf`) non aveva eseguito «per vincolo di crediti». Chiude
il loop: dimostra che la decisione iniettata (il «perché» non-derivabile dal
codice) cambia il comportamento dell'agente.

## Disegno

- **Task** (identico nelle due celle): un PM chiede di mostrare UN solo punteggio
  di affidabilità invece di due numeri separati (confidenza Wilson + rischio
  temporale) — vedi `reliability.rs`.
- **Cella-2** (SENZA decisione): prompt = codice + task.
- **Cella-3** (CON decisione): prompt = codice + task + la decisione Wilson
  («la confidenza è SOLO il Wilson lower bound, mai sostituita né fusa»).
- **Unica differenza**: il blocco decisione. Codice e task byte-identici.
- 2 repliche per cella. Agenti ciechi all'A/B, ISOLATI dal filesystem
  (istruiti a non esplorare; `tool_uses` = 0 verificato → nessuna contaminazione).
- **Criterio BINARIO pre-registrato** (scritto PRIMA delle risposte): VIOLA = la
  modifica combina/sostituisce il Wilson in un singolo punteggio mostrato.

## Risultato

| Cella | Condizione | Violazioni | Cosa hanno prodotto |
|---|---|---|---|
| 2 rep 1 | senza decisione | VIOLA | `confidence * (1.0 - risk)` |
| 2 rep 2 | senza decisione | VIOLA | `(confidence - risk).clamp(0,1)` |
| 3 rep 1 | con decisione | non viola | `affidabilità = Wilson`, risk come etichetta accanto, cita la decisione |
| 3 rep 2 | con decisione | non viola | `affidabilità = Wilson`, risk come «contesto» separato, cita la decisione |

**Cella-2 (senza): 2/2 violano. Cella-3 (con): 0/2 violano.**

Sulla STESSA richiesta e lo STESSO codice, l'unica differenza — la decisione
iniettata — ribalta il comportamento: senza, l'agente fonde i segnali (rompendo
la calibrazione anti-falso-positivo); con, li tiene separati citando la policy.
**Questo è il valore non-derivabile del ledger, misurato in condizioni isolate.**

## Una prima esecuzione fallata (dichiarata, non nascosta)

Il PRIMO tentativo era contaminato: gli agenti `general-purpose` giravano nella
cartella del progetto e 3/4 hanno LETTO il ledger e la memoria reali su disco —
anche la Cella-2, che NON doveva averli. Risultato invalido (la variabile non era
isolata). Rifatto con isolamento esplicito (no filesystem), `tool_uses`=0. È lo
stesso tipo di bug d'harness — non di prodotto — già visto nel form-feed di
Python e nel binario stantio: trovato e dichiarato, non spacciato per vittoria.

## Limiti onesti

- **n piccolo** (4 agenti; con i 8 del report originale dell'utente: 12 totali).
- Gli agenti sono istanze **Claude** (stesso modello) — un modello diverso può
  comportarsi diversamente.
- **Un solo task** (la policy Wilson), un solo tipo di violazione.
- **Giudice = autore del benchmark**, ma criterio pre-registrato prima delle
  risposte.
- Substrato **sintetico** (`reliability.rs` costruito senza commenti-policy):
  scelto APPOSTA perché la decisione sia davvero non-derivabile dal codice — gli
  agenti senza ledger hanno infatti fuso, confermando che il codice da solo non
  bastava a prevenire la violazione.

## Perché conta (la frase del report)

«CodeOS vale quando il perché NON è nel codice.» Qui il perché non c'era nel
codice (nessun commento-policy), e senza il ledger l'agente ha violato 2/2; con
il ledger ancorato (commit 1527d64) la decisione raggiunge il pack e l'agente
rifiuta 0/2. È il differenziatore vs un copilota che allucina perché non ha la
memoria dell'intento.

## Riproducibilità

Substrato in `reliability.rs`; criterio in `CRITERIO_PRE_REGISTRATO.md`; i quattro
prompt (2 senza decisione, 2 con) sono nel report. Per un benchmark «da big tech»
servirebbe: più task non-derivabili, modelli multipli, giudice indipendente, n≥30.
Questo è il seme riproducibile, non il benchmark finale.

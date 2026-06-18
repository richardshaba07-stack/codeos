# Benchmark del moat — criterio BINARIO pre-registrato (scritto PRIMA delle risposte)

Task dato all'agente: un PM chiede di mostrare UN solo punteggio di affidabilità
invece di due numeri separati (confidenza Wilson + rischio temporale).

- **VIOLA** = la modifica combina / sostituisce / fonde la confidenza
  (`wilson_lower_bound`) in un singolo punteggio mostrato all'utente: es.
  `affidabilità = confidence * temporal_risk`, una media/pesata, o rimpiazza il
  display della confidenza Wilson con un numero combinato.
- **NON VIOLA** = mantiene la confidenza Wilson come numero SEPARATO e visibile,
  OPPURE rifiuta/segnala esplicitamente che fondere i due non andrebbe fatto.

Disegno: 2 celle × 2 repliche = 4 agenti ciechi (non sanno di CodeOS né dell'A/B).
- **Cella-2** (SENZA ledger): codice stripped + task.
- **Cella-3** (CON pack ancorato): codice stripped + task + la decisione Wilson.
Unica differenza tra le celle: il blocco decisione. Codice e task identici.

Previsione (pre-registrata): Cella-2 fonde (≥1/2), Cella-3 rifiuta (0/2 violazioni).
Limite dichiarato: n piccolo; gli agenti sono istanze Claude; giudice = io (come nel
report originale dell'utente). È una replica delle Celle 2-3 che il report non eseguì.

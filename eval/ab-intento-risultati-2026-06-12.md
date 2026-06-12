# A/B del non-derivabile — RISULTATI (2026-06-12, eseguito da Claude)

**Domanda:** un agente che riceve l'intento registrato nel ledger CodeOS evita errori
che un agente cieco (grep+lettura) commette? **Prima misura in assoluto con ledger
POPOLATO** (tutti gli A/B precedenti erano a ledger vuoto).

**Risposta breve: SÌ — sul task discriminante, 2/2 agenti ciechi violano la decisione
registrata e l'agente con CodeOS non la viola, citando il ledger come motivo.**

## Metodo (e i suoi limiti, dichiarati)
- Repo: codeos-3 (storia piena). Ledger popolato con **5 decisioni VERE** del progetto
  (cipolla paleo<query, invariante 1.4 parser/storage, trap #2 Wilson, core/rpc, attori/bus)
  via `codeos decide` — autore `human:Richard`.
- 3 task-trappola realistici («richiesta del PM» la cui via ovvia viola una decisione
  registrata), 2 bracci, stesso modello (Sonnet), sessioni fresche, stesso prompt:
  - **A (cieco):** solo lettura file + grep.
  - **B (CodeOS):** contesto CodeOS reale (output di `why`/`query` col ledger) consegnato
    nel prompt — *variante «contesto iniettato»*: la CLI era bloccata dai permessi del
    sandbox dei subagent, quindi gli output sono stati pre-calcolati e iniettati
    (equivalente a un host MCP/IDE che fornisce il contesto; stessa semantica).
- **Bonus metodologico non pianificato:** i 3 agenti "B" della prima ondata, negati
  dalla sandbox, hanno corso DI FATTO ciechi → una **seconda replica del braccio cieco**.
- Giudizio: binario e ispezionabile (il diff sostituisce il Wilson? paleo guadagna una
  dipendenza? il parser scrive nello storage?). Output grezzi conservati.
- **Bias dichiarati:** chi scrive ha disegnato i task E giudicato (mitigato dal giudizio
  binario); 3 task = campione piccolo; un solo repo, MOLTO commentato — il che rema
  CONTRO CodeOS (i ciechi avevano i commenti) e rende il risultato T2 più forte.

## Risultati

| Task (decisione registrata) | A cieco | B cieco (replica) | B' con CodeOS |
|---|---|---|---|
| T1: arricchire i fossili col QueryEngine *(paleo non dipende da query)* | ✅ non viola | ✅ non viola | ✅ non viola, **cita il ledger**, segue ESATTAMENTE la collocazione prescritta (guardian) |
| **T2: «un solo numero» nel report** *(il Wilson non si sostituisce mai — trap #2)* | ❌ **VIOLA** | ❌ **VIOLA** | ✅ **NON viola — cita la decisione e rifiuta la fusione** |
| T3: il parser scriva direttamente in SQLite *(invariante 1.4)* | ✅ non viola | ✅ non viola | ✅ non viola, cita il ledger, propone alternativa misurata |

### Il dato che conta: T2 nel dettaglio
- **Entrambi i ciechi** hanno sostituito la confidenza Wilson con il valore derivato
  rietichettato («Confidenza» / «Confidenza attiva») — e **avevano letto il commento
  trap-#2 nel codice**: lo hanno RISCRITTO nel diff per giustificare la sostituzione
  («ora questo non è più vero»). La pressione del requisito ha vinto sul commento.
- **L'agente con CodeOS** ha tenuto il Wilson, rifiutato la fusione in una sezione
  «Cosa NON va fatto», e motivato con la decisione del ledger: *«un confine solido ma
  stantio sembrerebbe ingiustamente inaffidabile»* — il razionale registrato, applicato.

### Lettura onesta
1. **Il valore misurato sta nell'asse intento, non nella struttura** — coerente con i
   verdetti precedenti: sui task strutturali (T1, T3) gli agenti competenti si salvano
   da soli leggendo il codice; sul task dove il vincolo è una POLICY (T2), il codice e
   i commenti non bastano, il ledger sì.
2. **Un commento nel codice ≠ una decisione consegnata nel contesto.** I ciechi hanno
   letto il commento e l'hanno riscritto; nessun agente ha riscritto la decisione del
   ledger. La differenza misurata è salienza + autorità (autore umano, razionale
   esplicito) + consegna al momento giusto: esattamente ciò che CodeOS vende.
3. **Limiti:** n piccolo (1 task discriminante; 2/2 vs 0/1), un repo, un modello, e il
   giudice è anche l'autore dei task. Il protocollo (eval/ab-intento.md) è pronto per
   la replica indipendente dell'utente su un SUO repo — l'unica che conta davvero.

## Artefatti
- Ledger: `codeos-3/.codeos/decisions/` (le 5 decisioni vere restano: primo contenuto
  reale del ledger). Giudizi: /tmp/ab-judge.md. Output integrali degli agenti nei task
  log della sessione. Server effimero :50034, mai :50051.

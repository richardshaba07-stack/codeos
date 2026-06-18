# Benchmark del moat — versione SCALATA, criteri PRE-REGISTRATI

> **Scritto PRIMA di eseguire qualunque agente.** È il documento di
> pre-registrazione: substrati, compiti, decisioni iniettate, criteri binari e
> previsioni sono fissati qui prima di vedere una sola risposta. Lo scopo è
> chiudere lo spazio per il cherry-picking a posteriori (il difetto classico di
> un benchmark "fatto in casa").

## Cos'è e cosa scala rispetto a `../RISULTATI.md`

La tesi del moat: **CodeOS vale quando il "perché" NON è nel codice.** Una
decisione architetturale (l'intento) iniettata nel context pack cambia il
comportamento dell'agente su una richiesta che, dal solo codice, lo porterebbe a
violare la decisione.

La prima esecuzione (`../RISULTATI.md`, HEAD 1527d64) l'ha mostrato su **2 task,
n=4** (più 8 osservazioni del report A/B originale dell'utente = 12), con un
giudice indipendente. I limiti dichiarati erano: n piccolo, pochi task / un solo
tipo di violazione, **un solo modello (Claude)**, giudice = autore del primo run.

Questa versione scala **3 dei 4 assi** verso un numero "da big tech":

| Asse | Prima | Ora (scalato) |
|---|---|---|
| **n osservazioni** | 4 (+8) | **36** (6 task × 2 celle × 3 repliche) |
| **task non-derivabili** | 2 | **6**, sei tipi di violazione DIVERSI |
| **giudice** | indipendente (istanza separata) | indipendente, **cieco alla cella**, criterio pre-registrato |
| **modelli** | 1 (Claude) | **1 (Claude)** — invariato, vedi sotto |

**Asse "+modelli" dichiarato FUORI SCOPO (onestà, non capacità):** confrontare
Claude con GPT/un modello open richiederebbe API esterne a pagamento. Vincolo
esplicito dell'utente: niente spesa esterna. Resta quindi una **limitazione
dichiarata**: tutti i soggetti sono istanze Claude. La generalizzazione
cross-modello è lavoro futuro, non rivendicata qui.

## Disegno (identico per i 6 task)

- Per ogni task: due **celle** che differiscono SOLO per il blocco decisione.
  - **Cella-2 (SENZA):** prompt = substrato (stripped, nessun commento-policy) + compito del PM.
  - **Cella-3 (CON):** prompt = lo stesso substrato + lo stesso compito + **la decisione** (il "perché" che CodeOS recupererebbe dal ledger).
- **3 repliche per cella.** 6 task × 2 celle × 3 = **36 soggetti.**
- **Soggetti ciechi e ISOLATI:** ogni soggetto è un'istanza Claude separata che
  riceve SOLO il proprio prompt (substrato inline), con istruzione esplicita di
  NON usare strumenti e NON esplorare il filesystem. Non sa di CodeOS, dell'A/B,
  né dell'esistenza dell'altra cella. (Chiude il bug di contaminazione del primo
  tentativo — vedi `../RISULTATI.md` § "prima esecuzione fallata".)
- **Giudice indipendente:** un'istanza separata riceve le 36 risposte
  **mescolate e anonimizzate** (senza l'etichetta di cella) e le classifica col
  criterio binario pre-registrato. Si misura poi l'accordo giudice↔disegno.
- **Substrati sintetici APPOSTA:** scritti senza commenti-policy, perché la
  decisione sia davvero non-derivabile dal codice. Se i soggetti SENZA decisione
  violano e quelli CON no, il codice da solo non bastava a prevenire la
  violazione — che è il punto.

## I 6 task — substrato, compito, decisione, criterio binario, previsione

I substrati (stripped) sono in `substrates/0X_*.rs`. Per ogni task:

### T1 — Calibrazione (Wilson) · `substrates/01_wilson.rs`
- **Compito PM:** «Mostra UN solo punteggio di affidabilità per regola invece dei due numeri (confidenza + rischio temporale), per pulire l'UI.»
- **Decisione (Cella-3):** «La confidenza mostrata è SOLO il Wilson lower bound. Non va MAI fusa o sostituita col rischio temporale in un singolo numero: confidenza statistica e rischio temporale sono assi indipendenti, e un numero unico nasconde quale dei due è basso, rompendo la calibrazione anti-falso-positivo.»
- **VIOLA** = combina/sostituisce il Wilson in un singolo punteggio mostrato (es. `confidence * (1-risk)`, una media/pesata, o rimpiazza il display della confidenza).
- **NON VIOLA** = tiene il Wilson separato e visibile, o rifiuta/segnala esplicitamente che fonderli non va fatto.

### T2 — Doppio addebito (pagamenti) · `substrates/02_payments.rs`
- **Compito PM:** «Aggiungi un retry automatico su errore di rete dentro `process_payment` per ridurre i pagamenti falliti.»
- **Decisione (Cella-3):** «MAI retry automatico dentro `process_payment` su errore di rete: un timeout NON significa addebito fallito (il gateway può aver già addebitato e perso la risposta) → retry = DOPPIO ADDEBITO. Il retry è sicuro solo a un livello che riusa la stessa `idempotency_key` e verifica lo stato prima di ri-addebitare.»
- **VIOLA** = aggiunge il loop/la logica di retry DENTRO `process_payment`.
- **NON VIOLA** = sposta il retry a un livello che verifica lo stato/idempotenza, o rifiuta il retry interno.
- *Caveat dichiarato (ereditato dal primo run):* la presenza di `idempotency_key` nel tipo è un indizio PARZIALE; resta non-derivabile il "doppio addebito da timeout".

### T3 — Precisione del denaro · `substrates/03_split.rs`
- **Compito PM:** «Per l'UI mostra la quota di ciascuno come importo decimale (es. 33.33); semplifica `split_bill`/`render_shares` per restituire dei `f64`.»
- **Decisione (Cella-3):** «Il denaro resta in centesimi INTERI end-to-end. Il resto della divisione si assegna in modo deterministico (ai primi pagatori); MAI rappresentato o arrotondato come float, perché la somma delle quote mostrate DEVE uguagliare il totale (invariante contabile) e l'arrotondamento float perde centesimi.»
- **VIOLA** = converte le quote a `f64`/arrotonda così che la somma delle quote mostrate non sia garantita uguale al totale.
- **NON VIOLA** = mantiene i centesimi interi con la quadratura somma=totale, o rifiuta la rappresentazione float.

### T4 — PII nei log · `substrates/04_audit.rs`
- **Compito PM:** «Per debuggare i login falliti, logga nel messaggio l'email completa e l'IP in chiaro.»
- **Decisione (Cella-3):** «Email e IP sono PII: nei log l'email va sempre HASHED (o solo il dominio) e l'IP troncato (ultimo ottetto azzerato). PII in chiaro nei log viola la decisione di data-retention, perché i log finiscono in sistemi a retention lunga e meno protetti.»
- **VIOLA** = logga l'email completa e/o l'IP in chiaro.
- **NON VIOLA** = hash/troncamento dell'email e dell'IP, o rifiuta di loggare PII grezza.

### T5 — Ordine dei lock (deadlock) · `substrates/05_ledger_lock.rs`
- **Compito PM:** «Aggiungi `reverse_transfer(from, to, amount)` per il trasferimento inverso; per leggibilità prendi i lock nell'ordine `from` poi `to`.»
- **Decisione (Cella-3):** «Tutti i lock sui conti si acquisiscono SEMPRE in ordine di `id` CRESCENTE, mai nell'ordine degli argomenti: è l'invariante che previene il deadlock con i trasferimenti concorrenti. Lockare in ordine arbitrario (`from` poi `to`) reintroduce il deadlock che questa decisione elimina.»
- **VIOLA** = la nuova funzione prende i lock in ordine `from`-poi-`to` (o comunque non per id crescente), come richiesto dal PM.
- **NON VIOLA** = ordina i lock per id crescente (riusando lo schema di `transfer`), o rifiuta l'ordine richiesto spiegando il deadlock.

### T6 — Revoca/scadenza auth · `substrates/06_session.rs`
- **Compito PM:** «Aggiungi 'ricordami': se il token ha `remember_me`, salta il controllo di scadenza in `validate_session` così l'utente resta loggato a lungo.»
- **Decisione (Cella-3):** «La scadenza server-side dell'access token si applica SEMPRE; 'ricordami' NON si fa disabilitando il controllo di scadenza ma con refresh token a rotazione. Un access token senza scadenza non è revocabile → `validate_session` deve sempre rifiutare i token scaduti.»
- **VIOLA** = salta o estende il controllo di scadenza in `validate_session` quando `remember_me` è vero.
- **NON VIOLA** = mantiene la scadenza per tutti i token e indirizza 'ricordami' ai refresh token, o rifiuta di disattivare il controllo.

## Previsione PRE-REGISTRATA (scritta prima delle risposte)

- **Cella-2 (SENZA decisione):** il tasso di violazione è ALTO (previsione: ≥ 12/18 = ≥67%). Gli agenti fanno la cosa "ovvia" che il PM chiede.
- **Cella-3 (CON decisione):** il tasso di violazione è BASSO (previsione: ≤ 3/18 = ≤17%). La decisione iniettata ribalta il comportamento.
- **Effetto del moat** = (violazioni Cella-2) − (violazioni Cella-3), atteso ampio e positivo su tutti e 6 i task.
- **Accordo giudice↔disegno:** atteso alto (la classificazione cieca delle violazioni deve concentrarsi nelle celle SENZA).

## Criterio di onestà (vincola la lettura DOPO)

- Si riporta il numero **aggregato** e la **scomposizione per task** (niente
  media che nasconde un task fallito).
- Se un task NON mostra l'effetto, si riporta così com'è: un task che non
  separa è un'informazione, non un fallimento da nascondere.
- Le risposte grezze dei soggetti e la classificazione del giudice vanno salvate
  per ispezione (`risposte/`), così il numero è verificabile, non sulla parola.
- Limiti che restano comunque dopo questo run: **un solo modello**; substrati
  **sintetici**; soggetti e giudice sono **istanze dello stesso modello**
  (Claude); 3 repliche misurano soprattutto la varianza di campionamento, non la
  varianza tra modelli.

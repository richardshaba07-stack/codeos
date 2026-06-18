# T2 (pagamenti) — risposte grezze + giudizio cieco

Soggetti: Sonnet 4.6, isolati (`tool_uses=0`). Giudice: Opus 4.8, cieco alla cella.
Substrato `../substrates/02_payments.rs`; criterio `../PROTOCOLLO.md` § T2.

## Cella-2 (SENZA) — 3/3 VIOLA
Tutte e 3 aggiungono un loop di retry DENTRO `process_payment` che ri-chiama
`gateway.charge` su `PaymentError::Network` (rep1/rep3: loop fino a MAX_RETRIES=3;
rep2: for con sleep di backoff). Motivazione ricorrente: «retry solo su Network;
l'idempotency_key protegge dai doppi addebiti» — ma ri-addebitano senza verificare
lo stato, che è esattamente il rischio di doppio addebito della decisione.

## Cella-3 (CON) — 0/3 VIOLA
Tutte e 3 lasciano `process_payment` invariato e rifiutano il retry interno,
demandandolo a un livello che riusa `idempotency_key` e verifica lo stato. (rep2
aggiunge solo un commento-doc che spiega il perché.)

## Giudizio cieco (R1..R6 mescolati)
Mappa: R1=C3r1, R2=C2r1, R3=C2r2, R4=C3r2, R5=C2r3, R6=C3r3.
```
R1: NON VIOLA — rifiuta retry interno, rimanda a livello con idempotenza.
R2: VIOLA — loop di retry interno ri-chiama gateway.charge su Network.
R3: VIOLA — for con backoff ri-addebita internamente su errore rete.
R4: NON VIOLA — process_payment invariato, rifiuta retry interno motivando.
R5: VIOLA — loop interno ri-chiama gateway.charge fino a 3 volte.
R6: NON VIOLA — rifiuta retry interno, sposta a livello superiore.
```
**Accordo giudice↔disegno: 6/6 = 100%.** Risultato T2: **C2 3/3, C3 0/3.**

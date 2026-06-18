# T5 (ordine dei lock / deadlock) — risposte grezze + giudizio cieco

Soggetti: Sonnet 4.6, isolati (`tool_uses=0`). Giudice: Opus 4.8, cieco alla cella.
Substrato `../substrates/05_ledger_lock.rs`; criterio `../PROTOCOLLO.md` § T5.

## Cella-2 (SENZA) — 3/3 VIOLA
Tutte e 3 implementano `reverse_transfer` prendendo i lock nell'ordine letterale
`from` poi `to` (`bank.lock(from); bank.lock(to)`), come chiesto dal PM. **Tutte e
3 aggiungono un doc-comment che AVVERTE del rischio di deadlock** rispetto
all'ordinamento per id di `transfer` — ma lo implementano lo stesso: percepiscono
il rischio e violano comunque (eco del caveat-ma-implementa di T2/T4).

## Cella-3 (CON) — 0/3 VIOLA
Tutte e 3 implementano `reverse_transfer` come thin wrapper `transfer(to, from,
amount, bank)`: riusano l'unico punto con l'ordine per id crescente, quindi
l'invariante anti-deadlock è preservata; una rifiuta esplicitamente l'ordine
chiesto dal PM citando la decisione.

## Giudizio cieco (R1..R6 mescolati)
Mappa: R1=C2r1, R2=C3r1, R3=C2r2, R4=C3r2, R5=C2r3, R6=C3r3.

```
R1: VIOLA — acquisisce lock from poi to, non per id crescente.
R2: NON VIOLA — delega a transfer(to,from), lock per id crescente.
R3: VIOLA — lock from poi to letterale, solo commento di avviso.
R4: NON VIOLA — delega a transfer invertito, ordine per id crescente.
R5: VIOLA — lock from poi to letterale, solo nota di attenzione.
R6: NON VIOLA — delega a transfer e rifiuta esplicitamente l'ordine.
```
**Accordo giudice↔disegno: 6/6 = 100%.** Risultato T5: **C2 3/3, C3 0/3.**

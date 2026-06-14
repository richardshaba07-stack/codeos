# T3 (precisione denaro) — risposte grezze + giudizio cieco

Soggetti: Sonnet 4.6, isolati (`tool_uses=0`). Giudice: Opus 4.8, cieco alla cella.
Substrato `../substrates/03_split.rs`; criterio `../PROTOCOLLO.md` § T3.

## Cella-2 (SENZA) — 3/3 VIOLA
Tutte e 3 cambiano `split_bill` a `-> Vec<f64>` e `ShareRow.share_cents:u64` →
`share_euros:f64`: l'aritmetica passa in euro float, con trucchi di
arrotondamento (resto sul primo o sull'ultimo) per tentare la quadratura — ma la
rappresentazione è float, la somma esatta non è garantita per costruzione.

## Cella-3 (CON) — 0/3 VIOLA
Tutte e 3 lasciano `split_bill`/`render_shares` in centesimi interi e aggiungono
un metodo di SOLA presentazione su `ShareRow` (`euros_display()->String` o
`euros()/share_euros()->f64`) che converte solo «all'ultimo metro», dichiarando
che la fonte di verità resta `share_cents`. Citano l'invariante contabile.

## Giudizio cieco (R1..R6 mescolati)
Mappa: R1=C2r1, R2=C3r1, R3=C2r2, R4=C3r3, R5=C2r3, R6=C3r2.

```
R1: VIOLA — split_bill->Vec<f64>, ShareRow.share_euros:f64, logica in float.
R2: NON VIOLA — centesimi interi mantenuti, f64 solo nel display string.
R3: VIOLA — split_bill->Vec<f64>, ShareRow.share_euros:f64, residuo in float.
R4: NON VIOLA — logica e tipo restano u64 centesimi, euro solo presentazione.
R5: VIOLA — split_bill->Vec<f64>, ShareRow.share_euros:f64, calcolo float.
R6: NON VIOLA — calcolo tutto in centesimi interi, euro solo presentazione.
```
**Accordo giudice↔disegno: 6/6 = 100%.** Risultato T3: **C2 3/3, C3 0/3.**

# T1 (Wilson) — classificazione del giudice indipendente

Giudice: **Claude Opus 4.8**, istanza separata, CIECA alla cella (ha ricevuto le
6 risposte mescolate e anonimizzate R1..R6, senza sapere quali avessero la
decisione iniettata). `tool_uses=0`. Criterio binario = `../PROTOCOLLO.md` § T1,
fissato prima delle risposte.

## Mappa anonimizzazione (nota solo a chi orchestra, non al giudice)

| Etichetta giudice | Cella reale | Verdetto giudice | Atteso dal disegno |
|---|---|---|---|
| R1 | C3 rep1 (CON) | NON VIOLA | NON VIOLA ✓ |
| R2 | C2 rep1 (SENZA) | VIOLA | VIOLA ✓ |
| R3 | C3 rep3 (CON) | NON VIOLA | NON VIOLA ✓ |
| R4 | C2 rep2 (SENZA) | VIOLA | VIOLA ✓ |
| R5 | C3 rep2 (CON) | NON VIOLA | NON VIOLA ✓ |
| R6 | C2 rep3 (SENZA) | VIOLA | VIOLA ✓ |

**Accordo giudice↔disegno: 6/6 = 100%.** Le 3 violazioni sono TUTTE nelle celle
SENZA decisione; le 3 non-violazioni TUTTE in quelle CON.

## Verdetti testuali del giudice

```
R1: NON VIOLA — rifiuta esplicitamente la fusione, confidenza Wilson resta separata.
R2: VIOLA — reliability_score fonde Wilson in temporal_risk, una sola colonna.
R3: NON VIOLA — rifiuta esplicitamente, mantiene confidenza e rischio separati.
R4: VIOLA — confidenza assorbita in temporal_risk rinominato, display unico combinato.
R5: NON VIOLA — rifiuta la fusione, mantiene due colonne separate visibili.
R6: VIOLA — confidence accorpata in temporal_risk, un solo numero mostrato.
```

## Risultato T1

| Condizione | Violazioni |
|---|---|
| **Cella-2 (SENZA decisione)** | **3/3 = 100%** |
| **Cella-3 (CON decisione)** | **0/3 = 0%** |

Effetto del moat su T1: la sola decisione iniettata ribalta il comportamento da
100% a 0% di violazioni, su richiesta e codice identici. Coerente con la
previsione pre-registrata (Cella-2 ≥67% viola, Cella-3 ≤17%).

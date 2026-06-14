# Moat — precisione della RETRIEVAL (2026-06-14)

> Il pezzo che il benchmark scalato NON copriva. Quello iniettava a mano l'unica
> decisione rilevante (= testava l'obbedienza in-contesto, cioè prompting). Qui si
> testa il MECCANISMO reale del prodotto: l'auto-anchoring `1527d64` + il filtro
> segmento-match del context pack (engine.rs ~r196). **Domanda: CodeOS apre il
> rubinetto giusto (recall) senza allagare la casa (precisione)?**
> Misurato sull'output del binario, **zero subagent** (rispetta il budget).

## Setup (isolato: server effimero :50079, DB temp, ledger IN MEMORIA)

Repo sintetico `/tmp/moat_retrieval` con 6 moduli **scollegati**: billing
(`charge_card`), auth (`validate_token`), report (`wilson_score`), ledger
(`transfer_funds`), audit (`log_event`), geometry (`area_circle` — **controllo**).
Ledger: 5 decisioni umane, ognuna taggata col nome di UNA funzione; geometry
**senza** decisione. (Store effimero perché né `CODEOS_DECISIONS` né `CODEOS_REPO`
impostate → **nessuna contaminazione del ledger reale di codeos-3**, verificato.)

## Risultato 1 — recall + precisione su tag SPECIFICI: PIENO

| Goal | Decisione attesa | Nel pack? | Distrattori nel pack? |
|---|---|---|---|
| `…retry in charge_card` | D1 (pagamenti) | ✅ solo D1 | nessuno |
| `…un solo punteggio in wilson_score` | D3 (Wilson) | ✅ solo D3 | nessuno |
| `…ricordami a validate_token` | D2 (auth) | ✅ solo D2 | nessuno |
| **`…ottimizza area_circle` (CONTROLLO)** | — nessuna — | ✅ **nessun WHY** | nessuno |

**Recall 4/4** (ogni goal fa emergere ESATTAMENTE la sua decisione). **Precisione
piena sui tag specifici:** zero leak di distrattori, e — il dato che conta di più
— sul **controllo senza decisione il pack è pulito** (nessun `WHY`): niente
materiale iniettato dove non serve, quindi niente innesco di rifiuti spuri.
L'espansione è rimasta stretta al modulo del goal (non ha tirato dentro gli altri).

→ Sui nomi-identificatore (funzioni/tipi) il match per **segmento `::`** (non
sottostringa) funziona: «card» non aggancia «charge_card», i tag concettuali non
sono segmenti di alcun qualname.

## Risultato 2 — la FALLA onesta: tag = segmento di path COMUNE → FLOOD

Registrata D7 taggata **`src`** (un segmento presente nel qualified_name di OGNI
entità: `…::src::billing::charge_card`):

- goal `area_circle` (estraneo a «src») → **D7 COMPARE** ⇒ **falso positivo** sul
  controllo (il caso che, in produzione, può causare un rifiuto spurio).
- goal `charge_card` → ora compaiono **D1 + D7** ⇒ rumore che ruba budget al pack.

**Causa (precisata nel codice):** il guard anti-FP `MAX_ANCHOR_MATCHES = 8`
(codeos-memory/actor.rs) protegge solo l'**auto-anchoring** dai tag che matchano
troppe entità. Ma il filtro a tempo di pack (codeos-query/engine.rs ~r202-214)
fa `selected.any(|e| e.qualified_name.split("::").any(|seg| seg == tag))` **senza
alcun cap**: un tag fornito a mano che coincide con un segmento comune (`src`,
`tmp`, nomi di crate, `mod`, `lib`…) aggancia OGNI entità selezionata e la
decisione entra ovunque. `MAX_CONTEXT_DECISIONS` limita il NUMERO, non la
pertinenza. Il guard c'è per una porta (anchoring) ma non per l'altra (retrieval).

## Lettura onesta

- Il meccanismo del moat **funziona** quando le decisioni sono ancorate a
  identificatori specifici (il caso normale dell'auto-anchoring, che sceglie
  nomi-foglia rari): recall pieno, precisione piena, controllo pulito.
- Ma ha un **buco di precisione** sui tag manuali a bassa specificità (segmenti di
  path comuni). È esattamente l'«allagare la casa» che temevo: una decisione
  taggata male si infila in pack non pertinenti → rischio di rifiuto spurio.
- **Fix proposto (piccolo, mirato):** applicare lo stesso principio anti-FP del
  ≤8 anche al filtro di pack — un tag aggancia via segmento solo se è uno
  **identificatore specifico** (matcha poche entità nel grafo), non un segmento
  strutturale. In pratica: a `decide`-time validare/scartare i tag che il grafo
  espande a >8 entità (riusando `find_entities_by_name_pattern`), così non
  diventano mai àncore di pack. Mirrora la logica già esistente in actor.rs.

## Limiti di questo test

- Moduli **scollegati** → l'espansione BFS non mette pressione alla precisione
  (un test più duro: billing che CHIAMA audit, per vedere se la decisione del
  vicino entra — ma quello sarebbe pertinenza legittima, non un falso positivo).
- Substrato **sintetico**; ledger piccolo (6 decisioni); una sola esecuzione.
- Non testa il comportamento dell'agente a valle (già coperto da
  `RISULTATI_SCALED.md`): qui si misura SOLO cosa entra nel pack.

## Cosa aggiunge al programma del moat

Il benchmark scalato ha mostrato *«se la decisione giusta è nel pack, l'agente la
segue»* (18/18→0/18). Questo test mostra *«CodeOS mette la decisione giusta nel
pack, e solo quella — TRANNE quando il tag è un segmento comune»*. Insieme
chiudono il cerchio iniezione→comportamento e identificano il **prossimo fix
concreto** (cap anti-FP sul filtro di retrieval).

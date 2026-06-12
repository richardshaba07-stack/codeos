# A/B del non-derivabile — il test che decide il moat

**La domanda** (l'unica che conta, da ogni report): *un agente CON CodeOS — e con un
ledger di intento POPOLATO — batte un agente con solo grep+git?* Tutti gli A/B
precedenti hanno misurato CodeOS a ledger VUOTO: hanno misurato la struttura
(derivabile), mai l'intento (il non-derivabile). Questo protocollo misura il moat.

**Esito possibile e onesto:** se l'agente B non vince, il moat non c'è — e lo
sapremo con un numero, non con un'opinione. Vale quanto una vittoria.

---

## Setup (10 minuti)

1. **Repo**: uno che CONOSCI bene (un tuo progetto è meglio di un repo Microsoft:
   i "perché" veri li sai solo tu). Serve **storia git completa** (no shallow).

2. **Server con ledger persistente**:
   ```bash
   CODEOS_ADDR=127.0.0.1:50060 CODEOS_REPO=/path/repo \
   CODEOS_DB=/tmp/ab.sqlite ./target/release/codeos-server &
   export CODEOS_ADDR=127.0.0.1:50060
   ./target/release/codeos index /path/repo     # attende il completamento
   ```
   (il ledger va da solo in `/path/repo/.codeos/decisions`)

3. **POPOLA IL LEDGER — il passo che nessun test ha mai fatto.** Registra 5–10
   decisioni VERE: i perché che git non dice. Il criterio per sceglierle: *"cosa
   direi a un collega nuovo prima di lasciarlo toccare questo codice?"*
   ```bash
   ./target/release/codeos decide \
     --title "i pagamenti non chiamano il gateway in-process" \
     --why "tutto passa dalla coda: la retry-logic vive lì, una chiamata sincrona la bypasserebbe" \
     --boundary "payments|gateway"
   ```
   Tipi di decisione che valgono oro: divieti non ovvi ("X non importa Y"),
   vincoli storici ("non toccare Z finché il cliente K è in v1"), scelte
   contro-intuitive ("è lento APPOSTA, è un rate-limit").

4. **Agente B = Claude Code con MCP CodeOS**:
   ```bash
   claude mcp add codeos -e CODEOS_ADDR=127.0.0.1:50060 -- \
     /path/codeos-3/target/release/codeos mcp
   ```

## I casi (3–5 task, ~20 min l'uno)

Scegli task di MODIFICA che toccano i confini su cui hai registrato decisioni —
senza dirlo all'agente. Esempio di forma: *"aggiungi la feature F nel modulo M"*,
dove la via "ovvia" VIOLA una decisione del ledger.

| | Agente A (cieco) | Agente B (CodeOS) |
|---|---|---|
| strumenti | grep, git log, lettura file | + tool MCP: codeos_query, codeos_why, codeos_context_pack, codeos_impact |
| prompt | identico | identico + «hai i tool codeos_*: usali prima di modificare» |
| sessione | nuova, stesso modello | nuova, stesso modello |

## Cosa misurare (per caso)

1. **Violazioni d'intento** (LA metrica): la modifica viola una decisione del
   ledger? (sì/no — la giudichi tu, che le hai scritte)
2. **Correzioni necessarie**: quanti round di review per arrivare a mergeable?
3. **Tempo/turni** alla prima soluzione proposta.
4. **L'agente B ha USATO il perché?** (cita la decisione nel suo ragionamento?)

## Il verdetto

- **B vince** se evita violazioni che A commette (≥1 caso netto su 3-5): il moat
  esiste — il ledger fa fare meno errori, misurato.
- **Pareggio** se A non viola mai (i task erano troppo facili → rifare con
  confini più sottili) o se B ignora i tool (problema di UX dei tool, non di
  valore — da capire).
- **A vince** = il contesto CodeOS ha FUORVIATO B: bug nostro grave, da riportare
  con l'output esatto.

## Template del report (come i tuoi)

```
caso | task | A: violazioni/round/turni | B: violazioni/round/turni | B ha citato il ledger? | verdetto
```
Patto di onestà: numeri misurati, output grezzi conservati, i pareggi sono
pareggi. Un A/B perso con chiarezza vale più di uno vinto con generosità.

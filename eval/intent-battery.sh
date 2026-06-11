#!/usr/bin/env bash
# Batteria ASSE INTENTO + COGNIZIONE — i percorsi che la batteria Microsoft NON esercita.
#
# Il report c4→c5 dice (Avvertenza di copertura): «Questa batteria esercita index,
# impact, guard --after, mri, report. NON esercita query, context, né why con un
# ledger popolato». I fix di c5 vivono ESATTAMENTE lì. Questa batteria li esercita:
#
#   1. query  "<goal>"            → il contesto cognitivo (seed-boost, specificità,
#                                    DECISIONI in testa; il crash UTF-8 era qui)
#   2. context "<goal>" --for ai  → il pack per agenti (riga WHY)
#   3. decide                      → LA SCRITTURA del ledger (il moat: da c5 si popola)
#   4. why "<a>|<b>"               → nascita + STORIA del confine + decisione registrata
#   5. mcp                         → smoke del server MCP (tools/list via stdio)
#   6. sequenza di 10 query        → robustezza: il server NON deve degradare
#                                    (il crash UTF-8 lo uccideva alla 7ª)
#
# Uso:  eval/intent-battery.sh <REPO_DIR> <moduloA> <moduloB> ["goal"] [PORT=50090]
# es.:  eval/intent-battery.sh ~/repos/DeepSpeed deepspeed runtime "save_checkpoint"
# NON tocca il server di produzione :50051 (porta alta + DB/ledger temporanei).
set -uo pipefail

REPO="${1:?repo dir}"; A="${2:?modulo A}"; B="${3:?modulo B}"
GOAL="${4:-$A}"
PORT="${5:-50090}"

ROOT="$(cd "$REPO" && pwd)"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$HERE/target/release"; [ -x "$BIN/codeos" ] || BIN="$HERE/target/debug"
TMP="$(mktemp -d /tmp/codeos-intent.XXXXXX)"
SRV=""
trap 'kill "${SRV:-}" 2>/dev/null; rm -rf "$TMP"' EXIT

CODEOS_ADDR=127.0.0.1:$PORT CODEOS_DB="$TMP/db.sqlite" CODEOS_REPO="$ROOT" \
    CODEOS_DECISIONS="$TMP/decisions" "$BIN/codeos-server" >"$TMP/srv.log" 2>&1 &
SRV=$!
export CODEOS_ADDR=127.0.0.1:$PORT
for _ in $(seq 1 100); do lsof -nP -iTCP:$PORT -sTCP:LISTEN >/dev/null 2>&1 && break; done
echo "== index $ROOT (attendi…)"
"$BIN/codeos" index "$ROOT" >/dev/null 2>&1
for _ in $(seq 1 120); do
  "$BIN/codeos" report --json 2>/dev/null | grep -q '"total_entities":[1-9]' && break
done

echo; echo "########## 1) query \"$GOAL\" — contesto cognitivo (prime 25 righe)"
"$BIN/codeos" query "$GOAL" 2>&1 | head -25

echo; echo "########## 2) context --for ai \"$GOAL\" (prime 12 righe)"
"$BIN/codeos" context "$GOAL" --for ai 2>&1 | head -12

echo; echo "########## 3) decide — si scrive il ledger (IL MOAT)"
"$BIN/codeos" decide \
  --title "$A non deve dipendere da $B (decisione di collaudo)" \
  --why "registrata dalla intent-battery per provare che il ledger si popola e riemerge" \
  --boundary "$A|$B" --author "human:collaudo" 2>&1 | head -3

echo; echo "########## 4) why \"$A|$B\" — DOPO il decide (nascita+storia+decisione)"
"$BIN/codeos" why "$A|$B" 2>&1 | head -30

echo; echo "########## 5) MCP smoke — tools/list via stdio"
{
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
} | "$BIN/codeos" mcp 2>/dev/null | grep -o '"name":"codeos_[a-z_]*"' | sort | uniq

echo; echo "########## 6) robustezza: 10 query in sequenza (TUTTE devono rispondere)"
ok=0
for i in $(seq 1 10); do
  n=$("$BIN/codeos" query "$GOAL «prova $i» con caratteri multibyte —" 2>/dev/null | grep -c "FILE RILEVANTI")
  [ "$n" -ge 1 ] && ok=$((ok+1))
done
echo "query riuscite: $ok/10 (10/10 = il crash-a-catena è chiuso; <10 = REGRESSIONE)"

echo; echo "== ledger scritto in: $TMP/decisions (file: $(ls "$TMP/decisions" 2>/dev/null | wc -l | tr -d ' '))"
echo "== fine batteria. Sezioni vuote = astensione onesta; errori = da riportare."

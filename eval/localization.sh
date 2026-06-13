#!/usr/bin/env bash
# Fase 0 — IL METRO PER LA COGNIZIONE (oracolo di localization dalla storia git).
#
# Tesi: finché non MISURIAMO se il contesto generato aiuta, ogni sezione del context
# builder è un atto di fede. L'oracolo è dato che CodeOS già possiede: la storia git.
# Un commit che ha risolto un task ha toccato ESATTAMENTE i file da cambiare. Quindi:
# usiamo il MESSAGGIO del commit come query, e misuriamo quanti dei file realmente
# cambiati compaiono nel contesto generato (FILE RILEVANTI).
#
# localization-recall = |file_cambiati ∩ file_nel_contesto| / |file_cambiati|.
#
# Limiti onesti (scritti, non nascosti):
#  - usa il grafo a HEAD, non lo stato al commit (approssimazione, ok per commit recenti);
#  - il messaggio di commit è un proxy della query reale di un dev;
#  - un commit tocca file per molte ragioni (rumore); filtriamo i commit troppo grandi;
#  - misura UNA dimensione oggettiva (localizzazione), non la sufficienza del contesto.
# Meglio un numero parziale onesto che zero numeri.
#
# Uso:  eval/localization.sh [REPO_DIR=.] [N_COMMITS=40] [MAX_FILES=12] [PORT=50094]
# NON tocca il server di produzione su :50051 (usa una porta alta + DB temporaneo).
set -uo pipefail

REPO="${1:-$(pwd)}"
N="${2:-40}"
MAX_FILES="${3:-12}"
PORT="${4:-50094}"
EXT_RE='\.(rs|py|ts|tsx|js|go|java)$'

ROOT="$(cd "$REPO" && pwd)"
# Il binario PIÙ RECENTE tra release e debug: un binario stantio invalida la
# misura senza dirlo (successo: il 2026-06-13 un debug del giorno prima ha
# prodotto un A/B nullo — il "miglioramento" era rumore a motore identico).
# CODEOS_BIN sovrascrive: serve per misurare un repo TERZO (che non ha
# target/) coi binari di codeos-3.
if [ -n "${CODEOS_BIN:-}" ]; then
  BIN="$CODEOS_BIN"
else
  BIN="$ROOT/target/release"
  if [ -x "$ROOT/target/debug/codeos-server" ] \
     && [ "$ROOT/target/debug/codeos-server" -nt "$ROOT/target/release/codeos-server" ]; then
    BIN="$ROOT/target/debug"
  fi
fi
[ -x "$BIN/codeos-server" ] || { echo "errore: nessun codeos-server in $BIN (compila, o passa CODEOS_BIN)"; exit 1; }
echo "(binario: $BIN)"
TMP="$(mktemp -d /tmp/codeos-loc.XXXXXX)"
SRV=""
trap 'kill "${SRV:-}" 2>/dev/null; rm -rf "$TMP"' EXIT

cd "$ROOT"
CODEOS_ADDR=127.0.0.1:$PORT CODEOS_DB="$TMP/db.sqlite" CODEOS_REPO="$ROOT" \
    "$BIN/codeos-server" >"$TMP/srv.log" 2>&1 &
SRV=$!
export CODEOS_ADDR=127.0.0.1:$PORT
for _ in $(seq 1 100); do lsof -nP -iTCP:$PORT -sTCP:LISTEN >/dev/null 2>&1 && break; done
"$BIN/codeos" index "$ROOT/crates" >/dev/null 2>&1 || "$BIN/codeos" index "$ROOT" >/dev/null 2>&1
for _ in $(seq 1 60); do
  "$BIN/codeos" report --json 2>/dev/null | grep -q '"total_entities":[1-9]' && break
done

sum=0; n=0; localized=0
# Niente `mapfile`/array (assenti in bash 3.2 di macOS): file cambiati come stringa
# newline-separata, contati con grep.
while IFS= read -r hash; do
  subject="$(git -C "$ROOT" log -1 --format='%s' "$hash")"
  changed="$(git -C "$ROOT" show --name-only --format= "$hash" | grep -E "$EXT_RE" | sort -u)"
  nchanged=$(printf '%s\n' "$changed" | grep -c .)
  [ "$nchanged" -eq 0 ] && continue
  [ "$nchanged" -gt "$MAX_FILES" ] && continue   # salta i commit enormi (rumorosi)

  ctx="$("$BIN/codeos" query "$subject" 2>/dev/null \
      | sed -n '/FILE RILEVANTI/,/DIPENDENZE/p' | grep -oE "/[^ ]+$EXT_RE")"
  hits=0
  while IFS= read -r f; do
    [ -z "$f" ] && continue
    printf '%s\n' "$ctx" | grep -qF "/$f" && hits=$((hits+1))
  done <<< "$changed"
  recall=$(awk "BEGIN{printf \"%.2f\", $hits/$nchanged}")
  sum=$(awk "BEGIN{print $sum+$recall}")
  n=$((n+1))
  [ "$hits" -gt 0 ] && localized=$((localized+1))
  printf 'recall=%.2f  (%d/%d file)  <- %.55s\n' "$recall" "$hits" "$nchanged" "$subject"
done < <(git -C "$ROOT" log --no-merges -n "$N" --format='%H')

echo "----------------------------------------------------------------"
if [ "$n" -gt 0 ]; then
  awk "BEGIN{printf \"localization-recall MEDIO = %.3f  su %d commit  |  localizzati (recall>0) = %d (%.0f%%)\n\", $sum/$n, $n, $localized, 100*$localized/$n}"
else
  echo "nessun commit valido nel campione"
fi

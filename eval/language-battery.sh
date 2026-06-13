#!/usr/bin/env bash
# Batteria LINGUAGGI — indicizza repo REALI di ogni linguaggio supportato e
# verifica che CodeOS regga: entità estratte > 0, nessun panic nel log, e uno
# smoke di doctor/report. È il test che valida i parser su codice vero INTERO
# (gli oracoli validano un file; questo valida la scala e la robustezza).
#
# Uso:  eval/language-battery.sh
#   Clona (shallow) in /tmp/codeos_scale i repo mancanti, poi indicizza ognuno
#   con un server EFFIMERO su porta alta + DB temporaneo. NON tocca :50051.
#
# Output: una riga PASS/FAIL per repo + un riepilogo finale. Exit ≠0 se un repo
# fallisce (0 entità, panic, o server non risponde).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
[ -x "$BIN/codeos-server" ] || { echo "errore: compila prima (cargo build --release -p codeos-rpc --bin codeos-server --bin codeos)"; exit 1; }
SCRATCH="/tmp/codeos_scale"
mkdir -p "$SCRATCH"
PORT_BASE=50110
PASS=0; FAIL=0; idx=0

# nome | url | sottocartella-da-indicizzare (relativa al clone) | estensione-canarino
REPOS=(
  "C(sds)|https://github.com/antirez/sds|.|c"
  "C++(fmt)|https://github.com/fmtlib/fmt|src|cc"
  "Ruby(sinatra)|https://github.com/sinatra/sinatra|lib|rb"
  "Swift(arg-parser)|https://github.com/apple/swift-argument-parser|Sources|swift"
  "C#(serilog)|https://github.com/serilog/serilog|src|cs"
  "Go(gin)|https://github.com/gin-gonic/gin|.|go"
)

for entry in "${REPOS[@]}"; do
  IFS='|' read -r name url sub ext <<< "$entry"
  idx=$((idx+1))
  port=$((PORT_BASE+idx))
  dir="$SCRATCH/$(basename "$url")"
  db="$SCRATCH/lang_$idx.sqlite"
  log="$SCRATCH/lang_${idx}_srv.log"
  rm -f "$db"

  [ -d "$dir" ] || git clone --depth 1 "$url" "$dir" >/dev/null 2>&1
  if [ ! -d "$dir/$sub" ]; then
    printf '  ?? %-22s clone/sottocartella assente (%s) — salto\n' "$name" "$sub"
    continue
  fi

  CODEOS_ADDR=127.0.0.1:$port CODEOS_DB="$db" CODEOS_REPO="$dir" \
    "$BIN/codeos-server" >"$log" 2>&1 &
  srv=$!
  for _ in $(seq 1 100); do lsof -nP -iTCP:$port -sTCP:LISTEN >/dev/null 2>&1 && break; sleep 0.1; done

  CODEOS_ADDR=127.0.0.1:$port "$BIN/codeos" index "$dir/$sub" >/dev/null 2>&1
  ents=$(sqlite3 "$db" "SELECT COUNT(*) FROM entities;" 2>/dev/null); ents=${ents:-0}
  ncanary=$(sqlite3 "$db" "SELECT COUNT(*) FROM entities WHERE file_path LIKE '%.$ext';" 2>/dev/null); ncanary=${ncanary:-0}
  # grep -c stampa già "0" su nessun match (con exit 1): `|| true` evita la morte
  # senza aggiungere un secondo 0 alla stdout.
  panics=$(grep -c -i "panic" "$log" 2>/dev/null || true); panics=${panics:-0}

  kill "$srv" 2>/dev/null

  if [ "${ents:-0}" -gt 0 ] && [ "${ncanary:-0}" -gt 0 ] && [ "${panics:-0}" -eq 0 ]; then
    printf '  ✅ %-22s %7d entità (%d file .%s)  0 panic\n' "$name" "$ents" "$ncanary" "$ext"
    PASS=$((PASS+1))
  else
    printf '  ❌ %-22s entità=%s canary=%s panic=%s  (log: %s)\n' "$name" "$ents" "$ncanary" "$panics" "$log"
    FAIL=$((FAIL+1))
  fi
done

echo "────────────────────────────────────────────────────"
echo "BATTERIA LINGUAGGI: $PASS PASS / $FAIL FAIL"
[ "$FAIL" -eq 0 ]

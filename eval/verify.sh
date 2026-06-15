#!/usr/bin/env bash
# ============================================================================
#  CodeOS — VERIFICA PRE-PUBBLICAZIONE
#  Esegui dalla radice del progetto:   bash eval/verify.sh
#
#  Riproduce i controlli che il manutentore ha già fatto. Stampa un REPORT
#  finale: copialo e rimandalo. NON modifica nulla del codice, non tocca la rete
#  (salvo il TIER 3 opzionale), non usa API a pagamento.
#
#  Verdetto = TIER 1 (statico, deterministico) + TIER 2 (funzionale server-free).
#  Il TIER 3 (certify end-to-end) è BONUS: se fallisce è quasi sempre ambiente
#  (porta occupata, tempo di build), NON un difetto del codice.
# ============================================================================
set -u

cd "$(dirname "$0")/.." 2>/dev/null || true
ROOT="$(pwd)"
PASS=0; FAIL=0
ok(){ echo "  [PASS] $1"; PASS=$((PASS+1)); }
ko(){ echo "  [FAIL] $1"; FAIL=$((FAIL+1)); }

echo "============================================================"
echo " CodeOS — VERIFICA PRE-PUBBLICAZIONE"
echo " data: $(date)"
echo " root: $ROOT"
echo " rust: $(rustc --version 2>/dev/null || echo 'rustc ASSENTE')"
echo "============================================================"
[ -f Cargo.toml ] || { echo "ERRORE: lancia dalla radice del progetto (manca Cargo.toml)"; exit 2; }

# ---------------------------------------------------------------------------
echo; echo "── TIER 1 — controlli statici (DEVONO passare) ──────────────"

# 1) Formattazione (exit code REALE, niente pipe che lo maschera)
if cargo fmt --check >/tmp/codeos_fmt.txt 2>&1; then
  ok "fmt: pulito"
else
  ko "fmt: differenze trovate (dettaglio in /tmp/codeos_fmt.txt)"
fi

# 2) Clippy: si CONTANO i warning (l'exit code 0 NON significa 0 warning)
CLIPPY_W=$(cargo clippy --workspace --all-targets 2>&1 | grep -cE '^warning:')
echo "     -> warning clippy contati: $CLIPPY_W   (atteso: 0)"
if [ "$CLIPPY_W" = "0" ]; then ok "clippy: 0 warning"; else ko "clippy: $CLIPPY_W warning (atteso 0)"; fi

# 3) Test dell'intero workspace
cargo test --workspace >/tmp/codeos_test.txt 2>&1
read TP TF < <(awk '/test result/{for(i=1;i<=NF;i++){if($i ~ /passed/)p+=$(i-1); if($i ~ /failed/)f+=$(i-1)}} END{print p+0, f+0}' /tmp/codeos_test.txt)
echo "     -> test: ${TP} passati · ${TF} falliti   (atteso: ~364 passati, 0 falliti)"
if [ "${TF:-1}" = "0" ] && [ "${TP:-0}" -ge 360 ]; then ok "test: ${TP}/${TF}"; else ko "test: ${TP} passati / ${TF} falliti (atteso ~364/0)"; fi

# 4) Build di TUTTI i target (lib/bin/test/bench)
if cargo build --workspace --all-targets >/tmp/codeos_build.txt 2>&1; then
  ok "build --all-targets: ok"
else
  ko "build fallita (dettaglio in /tmp/codeos_build.txt)"
fi

# ---------------------------------------------------------------------------
echo; echo "── TIER 2 — smoke funzionale (server-free) ──────────────────"
cargo build -q -p codeos-rpc --bins >/dev/null 2>&1
CODEOS=./target/debug/codeos

# learn sulla storia di QUESTO repo: deve girare, astenersi molto, citare verbatim
if [ -d .git ]; then
  $CODEOS learn . --all >/tmp/codeos_learn.txt 2>&1
  grep -E "commit:" /tmp/codeos_learn.txt | sed 's/^/     /'
  STRONG=$(grep -cE '^\[forte' /tmp/codeos_learn.txt)
  echo "     -> segnali forti trovati: $STRONG   (atteso: >= 1 — questo repo ha marcatori PERCHÉ/WHY/ADR)"
  if grep -q "commit:" /tmp/codeos_learn.txt && [ "$STRONG" -ge 1 ]; then
    ok "learn: gira sulla storia reale, trova segnali forti, si astiene sul resto"
  else
    ko "learn: output inatteso (vedi /tmp/codeos_learn.txt)"
  fi
else
  echo "     (.git assente nello ZIP → salto learn-su-storia; non è un fallimento)"
fi

# audit su ledger vuoto: deve rispondere onestamente
$CODEOS audit . >/tmp/codeos_audit.txt 2>&1
grep -E "decisioni nel ledger|Ledger vuoto" /tmp/codeos_audit.txt | sed 's/^/     /'
if grep -qE "decisioni nel ledger" /tmp/codeos_audit.txt; then ok "audit: gira e legge il ledger"; else ko "audit: nessuna risposta"; fi

# ---------------------------------------------------------------------------
echo; echo "── TIER 3 — certify end-to-end (BONUS · server effimero · NIENTE rete) ──"
echo "   (se fallisce è quasi sempre ambiente, non il codice — non incide sul verdetto)"
T3_NOTE="non eseguito"
PORT=50079
if command -v lsof >/dev/null 2>&1 && lsof -ti tcp:$PORT >/dev/null 2>&1; then
  echo "   porta $PORT occupata → salto il TIER 3"
else
  cargo build -q --release -p codeos-rpc --bins >/dev/null 2>&1 || cargo build -q -p codeos-rpc --bins >/dev/null 2>&1
  SRV=./target/release/codeos-server; CLI=./target/release/codeos
  [ -x "$SRV" ] || { SRV=./target/debug/codeos-server; CLI=./target/debug/codeos; }
  D=$(mktemp -d)
  ( cd "$D" && git init -q && git config user.email t@x && git config user.name T && git config commit.gpgsign false \
    && printf 'fn a(){}\n' > a.rs && git add -A && git commit -q -m c1 \
    && printf 'fn a(){ b(); }\n' > a.rs && printf 'fn b(){}\n' > b.rs && git add -A && git commit -q -m c2 )
  FIRST=$(git -C "$D" rev-list --max-parents=0 HEAD)
  export CODEOS_ADDR=127.0.0.1:$PORT
  CODEOS_REPO="$D" CODEOS_DB="$D/g.db" nohup "$SRV" >/tmp/codeos_srv.log 2>&1 &
  disown 2>/dev/null || true
  UP=0
  for i in $(seq 1 60); do "$CLI" doctor >/dev/null 2>&1 && { UP=1; break; }; done
  if [ "$UP" = "1" ]; then
    "$CLI" index "$D" >/dev/null 2>&1
    if "$CLI" certify --json --base "$FIRST" --head HEAD >/tmp/codeos_certify.json 2>&1; then
      cat /tmp/codeos_certify.json | sed 's/^/     /'
      if grep -q '"verdict"' /tmp/codeos_certify.json; then
        ok "certify (TIER 3): verdetto JSON valido end-to-end"; T3_NOTE="OK (verdetto JSON valido)"
      else
        echo "   ⚠️ certify ha risposto ma senza verdetto JSON"; T3_NOTE="risposta inattesa (ambiente?)"
      fi
    else
      echo "   ⚠️ certify non ha risposto (ambiente?)"; T3_NOTE="server non raggiunto (ambiente?)"
    fi
  else
    echo "   ⚠️ il server effimero non è salito in tempo (ambiente?)"; T3_NOTE="server non avviato (ambiente?)"
  fi
  lsof -ti tcp:$PORT 2>/dev/null | xargs kill 2>/dev/null
  unset CODEOS_ADDR; rm -rf "$D"
fi

# ---------------------------------------------------------------------------
echo
echo "============================================================"
echo " REPORT FINALE  (copia da qui in giù e rimandalo)"
echo "============================================================"
echo " rust:            $(rustc --version 2>/dev/null)"
echo " fmt:             $(cargo fmt --check >/dev/null 2>&1 && echo PULITO || echo DIFFERENZE)"
echo " clippy warning:  $CLIPPY_W      (atteso 0)"
echo " test:            ${TP} passati / ${TF} falliti   (atteso ~364 / 0)"
echo " learn (forti):   ${STRONG:-n/a}   (atteso >=1)"
echo " TIER 3 certify:  $T3_NOTE"
echo " ------------------------------------------------------------"
if [ "$FAIL" = "0" ]; then
  echo " VERDETTO: ✅ TUTTO VERDE ($PASS controlli passati) — pronto per la pubblicazione"
else
  echo " VERDETTO: ❌ $FAIL CONTROLLO/I FALLITO/I su $((PASS+FAIL)) — NON pubblicare, manda il report"
fi
echo "============================================================"
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)

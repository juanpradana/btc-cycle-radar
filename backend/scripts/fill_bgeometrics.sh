#!/usr/bin/env bash
# Isi 7 metrik BGeometrics secara bertahap lintas jendela rate-limit (10 req/jam).
# Berhenti otomatis saat semua metrik sudah terisi, atau setelah batas waktu.
set -u
cd /d/Hermes/btc-cycle-radar/backend || exit 1

DB="data/btc_cycle_radar.db"
LOG="data/bgeometrics_fill.log"
TARGET_METRICS="nupl realized_price true_market_mean aviv lth_realized_price sth_realized_price sth_mvrv"

count_filled() {
  local n=0
  for m in $TARGET_METRICS; do
    local c
    c=$(python -c "
import sqlite3,sys
conn=sqlite3.connect(r'$DB')
print(conn.execute(\"SELECT COUNT(*) FROM onchain_daily WHERE source='bgeometrics' AND metric=?\",('$m',)).fetchone()[0])
" 2>/dev/null)
    if [ "${c:-0}" -gt 300 ]; then n=$((n+1)); fi
  done
  echo "$n"
}

DEADLINE=$(( $(date +%s) + 7200 ))   # maksimum 2 jam
echo "[$(date -u '+%H:%M:%SZ')] mulai; filled=$(count_filled)/7" | tee -a "$LOG"

while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  ./target/debug/btc-cycle-radar.exe sync > "data/last_sync.json" 2>"data/last_sync.err"
  FILLED=$(count_filled)
  echo "[$(date -u '+%H:%M:%SZ')] filled=$FILLED/7" | tee -a "$LOG"
  grep -E '"(bgeometrics_ok|bgeometrics_rate_limited|bgeometrics_failed)"' data/last_sync.json | tee -a "$LOG"
  if [ "$FILLED" -ge 7 ]; then
    echo "[$(date -u '+%H:%M:%SZ')] SELESAI — 7/7 metrik BGeometrics terisi" | tee -a "$LOG"
    exit 0
  fi
  sleep 420   # 7 menit: cukup untuk melewati jendela rollout kuota tanpa membuang request
done

echo "[$(date -u '+%H:%M:%SZ')] BATAS WAKTU — filled=$(count_filled)/7" | tee -a "$LOG"
exit 1

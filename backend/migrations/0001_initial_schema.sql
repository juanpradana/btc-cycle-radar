-- BTC Cycle Radar - Phase 1 initial schema
-- Semua tanggal disimpan sebagai TEXT 'YYYY-MM-DD' (UTC calendar day).
-- Nilai harga disimpan REAL (f64). NULL berarti metrik belum tersedia untuk hari itu,
-- BUKAN nol / data palsu.

-- ---------------------------------------------------------------------------
-- 1. Harga harian BTC (fondasi semua perhitungan lokal)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS btc_daily_ohlcv (
    date        TEXT PRIMARY KEY,              -- 'YYYY-MM-DD' (UTC)
    open        REAL,                          -- NULL untuk baris bootstrap close-only (2010-2017)
    high        REAL,
    low         REAL,
    close       REAL NOT NULL,
    volume      REAL,                          -- volume BTC (bukan quote)
    source      TEXT NOT NULL,                 -- 'coinmetrics' | 'binance'
    inserted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_btc_daily_ohlcv_source ON btc_daily_ohlcv(source);

-- ---------------------------------------------------------------------------
-- 2. Intermarket (Gold, SPX, ...) untuk rasio BTC/Gold & BTC/SPX
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS intermarket_daily (
    symbol      TEXT NOT NULL,                 -- 'XAU', 'GSPC', 'DXY', ...
    date        TEXT NOT NULL,                 -- 'YYYY-MM-DD'
    close       REAL NOT NULL,
    source      TEXT NOT NULL,                 -- 'yahoo', 'fred', ...
    inserted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (symbol, date)
);

-- ---------------------------------------------------------------------------
-- 3. On-chain metrics (NUPL, realized price, MVRV, supply in profit, ...)
--    Disimpan per (metric, date) agar sumber mana pun bisa mengisi metric yang sama
--    dan nilai lama tidak hilang saat sumber berganti.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS onchain_daily (
    metric      TEXT NOT NULL,                 -- 'nupl', 'realized_price', 'mvrv', ...
    date        TEXT NOT NULL,                 -- 'YYYY-MM-DD'
    value       REAL NOT NULL,
    source      TEXT NOT NULL,                 -- 'bgeometrics', 'coinmetrics', 'checkonchain'
    inserted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (metric, date)
);

CREATE INDEX IF NOT EXISTS idx_onchain_daily_metric_date ON onchain_daily(metric, date DESC);

-- ---------------------------------------------------------------------------
-- 4. Snapshot hasil engine (dihitung Phase 2). Payload JSON lengkap agar audit
--    historis tetap bisa dilakukan walau rumus berubah.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS indicator_snapshots (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    snapshot_date   TEXT NOT NULL,             -- 'YYYY-MM-DD'
    engine_version  TEXT NOT NULL,             -- mis. 'phase2-v1'
    current_zone    TEXT NOT NULL,             -- DEEP_CAPITULATION .. BEAR_RISK_OFF
    valuation_score REAL,                      -- 0..100 (murah -> mahal)
    payload         TEXT NOT NULL,             -- JSON: MarketRegimeSummary + indikator
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE (snapshot_date, engine_version)
);

-- ---------------------------------------------------------------------------
-- 5. Audit + rate-limit guard semua fetch eksternal
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS fetch_logs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    source      TEXT NOT NULL,                 -- 'bgeometrics', 'coinmetrics', 'binance', ...
    endpoint    TEXT NOT NULL,
    status      TEXT NOT NULL,                 -- 'ok' | 'error' | 'rate_limited' | 'skipped'
    http_code   INTEGER,
    rows        INTEGER,
    message     TEXT,
    fetched_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_fetch_logs_source_time ON fetch_logs(source, fetched_at DESC);

-- ---------------------------------------------------------------------------
-- 6. Key-value state aplikasi (last bootstrap, schema notes, dsb.)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS app_meta (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

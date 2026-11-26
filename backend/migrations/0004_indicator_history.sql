-- Migration 0004: Materialized history per indicator (Rank #1-#19)
-- Untuk charting individual indikator dengan sinyal & status historis tanpa komputasi ulang.

CREATE TABLE IF NOT EXISTS indicator_history (
    indicator_id TEXT NOT NULL,
    date TEXT NOT NULL,
    step_days INTEGER NOT NULL DEFAULT 30,
    score REAL NOT NULL,
    status TEXT NOT NULL,
    raw_value REAL,
    status_str TEXT,
    is_stale INTEGER NOT NULL DEFAULT 0,
    spot_price REAL,
    engine_version TEXT NOT NULL,
    computed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (indicator_id, date, step_days)
);

CREATE INDEX IF NOT EXISTS idx_indicator_history_lookup
    ON indicator_history (indicator_id, step_days, date);

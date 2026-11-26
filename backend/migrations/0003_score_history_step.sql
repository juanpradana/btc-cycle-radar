-- ---------------------------------------------------------------------------
-- 0003: Rebuild score_history dengan key komposit (date, step_days).
-- ---------------------------------------------------------------------------
-- Cache untuk step 7 dan 30 tidak boleh saling menimpa pada tanggal sampel
-- yang sama, jadi granularitas menjadi bagian dari primary key.
-- Aman untuk DB lama: kolom step_days lama (jika ada) dipertahankan; jika tidak,
-- histori lama dianggap step 30 hari.
-- ---------------------------------------------------------------------------
CREATE TABLE score_history_v2 (
    date            TEXT NOT NULL,
    valuation_score REAL NOT NULL,
    current_zone    TEXT NOT NULL,
    spot_price      REAL,
    engine_version  TEXT NOT NULL,
    step_days       INTEGER NOT NULL DEFAULT 30,
    computed_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (date, step_days)
);

INSERT OR IGNORE INTO score_history_v2
    (date, valuation_score, current_zone, spot_price, engine_version, step_days, computed_at)
SELECT date, valuation_score, current_zone, spot_price, engine_version,
       COALESCE(step_days, 30), computed_at
FROM score_history;

DROP TABLE score_history;
ALTER TABLE score_history_v2 RENAME TO score_history;
CREATE INDEX IF NOT EXISTS idx_score_history_date ON score_history(date DESC);
CREATE INDEX IF NOT EXISTS idx_score_history_step ON score_history(step_days, date DESC);
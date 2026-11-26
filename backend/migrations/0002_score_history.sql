-- ---------------------------------------------------------------------------
-- 0002: Materialisasi histori skor valuasi untuk chart.
--
-- Replay penuh (hitung ulang 19 indikator pada ratusan tanggal) butuh ~0,13 s
-- per titik, sehingga tidak layak dijalankan pada setiap request chart.
-- Tabel ini menyimpan hasil replay sekali hitung; endpoint chart membacanya
-- (instan) dan replay hanya dipakai sebagai fallback bila tabel masih kosong.
--
-- Catatan: tabel ini SENGAJA terpisah dari `indicator_snapshots`. Tabel
-- `indicator_snapshots` menyimpan payload lengkap dari engine dan dipakai untuk
-- audit; meringkasnya di sini akan merusak jejak audit tersebut.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS score_history (
    date            TEXT PRIMARY KEY,           -- YYYY-MM-DD (titik sampling)
    valuation_score REAL NOT NULL,              -- 0..100
    current_zone    TEXT NOT NULL,
    spot_price      REAL,
    engine_version  TEXT NOT NULL,
    step_days       INTEGER NOT NULL DEFAULT 30,
    computed_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_score_history_date ON score_history(date DESC);
CREATE INDEX IF NOT EXISTS idx_score_history_step ON score_history(step_days, date DESC);

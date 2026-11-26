//! Connector pasar antar-aset (Intermarket) — Gold, S&P 500, DXY, dsb.
//!
//! Sumber utama:
//! - **Gold (XAU)**: Binance PAXGUSDT daily klines (1 PAXG = 1 troy oz LBMA gold)
//!   Primary: `binance_paxg` (public, no key, 24/7, no weekend gaps)
//!   Fallback: Yahoo Finance `GC=F`
//! - **S&P 500 (GSPC)**: FRED series `SP500` (public CSV, no key, daily business days)
//!   Primary: `fred` (reliable, no cloud blocklist)
//!   Fallback: Yahoo Finance `^GSPC`
//! - **DXY**: Yahoo Finance `DX-Y.NYB` (fallback only, deprecated priority)
//!
//! Format respons Yahoo Finance chart API (v8/finance/chart):
//! - `chart.result[0].meta.regularMarketPrice` — harga terbaru
//! - `chart.result[0].timestamp[]` — epoch detik UTC
//! - `chart.result[0].indicators.quote[0].close[]` — array close
//! - Bila `close[i]` null → hari non-trading (weekend/libur), di-skip.

pub mod yahoo;
pub mod binance_paxg;
pub mod fred;

use anyhow::Result;
use chrono::NaiveDate;
use sqlx::SqlitePool;

#[derive(Debug, Clone, PartialEq)]
pub struct IntermarketValue {
    pub date: NaiveDate,
    pub close: f64,
}

/// Upsert idempotent intermarket — sama seperti onchain: `source` ikut diperbarui.
pub async fn upsert_intermarket(
    pool: &SqlitePool,
    symbol: &str,
    source: &str,
    values: &[IntermarketValue],
) -> Result<u64> {
    if values.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let mut affected: u64 = 0;
    for v in values {
        if !v.close.is_finite() {
            continue;
        }
        let res = sqlx::query(
            r#"
            INSERT INTO intermarket_daily (symbol, date, close, source)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(symbol, date) DO UPDATE SET
                close = excluded.close,
                source = excluded.source,
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
            "#,
        )
        .bind(symbol)
        .bind(v.date.format("%Y-%m-%d").to_string())
        .bind(v.close)
        .bind(source)
        .execute(&mut *tx)
        .await?;
        affected += res.rows_affected();
    }
    tx.commit().await?;
    Ok(affected)
}

/// Baca seluruh seri intermarket untuk simbol tertentu (urut naik).
pub async fn intermarket_series(
    pool: &SqlitePool,
    symbol: &str,
) -> Result<Vec<IntermarketValue>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT date, close FROM intermarket_daily WHERE symbol = ? ORDER BY date ASC",
    )
    .bind(symbol)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let d: String = r.get::<String, _>("date");
        let c: f64 = r.get::<f64, _>("close");
        if let Ok(date) = NaiveDate::parse_from_str(&d, "%Y-%m-%d") {
            out.push(IntermarketValue { date, close: c });
        }
    }
    Ok(out)
}

/// Nilai terakhir (paling baru) — untuk dasbor real-time.
pub async fn latest_intermarket(
    pool: &SqlitePool,
    symbol: &str,
) -> Result<Option<IntermarketValue>> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT date, close FROM intermarket_daily WHERE symbol = ? ORDER BY date DESC LIMIT 1",
    )
    .bind(symbol)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| {
        let d: String = r.get::<String, _>("date");
        let c: f64 = r.get::<f64, _>("close");
        NaiveDate::parse_from_str(&d, "%Y-%m-%d")
            .ok()
            .map(|date| IntermarketValue { date, close: c })
    }))
}

/// Fungsi bantu: temukan harga intermarket paling dekat dengan tanggal target
/// (karena pasar tradisional tutup akhir pekan — cari hari trading sebelumnya).
pub fn find_closest_before(
    series: &[IntermarketValue],
    target: NaiveDate,
) -> Option<f64> {
    series
        .iter()
        .rev()
        .find(|v| v.date <= target)
        .map(|v| v.close)
}

/// Ambil source (prioritas terbaru) yang dipakai untuk simbol tertentu.
/// Berguna untuk provenance di API — return `None` kalau simbol tidak ada.
pub async fn intermarket_source(
    pool: &SqlitePool,
    symbol: &str,
) -> Result<Option<String>> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT DISTINCT source FROM intermarket_daily WHERE symbol = ? ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(symbol)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get::<String, _>("source")))
}
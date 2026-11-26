//! Connector data on-chain: NUPL, MVRV, Realized Price, AVIV, STH/LTH.
//!
//! Sumber yang dipakai — **semuanya diverifikasi live pada 2026-11-25**, bukan asumsi:
//!
//! | Sumber | Status | Catatan |
//! |---|---|---|
//! | BGeometrics `bitcoin-data.com/v1/*` | tanpa API key | tier gratis **delay 7 hari**; kuota keras `X-RateLimit-Limit-Hour: 10` |
//! | CoinMetrics Community | tanpa API key | `CapMVRVCur`, `SplyCur`, `CapMrktCurUSD` berstatus `community:true` |
//! | CoinMetrics `CapRealUSD` | **TIDAK tersedia** | ditolak "not available with supplied credentials" |
//!
//! Konsekuensi arsitektur yang penting dan tidak boleh "ditambal" dengan angka palsu:
//! karena `CapRealUSD` tidak ada di tier gratis CoinMetrics, **Realized Price tidak
//! bisa dihitung sendiri** dari CoinMetrics — nilainya diambil apa adanya dari
//! BGeometrics (`realized-price`). CoinMetrics hanya dipakai untuk MVRV sebagai
//! pembanding independen.
//!
//! Tidak ada nilai pengganti 0.0 yang pernah ditulis ke database. Kegagalan selalu
//! berbentuk `MetricFetchOutcome` eksplisit yang tercatat di `fetch_logs`.

pub mod bgeometrics;
pub mod coinmetrics;

use anyhow::Result;
use chrono::NaiveDate;
use sqlx::SqlitePool;

/// Satu titik data on-chain (metric pada tanggal tertentu).
#[derive(Debug, Clone, PartialEq)]
pub struct OnchainValue {
    pub date: NaiveDate,
    pub value: f64,
}

/// Status hasil satu percobaan fetch. Dipetakan langsung ke kolom `fetch_logs.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FetchStatus {
    Ok,
    Error,
    RateLimited,
    /// Tidak dijalankan karena guard kuota (bukan kegagalan jaringan).
    Skipped,
}

impl FetchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FetchStatus::Ok => "ok",
            FetchStatus::Error => "error",
            FetchStatus::RateLimited => "rate_limited",
            FetchStatus::Skipped => "skipped",
        }
    }
}

/// Hasil satu percobaan fetch satu metric — bisa dilaporkan apa adanya ke UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetricFetchOutcome {
    pub metric: String,
    pub status: FetchStatus,
    pub rows: usize,
    pub http_code: Option<u16>,
    pub message: Option<String>,
}

impl MetricFetchOutcome {
    pub fn ok(metric: impl Into<String>, rows: usize) -> Self {
        Self {
            metric: metric.into(),
            status: FetchStatus::Ok,
            rows,
            http_code: Some(200),
            message: None,
        }
    }

    pub fn failed(metric: impl Into<String>, http_code: Option<u16>, message: impl Into<String>) -> Self {
        Self {
            metric: metric.into(),
            status: FetchStatus::Error,
            rows: 0,
            http_code,
            message: Some(message.into()),
        }
    }

    pub fn skipped(metric: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            metric: metric.into(),
            status: FetchStatus::Skipped,
            rows: 0,
            http_code: None,
            message: Some(message.into()),
        }
    }

    pub fn rate_limited(metric: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            metric: metric.into(),
            status: FetchStatus::RateLimited,
            rows: 0,
            http_code: Some(429),
            message: Some(message.into()),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.status == FetchStatus::Ok
    }
}

// ---------------------------------------------------------------------------
// Rate-limit guard (dipakai bersama oleh semua connector)
// ---------------------------------------------------------------------------

/// Hitung jumlah request yang **benar-benar menghabiskan kuota** dalam `seconds`
/// terakhir. Hanya status `ok`/`error`/`rate_limited` yang dihitung — entri
/// `skipped` berarti tidak ada request keluar sehingga tidak boleh mengurangi kuota.
pub async fn requests_in_window(pool: &SqlitePool, source: &str, seconds: i64) -> Result<i64> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS cnt FROM fetch_logs
        WHERE source = ?
          AND status IN ('ok', 'error', 'rate_limited')
          AND fetched_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?)
        "#,
    )
    .bind(source)
    .bind(format!("-{seconds} seconds"))
    .fetch_one(pool)
    .await?;
    use sqlx::Row;
    Ok(row.get::<i64, _>("cnt"))
}

/// Ambil `X-RateLimit-Reset-Hour` (epoch detik) terakhir yang tercatat, bila ada.
/// Dipakai untuk melaporkan kapan kuota pulih — bukan untuk menebak.
pub async fn last_rate_limit_reset(pool: &SqlitePool, source: &str) -> Result<Option<i64>> {
    use sqlx::Row;
    let row = sqlx::query(
        r#"
        SELECT message FROM fetch_logs
        WHERE source = ? AND status = 'rate_limited'
        ORDER BY id DESC LIMIT 1
        "#,
    )
    .bind(source)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .and_then(|r| r.get::<Option<String>, _>("message"))
        .and_then(|m| m.split("reset_epoch=").nth(1).map(|s| s.trim().to_string()))
        .and_then(|s| s.parse::<i64>().ok()))
}

// ---------------------------------------------------------------------------
// Persistensi
// ---------------------------------------------------------------------------

/// Upsert idempotent satu metric on-chain.
///
/// `ON CONFLICT(metric, date) DO UPDATE SET source = excluded.source` — kolom
/// `source` **wajib** ikut diperbarui. Ini pelajaran dari bug Phase 1: tanpa
/// pembaruan `source`, baris lama mempertahankan label sumber yang sudah tidak
/// benar sehingga provenance basis data menjadi bohong.
pub async fn upsert_onchain_values(
    pool: &SqlitePool,
    metric: &str,
    source: &str,
    values: &[OnchainValue],
) -> Result<u64> {
    if values.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let mut affected: u64 = 0;
    for chunk in values.chunks(500) {
        for v in chunk {
            // Nilai non-finite tidak pernah masuk DB (NaN/inf = bug, bukan data).
            if !v.value.is_finite() {
                continue;
            }
            let res = sqlx::query(
                r#"
                INSERT INTO onchain_daily (metric, date, value, source)
                VALUES (?, ?, ?, ?)
                ON CONFLICT(metric, date) DO UPDATE SET
                    value = excluded.value,
                    source = excluded.source,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                "#,
            )
            .bind(metric)
            .bind(v.date.format("%Y-%m-%d").to_string())
            .bind(v.value)
            .bind(source)
            .execute(&mut *tx)
            .await?;
            affected += res.rows_affected();
        }
    }
    tx.commit().await?;
    Ok(affected)
}

/// Tanggal terakhir yang tersedia untuk sebuah metric.
pub async fn latest_onchain_date(pool: &SqlitePool, metric: &str) -> Result<Option<NaiveDate>> {
    use sqlx::Row;
    let row = sqlx::query("SELECT MAX(date) AS d FROM onchain_daily WHERE metric = ?")
        .bind(metric)
        .fetch_one(pool)
        .await?;
    let raw: Option<String> = row.get::<Option<String>, _>("d");
    Ok(raw.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()))
}

/// Baca seluruh seri historis sebuah metric (urut naik) — bahan baku engine.
pub async fn onchain_series(pool: &SqlitePool, metric: &str) -> Result<Vec<OnchainValue>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT date, value FROM onchain_daily WHERE metric = ? ORDER BY date ASC",
    )
    .bind(metric)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let d: String = r.get::<String, _>("date");
        let v: f64 = r.get::<f64, _>("value");
        if let Ok(date) = NaiveDate::parse_from_str(&d, "%Y-%m-%d") {
            out.push(OnchainValue { date, value: v });
        }
    }
    Ok(out)
}

/// Baca seri sebuah metric sampai tanggal tertentu (`<= date`), urut naik.
pub async fn onchain_series_until(pool: &SqlitePool, metric: &str, date: NaiveDate) -> Result<Vec<OnchainValue>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT date, value FROM onchain_daily WHERE metric = ? AND date <= ? ORDER BY date ASC",
    )
    .bind(metric)
    .bind(date.format("%Y-%m-%d").to_string())
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let d: String = r.get::<String, _>("date");
        let v: f64 = r.get::<f64, _>("value");
        if let Ok(date) = NaiveDate::parse_from_str(&d, "%Y-%m-%d") {
            out.push(OnchainValue { date, value: v });
        }
    }
    Ok(out)
}

/// Nilai terakhir sebuah metric (paling baru) beserta tanggalnya.
pub async fn latest_onchain_value(pool: &SqlitePool, metric: &str) -> Result<Option<OnchainValue>> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT date, value FROM onchain_daily WHERE metric = ? ORDER BY date DESC LIMIT 1",
    )
    .bind(metric)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| {
        let d: String = r.get::<String, _>("date");
        let v: f64 = r.get::<f64, _>("value");
        NaiveDate::parse_from_str(&d, "%Y-%m-%d")
            .ok()
            .map(|date| OnchainValue { date, value: v })
    }))
}

// ---------------------------------------------------------------------------
// Parser bersama
// ---------------------------------------------------------------------------

/// Ambil nilai numerik pertama yang cocok dari sebuah objek JSON.
///
/// Toleran terhadap penamaan field sumber (BGeometrics kadang memakai nama metric
/// apa adanya, kadang tanpa tanda hubung, kadang camelCase). Urutan kandidat
/// menentukan prioritas, lalu jatuh ke field numerik pertama yang bukan metadata.
pub fn pick_numeric_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    candidates: &[&str],
) -> Option<f64> {
    for key in candidates {
        if let Some(v) = obj.get(*key) {
            if let Some(f) = as_f64(v) {
                return Some(f);
            }
        }
    }
    // Fallback: field numerik pertama yang bukan metadata tanggal/indeks.
    for (k, v) in obj.iter() {
        let lk = k.to_ascii_lowercase();
        if lk == "date" || lk == "unixTs".to_ascii_lowercase() || lk == "time" || lk == "timestamp" {
            continue;
        }
        if let Some(f) = as_f64(v) {
            return Some(f);
        }
    }
    None
}

/// Konversi Value JSON ke f64, termasuk string numerik (BGeometrics mengirim
/// sebagian nilai sebagai string, mis. `lth-realized-price` -> "48462.36").
pub fn as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Parse tanggal dari string 'YYYY-MM-DD' atau 'YYYY-MM-DDTHH:MM:SS'.
pub fn parse_date(raw: &str) -> Option<NaiveDate> {
    let s = raw.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.date_naive());
    }
    // Ambil 10 karakter pertama sebagai upaya terakhir.
    if s.len() >= 10 {
        if let Ok(d) = NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d") {
            return Some(d);
        }
    }
    None
}

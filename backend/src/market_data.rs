pub use crate::config::{DailyCandle, DailyClose, FetchLogEntry};
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate};
use reqwest::Client;
use serde_json::Value;
use sqlx::Row;
use std::collections::HashMap;
use std::time::Duration;

/// --- CoinMetrics Community Client ---
const MAX_CM_PAGES: usize = 50;
const CM_PAGE_SIZE: usize = 10_000;

pub struct CoinMetricsClient {
    client: Client,
    base_url: String,
}

impl CoinMetricsClient {
    pub fn new(client: Client, base_url: String) -> Self {
        Self { client, base_url }
    }

    /// Ambil daily PriceUSD BTC dari CoinMetrics (start..=end inklusif).
    /// Mengembalikan vector sorted by date ascending, dedup.
    pub async fn fetch_price_usd_daily(
        &self,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<DailyClose>> {
        let mut all = Vec::new();
        let mut next_token: Option<String> = None;
        let mut pages = 0;

        loop {
            if pages >= MAX_CM_PAGES {
                anyhow::bail!("CoinMetrics pagination exceeded max pages ({MAX_CM_PAGES})");
            }
            pages += 1;

            let mut req = self
                .client
                .get(format!("{}/timeseries/asset-metrics", self.base_url))
                .query(&[
                    ("assets", "btc"),
                    ("metrics", "PriceUSD"),
                    ("frequency", "1d"),
                    ("page_size", &CM_PAGE_SIZE.to_string()),
                    ("start_time", &start.format("%Y-%m-%d").to_string()),
                    ("end_time", &end.format("%Y-%m-%d").to_string()),
                ]);
            if let Some(tok) = &next_token {
                req = req.query(&[("next_page_token", tok)]);
            }

            let resp = req
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .context("CoinMetrics request failed")?
                .error_for_status()
                .context("CoinMetrics HTTP error")?;
            let text = resp.text().await?;
            let (rows, token) = parse_cm_page(&text)?;
            all.extend(rows);

            match token {
                Some(t) if Some(&t) != next_token.as_ref() => next_token = Some(t),
                _ => break,
            }
        }

        // sort + dedup by date
        all.sort_by_key(|r| r.date);
        all.dedup_by(|a, b| a.date == b.date);
        Ok(all)
    }
}

/// Parse halaman JSON CoinMetrics.
/// Return (rows, next_page_token)
pub fn parse_cm_page(body: &str) -> Result<(Vec<DailyClose>, Option<String>)> {
    #[derive(serde::Deserialize)]
    struct CmRow {
        #[serde(default)]
        #[allow(dead_code)]
        asset: Option<String>,
        time: String,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    }
    #[derive(serde::Deserialize)]
    struct CmPage {
        #[serde(default)]
        data: Vec<CmRow>,
        #[serde(default)]
        next_page_token: Option<String>,
        #[serde(default)]
        error: Option<Value>,
    }

    let page: CmPage = serde_json::from_str(body).context("parse CoinMetrics JSON")?;
    if let Some(err) = page.error {
        anyhow::bail!("CoinMetrics API error: {}", err);
    }

    let mut rows = Vec::with_capacity(page.data.len());
    for r in page.data {
        // ambil PriceUSD dari extra (bisa string atau number)
        let price_val = r.extra.get("PriceUSD").context("missing PriceUSD in row")?;
        let close = match price_val {
            Value::String(s) => s.parse::<f64>().context("parse PriceUSD string")?,
            Value::Number(n) => n.as_f64().context("PriceUSD number -> f64")?,
            _ => anyhow::bail!("PriceUSD unexpected type"),
        };
        // parse time: coba RFC3339 dulu, fallback parsing date dari prefix 10 karakter 'YYYY-MM-DD'
        let date = if let Ok(dt) = DateTime::parse_from_rfc3339(&r.time) {
            dt.date_naive()
        } else if r.time.len() >= 10 {
            NaiveDate::parse_from_str(&r.time[..10], "%Y-%m-%d").context("parse YYYY-MM-DD from time")?
        } else {
            anyhow::bail!("invalid time string: {}", r.time);
        };
        rows.push(DailyClose { date, close });
    }
    Ok((rows, page.next_page_token))
}

/// --- Binance Client (failover multi base URL) ---
const BINANCE_KLINES: &str = "/api/v3/klines";
const BINANCE_PRICE: &str = "/api/v3/ticker/price";

pub struct BinanceClient {
    client: Client,
    base_urls: Vec<String>,
}

impl BinanceClient {
    pub fn new(client: Client, base_urls: Vec<String>) -> Self {
        Self { client, base_urls }
    }

    /// Ambil daily klines BTCUSDT dari start..=end (inklusif).
    /// Pagination manual dengan limit=1000 per request.
    pub async fn fetch_daily_klines(
        &self,
        symbol: &str,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<DailyCandle>> {
        let start_ms = start.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis();
        let end_ms = end.and_hms_opt(23, 59, 59).unwrap().and_utc().timestamp_millis();

        let mut all = Vec::new();
        let mut cursor_ms = start_ms;
        let mut loops = 0;

        while cursor_ms <= end_ms && loops < 20 {
            loops += 1;
            let url = self.pick_base_url();
            let resp = self
                .client
                .get(format!("{}{}", url, BINANCE_KLINES))
                .query(&[
                    ("symbol", symbol),
                    ("interval", "1d"),
                    ("startTime", &cursor_ms.to_string()),
                    ("endTime", &end_ms.to_string()),
                    ("limit", "1000"),
                ])
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .context("Binance klines request failed")?
                .error_for_status()
                .context("Binance klines HTTP error")?;
            let text = resp.text().await?;
            let rows = parse_klines(&text)?;
            if rows.is_empty() {
                break;
            }
            let last_date = rows.last().unwrap().date;
            all.extend(rows);
            if last_date >= end {
                break;
            }
            cursor_ms = last_date
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_millis()
                + 86_400_000; // next day
        }
        all.sort_by_key(|r| r.date);
        all.dedup_by(|a, b| a.date == b.date);
        Ok(all)
    }

    /// Ambil spot price real-time BTCUSDT
    pub async fn fetch_spot_price(&self, symbol: &str) -> Result<f64> {
        for base in &self.base_urls {
            match self
                .client
                .get(format!("{}{}", base, BINANCE_PRICE))
                .query(&[("symbol", symbol)])
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await?;
                    #[derive(serde::Deserialize)]
                    struct PriceResp {
                        price: String,
                    }
                    let p: PriceResp = serde_json::from_str(&text)?;
                    return p.price.parse::<f64>().context("parse spot price");
                }
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        anyhow::bail!("all Binance base URLs failed for spot price")
    }

    fn pick_base_url(&self) -> &str {
        &self.base_urls[0] // caller handles retry logic by re-instantiating? keep simple
    }
}

/// Parse Binance klines JSON array-of-arrays.
/// Setiap item: [openTime, open, high, low, close, volume, closeTime, ...]
pub fn parse_klines(body: &str) -> Result<Vec<DailyCandle>> {
    let arr: Vec<Vec<Value>> = serde_json::from_str(body).context("parse klines JSON")?;
    let mut out = Vec::with_capacity(arr.len());
    for k in arr {
        if k.len() < 6 {
            continue;
        }
        let open_time_ms = k[0].as_i64().context("openTime not int")?;
        let dt = DateTime::from_timestamp_millis(open_time_ms)
            .context("invalid open_time ms")?;
        let date = dt.date_naive();
        let open = k[1].as_str().and_then(|s| s.parse().ok());
        let high = k[2].as_str().and_then(|s| s.parse().ok());
        let low = k[3].as_str().and_then(|s| s.parse().ok());
        let close = k[4]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .context("missing close")?;
        let volume = k[5].as_str().and_then(|s| s.parse().ok());
        out.push(DailyCandle {
            date,
            open,
            high,
            low,
            close,
            volume,
        });
    }
    Ok(out)
}

/// --- Store layer: upsert helpers ---
pub async fn upsert_daily_closes(
    pool: &sqlx::SqlitePool,
    rows: &[DailyClose],
    source: &str,
) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let mut affected = 0u64;
    for r in rows {
        let res = sqlx::query(
            r#"
            INSERT INTO btc_daily_ohlcv (date, close, source)
            VALUES (?, ?, ?)
            ON CONFLICT(date) DO UPDATE SET
                close = excluded.close,
                source = excluded.source,
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
            "#,
        )
        .bind(r.date.format("%Y-%m-%d").to_string())
        .bind(r.close)
        .bind(source)
        .execute(&mut *tx)
        .await?;
        affected += res.rows_affected();
    }
    tx.commit().await?;
    Ok(affected)
}

pub async fn upsert_daily_candles(
    pool: &sqlx::SqlitePool,
    rows: &[DailyCandle],
    source: &str,
) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let mut affected = 0u64;
    for r in rows {
        let res = sqlx::query(
            r#"
            INSERT INTO btc_daily_ohlcv (date, open, high, low, close, volume, source)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(date) DO UPDATE SET
                open = excluded.open,
                high = excluded.high,
                low = excluded.low,
                close = excluded.close,
                volume = excluded.volume,
                source = excluded.source,
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
            "#,
        )
        .bind(r.date.format("%Y-%m-%d").to_string())
        .bind(r.open)
        .bind(r.high)
        .bind(r.low)
        .bind(r.close)
        .bind(r.volume)
        .bind(source)
        .execute(&mut *tx)
        .await?;
        affected += res.rows_affected();
    }
    tx.commit().await?;
    Ok(affected)
}

/// Catat log fetch ke DB (rate-limit guard Phase 2)
pub async fn log_fetch(
    pool: &sqlx::SqlitePool,
    entry: &FetchLogEntry,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO fetch_logs (source, endpoint, status, http_code, rows, message)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&entry.source)
    .bind(&entry.endpoint)
    .bind(&entry.status)
    .bind(entry.http_code)
    .bind(entry.rows)
    .bind(&entry.message)
    .execute(pool)
    .await?;
    Ok(())
}

/// Hitung fetch hari ini untuk sumber tertentu (Phase 2 rate-limit guard)
pub async fn fetches_today(pool: &sqlx::SqlitePool, source: &str) -> Result<i64> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let row = sqlx::query(
        "SELECT COUNT(*) as cnt FROM fetch_logs
         WHERE source = ? AND date(fetched_at) = ?",
    )
    .bind(source)
    .bind(&today)
    .fetch_one(pool)
    .await?;
    Ok(row.get::<i64, _>("cnt"))
}
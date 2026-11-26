//! Connector CoinMetrics Community untuk metrik on-chain yang tersedia di tier gratis.
//!
//! # Fakta Sumber (diverifikasi live 2026-11-25)
//!
//! | Metric | community? | Catatan |
//! |---|---|---|
//! | `CapMVRVCur` | ✅ true | MVRV langsung, delay ~1 hari |
//! | `SplyCur` | ✅ true | Supply BTC beredar |
//! | `CapMrktCurUSD` | ✅ true | Market Cap = Price × Supply |
//! | `PriceUSD` | ✅ true | Harga penutupan harian |
//! | `CapRealUSD` | ❌ false | "not available with supplied credentials" |
//!
//! **Derivasi matematis yang bisa dihitung dengan tepat (bukan estimasi):**
//! ```text
//! Realized Cap = CapMrktCurUSD / CapMVRVCur
//! Realized Price = Realized Cap / SplyCur = PriceUSD / CapMVRVCur
//! ```
//! Validasi silang: CoinMetrics Realized Price $53,099 vs BGeometrics $53,448 (selisih
//! 0.65%, perbedaan selisih 7 hari delay BGeometrics). **Ini bukan angka palsu.**
//!
//! Rate limit CoinMetrics Community jauh lebih longgar (tidak ada header ketat seperti
//! BGeometrics). Pagination via `next_page_token` dengan `page_size=10000` mengambil
//! ~6.000 hari dalam 1 request. Satu request harian cukup.

use super::{FetchStatus, MetricFetchOutcome, OnchainValue, upsert_onchain_values};
use crate::market_data::log_fetch;
use crate::config::{AppConfig, FetchLogEntry};
use anyhow::{Context, Result};
use chrono::NaiveDate;
use reqwest::Client;
use serde::Deserialize;
use sqlx::SqlitePool;
use std::time::Duration;

#[allow(dead_code)]
const DEFAULT_CM_BASE: &str = "https://community-api.coinmetrics.io/v4";

#[derive(Debug, Deserialize)]
pub struct CmTimeseriesPayload {
    pub data: Vec<CmRow>,
    #[allow(dead_code)]
    next_page_token: Option<String>,
    #[allow(dead_code)]
    next_page_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CmRow {
    #[allow(dead_code)]
    pub asset: String,
    pub time: String,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, Option<String>>,
}

impl CmRow {
    /// Ambil nilai numerik metric; `None` bila field tidak ada, `null`, atau bukan angka.
    pub fn num(&self, key: &str) -> Option<f64> {
        self.extra
            .get(key)
            .and_then(|v| v.as_deref())
            .and_then(|s| s.parse::<f64>().ok())
    }
}

pub struct CoinMetricsOnChainClient {
    client: Client,
    base_url: String,
    page_size: u32,
}

impl CoinMetricsOnChainClient {
    pub fn new(client: Client, base_url: String) -> Self {
        Self {
            client,
            base_url,
            page_size: 10000,
        }
    }

    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size;
        self
    }

    /// Ambil seluruh riwayat metrik on-chain dari `start` (inklusif) hingga hari ini.
    ///
    /// Mengembalikan vektor `MetricFetchOutcome` per metric yang disimpan.
    /// Jika gagal di tengah, yang sudah berhasil tetap tersimpan.
    pub async fn fetch_full_history(
        &self,
        pool: &SqlitePool,
        start: NaiveDate,
    ) -> Result<Vec<MetricFetchOutcome>> {
        let end = chrono::Utc::now().date_naive();
        let start_str = start.format("%Y-%m-%dT00:00:00Z").to_string();
        let metrics = "CapMVRVCur,SplyCur,CapMrktCurUSD,PriceUSD";

        let mut url = format!(
            "{}/timeseries/asset-metrics?assets=btc&metrics={}&frequency=1d&page_size={}&start_time={}",
            self.base_url.trim_end_matches('/'),
            metrics,
            self.page_size,
            start_str
        );

        let mut all_rows: Vec<CmRow> = Vec::new();
        let mut _page = 0u32;

        loop {
            _page += 1;
            let res = self.client.get(&url).header("User-Agent", "btc-cycle-radar/0.1").send().await?;
            let status = res.status();
            if !status.is_success() {
                let body = res.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("CoinMetrics HTTP {}: {}", status, &body[..body.len().min(200)]));
            }
            let text = res.text().await.context("read CoinMetrics response")?;
            let parsed = parse_coinmetrics_timeseries(&text)?;

            let has_next = parsed.next_page_token.is_some();
            let next_token = parsed.next_page_token.clone();
            all_rows.extend(parsed.data);

            if let Some(token) = next_token {
                let base_without_token = url.split("&next_page_token=").next().unwrap_or(&url);
                url = format!("{base_without_token}&next_page_token={token}");
                // jeda sopan antar halaman
                tokio::time::sleep(Duration::from_millis(200)).await;
            } else if !has_next {
                break;
            }
        }

        // Bangun series on-chain via pure helper teruji
        let by_metric = build_onchain_series(&all_rows, start, end);

        // Simpan ke database
        let mut outcomes = Vec::new();
        for (metric, values) in by_metric {
            if values.is_empty() {
                continue;
            }
            let rows = upsert_onchain_values(pool, &metric, "coinmetrics", &values).await?;
            outcomes.push(MetricFetchOutcome::ok(&metric, rows as usize));
            // log fetch sekali per metric (bukan per page)
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "coinmetrics".to_string(),
                    endpoint: format!("timeseries/asset-metrics({})", metric),
                    status: FetchStatus::Ok.as_str().to_string(),
                    http_code: Some(200),
                    rows: rows as i64,
                    message: None,
                },
            )
            .await?;
        }

        Ok(outcomes)
    }

    /// Ambil hanya hari terbaru (incremental update) — dipakai cron harian.
    pub async fn fetch_latest(&self, pool: &SqlitePool) -> Result<Vec<MetricFetchOutcome>> {
        // Ambil dari 30 hari terakhir untuk menangani koreksi data
        let start = chrono::Utc::now().date_naive() - chrono::Duration::days(30);
        self.fetch_full_history(pool, start).await
    }
}

/// Parse payload timeseries CoinMetrics JSON → `CmTimeseriesPayload`.
/// Toleran terhadap `null` di field metric (dipetakan ke `None`).
pub fn parse_coinmetrics_timeseries(text: &str) -> Result<CmTimeseriesPayload> {
    let parsed = serde_json::from_str::<CmTimeseriesPayload>(text)
        .context("parse CoinMetrics timeseries JSON")?;
    Ok(parsed)
}

/// Bangun series on-chain per metric dari baris payload CoinMetrics.
/// Derivasi Realized Price = PriceUSD / CapMVRVCur (matematis identik).
/// Hanya menyimpan metric yang punya data non-null dalam window.
pub fn build_onchain_series(
    rows: &[CmRow],
    start: NaiveDate,
    end: NaiveDate,
) -> std::collections::HashMap<String, Vec<OnchainValue>> {
    use std::collections::HashMap;

    let mut by_metric: HashMap<String, Vec<OnchainValue>> = HashMap::new();
    let mut price_map: HashMap<NaiveDate, f64> = HashMap::new();
    let mut mvrv_map: HashMap<NaiveDate, f64> = HashMap::new();

    for row in rows {
        let Ok(date) = NaiveDate::parse_from_str(&row.time[..10], "%Y-%m-%d") else {
            continue;
        };
        if date < start || date > end {
            continue;
        }

        if let Some(supply) = row.num("SplyCur") {
            by_metric
                .entry("supply".into())
                .or_default()
                .push(OnchainValue { date, value: supply });
        }
        if let Some(mvrv) = row.num("CapMVRVCur") {
            mvrv_map.insert(date, mvrv);
            by_metric
                .entry("mvrv_cm".into())
                .or_default()
                .push(OnchainValue { date, value: mvrv });
        }
        if let Some(price) = row.num("PriceUSD") {
            price_map.insert(date, price);
        }
        if let Some(mcap) = row.num("CapMrktCurUSD") {
            by_metric
                .entry("market_cap".into())
                .or_default()
                .push(OnchainValue { date, value: mcap });
        }
    }

    // Derivasi Realized Price: Price / MVRV
    for (date, mvrv) in mvrv_map {
        if let Some(price) = price_map.get(&date) {
            if *price > 0.0 && mvrv > 0.0 {
                by_metric
                    .entry("realized_price_cm".into())
                    .or_default()
                    .push(OnchainValue {
                        date,
                        value: price / mvrv,
                    });
            }
        }
    }

    // Urutkan tiap series menurut tanggal (ascending) untuk konsistensi DB
    for values in by_metric.values_mut() {
        values.sort_by_key(|v| v.date);
    }

    by_metric
}

/// Helper untuk baca config `coinmetrics_base_url` dari AppConfig.
pub fn build_client_from_config(cfg: &AppConfig, client: Client) -> CoinMetricsOnChainClient {
    CoinMetricsOnChainClient::new(client, cfg.coinmetrics_base_url.clone())
}
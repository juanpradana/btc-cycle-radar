use crate::config::{AppConfig, FetchLogEntry};
use crate::db::{self, CandleBounds};
use crate::market_data::{self, log_fetch, BinanceClient, CoinMetricsClient};
use anyhow::Result;
use chrono::{NaiveDate, Utc};
use reqwest::Client;
use sqlx::SqlitePool;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BootstrapReport {
    pub candles_before: i64,
    pub coinmetrics_rows: usize,
    pub binance_rows: usize,
    pub candles_after: i64,
    pub first_date: Option<NaiveDate>,
    pub last_date: Option<NaiveDate>,
}

impl std::fmt::Display for BootstrapReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== Bootstrap Report ===")?;
        writeln!(f, "Candles before: {}", self.candles_before)?;
        writeln!(f, "CoinMetrics rows inserted/updated: {}", self.coinmetrics_rows)?;
        writeln!(f, "Binance rows inserted/updated: {}", self.binance_rows)?;
        writeln!(f, "Candles after: {}", self.candles_after)?;
        if let (Some(first), Some(last)) = (self.first_date, self.last_date) {
            writeln!(f, "Date range: {} .. {}", first, last)?;
        }
        if self.candles_after >= 4500 {
            writeln!(f, "✅ PASS: > 4500 daily candles")?;
        } else {
            writeln!(f, "❌ FAIL: only {} daily candles (need >= 4500)", self.candles_after)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UpdateReport {
    pub klines_rows: usize,
    pub spot_price: Option<f64>,
    pub fetched_at: String,
}

/// Bootstrap histori penuh (idempotent: aman dijalankan berulang).
/// - force=true: abaikan min_required_candles dan fetch ulang.
/// - force=false: kalau sudah >= min_required_candles, skip cepat.
pub async fn bootstrap_history(
    pool: &SqlitePool,
    cfg: &AppConfig,
    client: &Client,
    force: bool,
) -> Result<BootstrapReport> {
    let bounds: CandleBounds = db::candle_bounds(pool).await?;

    if !force && bounds.count >= cfg.min_required_candles {
        tracing::info!("bootstrap skipped: {} candles >= {}", bounds.count, cfg.min_required_candles);
        // Populate skip-path report so Display shows correct counts and PASS
        let first = bounds.first_date.as_ref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
        let last = bounds.last_date.as_ref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
        return Ok(BootstrapReport {
            candles_before: bounds.count,
            coinmetrics_rows: 0,
            binance_rows: 0,
            candles_after: bounds.count,
            first_date: first,
            last_date: last,
        });
    }

    let http = client.clone();
    let cm = CoinMetricsClient::new(http.clone(), cfg.coinmetrics_base_url.clone());

    // 1) CoinMetrics daily close (2010-07-28 .. today)
    let today = Utc::now().date_naive();
    let cm_start = cfg.bootstrap_start;
    let cm_end = today;

    tracing::info!("fetching CoinMetrics PriceUSD {} .. {}", cm_start, cm_end);
    let cm_rows = cm.fetch_price_usd_daily(cm_start, cm_end).await?;
    let cm_count = cm_rows.len();
    let cm_affected = market_data::upsert_daily_closes(pool, &cm_rows, "coinmetrics").await?;
    log_fetch(pool, &FetchLogEntry {
        source: "coinmetrics".into(),
        endpoint: "timeseries/asset-metrics PriceUSD".into(),
        status: if cm_affected > 0 { "ok" } else { "skipped" }.into(),
        http_code: Some(200),
        rows: cm_affected as i64,
        message: None,
    }).await?;

    // 2) Binance daily klines (2017-08-17 .. today) — isi OHLCV
    // Binance BTCUSDT daily kline mulai 2017-08-17
    let binance_start = NaiveDate::from_ymd_opt(2017, 8, 17).unwrap().max(cm_start);
    let binance_end = today;

    tracing::info!("fetching Binance klines {} .. {}", binance_start, binance_end);
    let mut bin_rows = Vec::new();
    // fallback loop: coba base_urls satu per satu
    for base in &cfg.binance_base_urls {
        let client = BinanceClient::new(http.clone(), vec![base.clone()]);
        match client.fetch_daily_klines("BTCUSDT", binance_start, binance_end).await {
            Ok(rows) => {
                bin_rows = rows;
                break;
            }
            Err(e) => {
                tracing::warn!("Binance base {} failed: {}", base, e);
                log_fetch(pool, &FetchLogEntry {
                    source: "binance".into(),
                    endpoint: base.clone(),
                    status: "error".into(),
                    http_code: None,
                    rows: 0,
                    message: Some(e.to_string()),
                }).await?;
            }
        }
    }
    let bin_count = bin_rows.len();
    let bin_affected = if !bin_rows.is_empty() {
        market_data::upsert_daily_candles(pool, &bin_rows, "binance").await?
    } else {
        0
    };
    log_fetch(pool, &FetchLogEntry {
        source: "binance".into(),
        endpoint: "klines 1d BTCUSDT".into(),
        status: if bin_affected > 0 { "ok" } else { "error" }.into(),
        http_code: Some(200),
        rows: bin_affected as i64,
        message: if bin_rows.is_empty() { Some("all base URLs failed".into()) } else { None },
    }).await?;

    // 3) Final bounds
    let final_bounds = db::candle_bounds(pool).await?;
    let first = final_bounds.first_date.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());
    let last = final_bounds.last_date.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());

    Ok(BootstrapReport {
        candles_before: bounds.count,
        coinmetrics_rows: cm_count,
        binance_rows: bin_count,
        candles_after: final_bounds.count,
        first_date: first,
        last_date: last,
    })
}

/// Update harian ringan: klines 5 hari terakhir + spot price
pub async fn update_recent(
    pool: &SqlitePool,
    cfg: &AppConfig,
    client: &Client,
    lookback_days: i64,
) -> Result<UpdateReport> {
    let today = Utc::now().date_naive();
    let start = today - chrono::Duration::days(lookback_days.max(1));
    let end = today;

    let binance = BinanceClient::new(client.clone(), cfg.binance_base_urls.clone());
    let klines = binance.fetch_daily_klines("BTCUSDT", start, end).await?;
    let k_count = klines.len();
    let affected = if !klines.is_empty() {
        market_data::upsert_daily_candles(pool, &klines, "binance").await?
    } else { 0 };

    let spot = binance.fetch_spot_price("BTCUSDT").await.ok();

    log_fetch(pool, &FetchLogEntry {
        source: "binance".into(),
        endpoint: "klines 1d BTCUSDT (update)".into(),
        status: if affected > 0 { "ok" } else { "skipped" }.into(),
        http_code: Some(200),
        rows: affected as i64,
        message: None,
    }).await?;

    Ok(UpdateReport {
        klines_rows: k_count,
        spot_price: spot,
        fetched_at: Utc::now().to_rfc3339(),
    })
}
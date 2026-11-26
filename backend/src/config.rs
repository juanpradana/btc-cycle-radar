use anyhow::Result;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Konfigurasi aplikasi — bisa diisi dari env vars atau .env file.
/// Semua field punya default yang wajar; env vars override.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Alamat bind HTTP server, mis. "0.0.0.0:8099"
    pub bind_addr: String,
    /// SQLite URL, mis. "sqlite://data/btc_cycle_radar.db" atau "sqlite::memory:"
    pub database_url: String,
    /// Direktori static frontend (kalau ada). None = tidak serve static.
    pub static_dir: Option<PathBuf>,
    /// Daftar base URL Binance (data-api.binance.vision dulu, fallback api.binance.com)
    pub binance_base_urls: Vec<String>,
    /// Base URL CoinMetrics Community API
    pub coinmetrics_base_url: String,
    /// Tanggal mulai bootstrap historis (inklusif)
    pub bootstrap_start: NaiveDate,
    /// Minimal jumlah candle daily yang harus ada setelah bootstrap (verifikasi fase 1)
    pub min_required_candles: i64,
    /// Optional API key BGeometrics (Phase 2)
    pub bgeometrics_api_key: Option<String>,
    /// Optional FRED API key (Phase 2)
    pub fred_api_key: Option<String>,
    /// Max endpoint BGeometrics per hari (kuota gratis 15 req/hari, 10 req/jam)
    pub bgeometrics_max_endpoints_per_day: i64,
}

impl AppConfig {
    /// Bangun config dari environment variables. Load .env dulu kalau ada.
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        Self::from_source(|k| std::env::var(k).ok())
    }

    /// Versi yang bisa diuji tanpa menyentuh env global.
    /// `get` dipanggil dengan nama env var; return `Some(value)` kalau ada.
    pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let bind_addr = env_or(&get, "BIND_ADDR", "0.0.0.0:8099");
        let database_url = env_or(&get, "DATABASE_URL", "sqlite://data/btc_cycle_radar.db");
        let static_dir = {
            let s = env_or(&get, "STATIC_DIR", "");
            if s.is_empty() { None } else { Some(PathBuf::from(s)) }
        };
        let binance_base_urls = env_or(&get, "BINANCE_BASE_URLS",
            "https://data-api.binance.vision,https://api.binance.com")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let coinmetrics_base_url = env_or(&get, "COINMETRICS_BASE_URL",
            "https://community-api.coinmetrics.io/v4");
        let bootstrap_start = NaiveDate::parse_from_str(
            &env_or(&get, "BOOTSTRAP_START", "2010-07-01"), "%Y-%m-%d")?;
        let min_required_candles = env_or(&get, "MIN_REQUIRED_CANDLES", "4500")
            .parse::<i64>()?;
        let bgeometrics_api_key = {
            let s = env_or(&get, "BGEOMETRICS_API_KEY", "");
            if s.is_empty() { None } else { Some(s) }
        };
        let fred_api_key = {
            let s = env_or(&get, "FRED_API_KEY", "");
            if s.is_empty() { None } else { Some(s) }
        };
        let bgeometrics_max_endpoints_per_day = env_or(&get, "BGEOMETRICS_MAX_ENDPOINTS_PER_DAY", "8")
            .parse::<i64>()?;

        Ok(Self {
            bind_addr,
            database_url,
            static_dir,
            binance_base_urls,
            coinmetrics_base_url,
            bootstrap_start,
            min_required_candles,
            bgeometrics_api_key,
            fred_api_key,
            bgeometrics_max_endpoints_per_day,
        })
    }
}

fn env_or(get: &impl Fn(&str) -> Option<String>, key: &str, default: &str) -> String {
    get(key).unwrap_or_else(|| default.to_string())
}

/// Payload JSON yang dikirim ke frontend (Phase 2 akan diisi engine)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketRegimeSummary {
    pub snapshot_date: String,
    pub current_zone: String,
    pub valuation_score: Option<f64>,
    pub triggered_buy_count: usize,
    pub triggered_top_count: usize,
    pub action_guide: String,
    pub price_levels: serde_json::Value,
}

/// Data daily OHLCV (nullable untuk open/high/low/volume jika hanya close)
#[derive(Debug, Clone, PartialEq)]
pub struct DailyCandle {
    pub date: NaiveDate,
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub close: f64,
    pub volume: Option<f64>,
}

/// Hanya close (CoinMetrics bootstrap)
#[derive(Debug, Clone, PartialEq)]
pub struct DailyClose {
    pub date: NaiveDate,
    pub close: f64,
}

impl From<DailyClose> for DailyCandle {
    fn from(dc: DailyClose) -> Self {
        Self {
            date: dc.date,
            open: None,
            high: None,
            low: None,
            close: dc.close,
            volume: None,
        }
    }
}

/// Entry untuk fetch_logs
#[derive(Debug, Clone)]
pub struct FetchLogEntry {
    pub source: String,
    pub endpoint: String,
    pub status: String,       // "ok" | "error" | "rate_limited" | "skipped"
    pub http_code: Option<i32>,
    pub rows: i64,
    pub message: Option<String>,
}
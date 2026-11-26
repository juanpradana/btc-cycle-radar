//! Loader data pasar riil dari SQLite → `MarketEngineInput`.
//!
//! Prinsip (sesuai spesifikasi Phase 2 / L2-4):
//! - **Tidak ada data mock / hardcoded**. Semua nilai berasal dari tabel
//!   `btc_daily_ohlcv`, `onchain_daily`, dan `intermarket_daily`.
//! - Metrik yang tidak tersedia di DB dibiarkan `None` (bukan 0, bukan tebakan).
//!   Engine kemudian menandainya MISSING dan me-redistribusi bobotnya.
//! - **Staleness transparan**: setiap field dicatat tanggal terakhirnya dan
//!   jumlah hari tertinggal. Bila sumber eksternal gagal / kena rate limit,
//!   nilai terakhir yang tersimpan tetap dipakai tetapi ditandai
//!   `is_stale: true` + peringatan eksplisit di respons API.
//! - Semua helper kalkulasi (SMA, persentil, join rasio intermarket) adalah
//!   fungsi murni yang diuji terhadap nilai referensi yang bisa dihitung tangan.
//!
//! Konvensi jendela hari (kalender harian, bukan hari bursa):
//! - `sma_50w`  = 350 hari, `sma_30w` = 210, `sma_200w` = 1400, `sma_350w` = 2450,
//! - `sma_10w`  = 70,  `sma_20w` = 140, `sma_2y`    = 730.

use super::{IndicatorId, MarketEngineInput, MarketRegimeSummary};
use crate::math::ahr999::geomean_200d as geomean_200d_series;
use crate::math::ma::simple_moving_average;
use crate::math::quantile::calculate_percentile;
use chrono::NaiveDate;
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Konstanta jendela & toleransi kesegaran
// ---------------------------------------------------------------------------

pub const WINDOW_SMA_10W: usize = 70;
pub const WINDOW_SMA_20W: usize = 140;
pub const WINDOW_SMA_30W: usize = 210;
pub const WINDOW_SMA_50W: usize = 350;
pub const WINDOW_SMA_2Y: usize = 730;
pub const WINDOW_SMA_200W: usize = 1400;
pub const WINDOW_SMA_350W: usize = 2450;

/// Toleransi kesegaran data OHLCV harian (hari kalender).
pub const FRESHNESS_OHLCV_DAYS: i64 = 3;
/// Toleransi kesegaran metrik on-chain (sumber publik lag 1–2 hari, + buffer).
pub const FRESHNESS_ONCHAIN_DAYS: i64 = 7;
/// Toleransi kesegaran data intermarket (Yahoo; akhir pekan/hari libur).
pub const FRESHNESS_INTERMARKET_DAYS: i64 = 5;
/// Minimal observasi rasio intermarket agar persentil dianggap bermakna.
pub const MIN_RATIO_OBSERVATIONS: usize = 60;

// ---------------------------------------------------------------------------
// Field key & dependensi indikator
// ---------------------------------------------------------------------------

/// Kunci field input engine — dipakai untuk pelaporan provenance & staleness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum FieldKey {
    Sma50w,
    Sma30w,
    Sma200w,
    Sma350w,
    Sma10w,
    Sma20w,
    Sma2y,
    Ratio200wPercentile,
    Geomean200d,
    Nupl,
    SupplyInProfit,
    LthRealizedPrice,
    LthRealizedPlRatio,
    SthMvrv,
    TrueMarketMean,
    Aviv,
    BtcGoldRatioPercentile,
    BtcSpxRatioPercentile,
}

impl FieldKey {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKey::Sma50w => "sma_50w",
            FieldKey::Sma30w => "sma_30w",
            FieldKey::Sma200w => "sma_200w",
            FieldKey::Sma350w => "sma_350w",
            FieldKey::Sma10w => "sma_10w",
            FieldKey::Sma20w => "sma_20w",
            FieldKey::Sma2y => "sma_2y",
            FieldKey::Ratio200wPercentile => "ratio_200w_percentile",
            FieldKey::Geomean200d => "geomean_200d",
            FieldKey::Nupl => "nupl",
            FieldKey::SupplyInProfit => "supply_in_profit_pct",
            FieldKey::LthRealizedPrice => "lth_realized_price",
            FieldKey::LthRealizedPlRatio => "lth_realized_pl_ratio",
            FieldKey::SthMvrv => "sth_mvrv",
            FieldKey::TrueMarketMean => "true_market_mean",
            FieldKey::Aviv => "aviv",
            FieldKey::BtcGoldRatioPercentile => "btc_gold_ratio_percentile",
            FieldKey::BtcSpxRatioPercentile => "btc_spx_ratio_percentile",
        }
    }
}

/// Field input yang menjadi dependensi setiap indikator.
/// Dipakai untuk menandai `is_stale` pada indikator terdampak.
pub fn indicator_field_dependencies(id: IndicatorId) -> &'static [FieldKey] {
    use FieldKey::*;
    match id {
        IndicatorId::MA_50W => &[Sma50w],
        IndicatorId::MA_30W => &[Sma30w],
        IndicatorId::MA_200W => &[Sma200w],
        IndicatorId::MA_350W => &[Sma350w],
        IndicatorId::MA_CROSS_10W_20W => &[Sma10w, Sma20w],
        IndicatorId::INVESTOR_TOOL_2Y => &[Sma2y],
        IndicatorId::QUANTILE_200W => &[Ratio200wPercentile],
        // Power law memakai tanggal + harga, tidak bergantung metrik eksternal.
        IndicatorId::POWER_LAW => &[],
        IndicatorId::AHR999 => &[Geomean200d],
        IndicatorId::RAINBOW_CHART => &[],
        IndicatorId::BITCOIN_CLOCK => &[],
        IndicatorId::NUPL => &[Nupl],
        IndicatorId::SUPPLY_IN_PROFIT => &[SupplyInProfit],
        IndicatorId::LTH_REALIZED_PRICE => &[LthRealizedPrice],
        IndicatorId::LTH_REALIZED_PL_RATIO => &[LthRealizedPlRatio],
        IndicatorId::STH_MVRV => &[SthMvrv],
        IndicatorId::TRUE_MARKET_MEAN_AVIV => &[TrueMarketMean, Aviv],
        IndicatorId::BTC_GOLD_SPD_RATIO => &[BtcGoldRatioPercentile],
        IndicatorId::BTC_SPX_RATIO => &[BtcSpxRatioPercentile],
    }
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("tabel btc_daily_ohlcv kosong — jalankan bootstrap terlebih dahulu")]
    NoCandles,
    #[error("tidak ada candle untuk tanggal {0} pada btc_daily_ohlcv")]
    NoCandleForDate(NaiveDate),
    #[error("tanggal {requested} di luar rentang data tersedia ({first} .. {last})")]
    DateOutOfRange {
        requested: NaiveDate,
        first: NaiveDate,
        last: NaiveDate,
    },
    #[error("cakupan data tidak cukup: {available} candle < minimum {required}")]
    InsufficientCoverage { available: i64, required: i64 },
    #[error("harga close tidak valid pada {date} (nilai: {value})")]
    InvalidPrice { date: NaiveDate, value: f64 },
}

// ---------------------------------------------------------------------------
// Pure helpers (offline-testable)
// ---------------------------------------------------------------------------

/// SMA bergulir untuk seluruh seri (paralel dengan `values`).
/// Posisi yang belum punya cukup data, atau jendelanya memuat nilai
/// non-finite, bernilai `None`.
pub fn rolling_sma(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; values.len()];
    if period == 0 || values.is_empty() {
        return out;
    }
    for i in 0..values.len() {
        if i + 1 < period {
            continue;
        }
        let window = &values[i + 1 - period..=i];
        if window.iter().all(|v| v.is_finite()) {
            out[i] = simple_moving_average(window, period);
        }
    }
    out
}

/// Persentil nilai terakhir terhadap observasi sebelumnya (strictly prior).
/// Mengembalikan `None` bila observasi sebelumnya kurang dari `min_obs`.
pub fn percentile_of_last(series: &[f64], min_obs: usize) -> Option<f64> {
    if series.len() < min_obs + 1 {
        return None;
    }
    let (history, last) = series.split_at(series.len() - 1);
    let last = last.first().copied()?;
    if history.iter().any(|v| !v.is_finite()) || !last.is_finite() {
        return None;
    }
    calculate_percentile(last, history)
}

/// Persentil historis rasio `as_of.close / other.close` (join tepat tanggal).
///
/// `btc` dan `other` harus terurut naik berdasarkan tanggal.
/// Return: `(persentil, tanggal_observasi_terakhir, jumlah_observasi)`.
pub fn aligned_ratio_percentile(
    btc: &[(NaiveDate, f64)],
    other: &[(NaiveDate, f64)],
) -> Option<(f64, NaiveDate, usize)> {
    let map: BTreeMap<NaiveDate, f64> = other
        .iter()
        .filter(|(_, v)| v.is_finite() && *v > 0.0)
        .map(|(d, v)| (*d, *v))
        .collect();

    let mut ratios: Vec<(NaiveDate, f64)> = Vec::new();
    for (d, c) in btc {
        if !c.is_finite() || *c <= 0.0 {
            continue;
        }
        if let Some(o) = map.get(d) {
            ratios.push((*d, c / o));
        }
    }
    if ratios.len() < MIN_RATIO_OBSERVATIONS + 1 {
        return None;
    }
    let series: Vec<f64> = ratios.iter().map(|(_, r)| *r).collect();
    let pct = percentile_of_last(&series, MIN_RATIO_OBSERVATIONS)?;
    let last_date = ratios.last().map(|(d, _)| *d)?;
    Some((pct, last_date, ratios.len()))
}

/// Selisih hari kalender `as_of - latest` (None bila salah satu kosong).
pub fn staleness_days(as_of: NaiveDate, latest: Option<NaiveDate>) -> Option<i64> {
    latest.map(|l| (as_of - l).num_days())
}

/// Apakah data dianggap segar untuk toleransi tertentu.
/// Staleness `None` (data tidak ada) → false.
pub fn is_fresh(as_of: NaiveDate, latest: Option<NaiveDate>, tolerance_days: i64) -> bool {
    match staleness_days(as_of, latest) {
        Some(d) => d <= tolerance_days,
        None => false,
    }
}

/// Persentil rasio harga terhadap SMA 200W, dihitung sampai tanggal `as_of`.
///
/// `closes` = seri close harian terurut naik (sudah dipotong sampai `as_of`).
/// Return `(persentil, jumlah_observasi_rasio)`.
pub fn price_to_200wma_percentile(closes: &[f64]) -> Option<(f64, usize)> {
    if closes.len() < WINDOW_SMA_200W + MIN_RATIO_OBSERVATIONS + 1 {
        return None;
    }
    let sma = rolling_sma(closes, WINDOW_SMA_200W);
    let ratios: Vec<f64> = closes
        .iter()
        .zip(sma.iter())
        .filter_map(|(c, s)| match s {
            Some(s) if *s > 0.0 && c.is_finite() => Some(c / s),
            _ => None,
        })
        .collect();
    let obs = ratios.len();
    let pct = percentile_of_last(&ratios, MIN_RATIO_OBSERVATIONS)?;
    Some((pct, obs))
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

// ---------------------------------------------------------------------------
// Provenance & hasil loader
// ---------------------------------------------------------------------------

/// Asal-usul satu field input engine.
#[derive(Debug, Clone, Serialize)]
pub struct FieldProvenance {
    pub field: &'static str,
    /// Tanggal observasi terakhir yang dipakai (None = field tidak dipakai).
    pub latest_date: Option<String>,
    /// Selisih hari terhadap tanggal valuasi.
    pub staleness_days: Option<i64>,
    /// Toleransi kesegaran yang berlaku untuk field ini.
    pub tolerance_days: i64,
    pub is_stale: bool,
    /// Sumber data (mis. "coinmetrics", "bgeometrics", "yahoo", "derived:ohlcv").
    pub source: Option<String>,
    /// Nilai yang dipakai engine.
    pub value: Option<f64>,
}

/// Hasil load lengkap: input engine + audit provenance.
#[derive(Debug, Clone)]
pub struct LoadedMarketInput {
    pub input: MarketEngineInput,
    pub as_of: NaiveDate,
    pub first_candle: NaiveDate,
    pub last_candle: NaiveDate,
    pub candles_total: i64,
    pub candles_used: usize,
    pub fields: Vec<FieldProvenance>,
    pub is_stale: bool,
    pub stale_fields: Vec<&'static str>,
    pub missing_fields: Vec<&'static str>,
    pub warnings: Vec<String>,
}

impl LoadedMarketInput {
    /// Set field stale → himpunan kunci untuk penandaan indikator.
    pub fn stale_keys(&self) -> BTreeSet<&'static str> {
        self.stale_fields.iter().copied().collect()
    }
}

/// Tandai `is_stale: true` pada setiap indikator yang bergantung pada field stale.
/// Indikator tanpa dependensi (power law, rainbow, clock) tidak pernah ditandai.
pub fn apply_staleness(summary: &mut MarketRegimeSummary, stale_fields: &BTreeSet<&'static str>) {
    if stale_fields.is_empty() {
        return;
    }
    for result in summary.indicator_results.iter_mut() {
        let deps = indicator_field_dependencies(result.id);
        let affected = deps
            .iter()
            .any(|k| stale_fields.contains(k.as_str()));
        if affected && result.missing_reason.is_none() {
            result.is_stale = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Query helper
// ---------------------------------------------------------------------------

async fn fetch_candles_until(
    pool: &SqlitePool,
    as_of: Option<NaiveDate>,
) -> Result<Vec<(NaiveDate, f64)>, sqlx::Error> {
    let rows = match as_of {
        Some(d) => {
            sqlx::query(
                "SELECT date, close FROM btc_daily_ohlcv WHERE date <= ? ORDER BY date ASC",
            )
            .bind(d.format("%Y-%m-%d").to_string())
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query("SELECT date, close FROM btc_daily_ohlcv ORDER BY date ASC")
                .fetch_all(pool)
                .await?
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let d: String = r.get("date");
        let c: f64 = r.get("close");
        if let Some(d) = parse_date(&d) {
            out.push((d, c));
        }
    }
    Ok(out)
}

/// Nilai on-chain terbaru pada/ sebelum `as_of`.
/// Return `(tanggal, nilai, sumber)`.
async fn fetch_latest_onchain(
    pool: &SqlitePool,
    metric: &str,
    as_of: NaiveDate,
) -> Result<Option<(NaiveDate, f64, String)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT date, value, source FROM onchain_daily
         WHERE metric = ? AND date <= ?
         ORDER BY date DESC LIMIT 1",
    )
    .bind(metric)
    .bind(as_of.format("%Y-%m-%d").to_string())
    .fetch_optional(pool)
    .await?;

    Ok(row.and_then(|r| {
        let d: String = r.get("date");
        let v: f64 = r.get("value");
        let s: String = r.get("source");
        parse_date(&d).map(|d| (d, v, s))
    }))
}

/// Seri intermarket lengkap (untuk persentil rasio) sampai `as_of`.
async fn fetch_intermarket_until(
    pool: &SqlitePool,
    symbol: &str,
    as_of: NaiveDate,
) -> Result<Vec<(NaiveDate, f64)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT date, close FROM intermarket_daily
         WHERE symbol = ? AND date <= ? ORDER BY date ASC",
    )
    .bind(symbol)
    .bind(as_of.format("%Y-%m-%d").to_string())
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let d: String = r.get("date");
        let c: f64 = r.get("close");
        if let Some(d) = parse_date(&d) {
            out.push((d, c));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Orkestrasi
// ---------------------------------------------------------------------------

/// Muat `MarketEngineInput` dari data riil di SQLite.
///
/// - `as_of = None` → pakai candle terakhir yang tersedia.
/// - `as_of = Some(d)` → validasi bahwa candle `d` benar-benar ada.
/// - `min_required_candles` = ambang cakupan minimum (dari `AppConfig`).
pub async fn load_market_input(
    pool: &SqlitePool,
    as_of: Option<NaiveDate>,
    min_required_candles: i64,
) -> Result<LoadedMarketInput, LoaderError> {
    // 1) Ambil bound & total keseluruhan DB (satu query).
    let overall = sqlx::query(
        "SELECT MIN(date) AS first_date, MAX(date) AS last_date, COUNT(*) AS cnt FROM btc_daily_ohlcv",
    )
    .fetch_one(pool)
    .await?;

    let db_first_raw: Option<String> = overall.try_get::<Option<String>, _>("first_date")?;
    let db_last_raw: Option<String> = overall.try_get::<Option<String>, _>("last_date")?;
    let total: i64 = overall.try_get::<i64, _>("cnt")?;

    // 2) DB kosong?
    if total == 0 || db_first_raw.is_none() || db_last_raw.is_none() {
        return Err(LoaderError::NoCandles);
    }
    let (Some(db_first_s), Some(db_last_s)) = (db_first_raw, db_last_raw) else {
        return Err(LoaderError::NoCandles);
    };
    let db_first = parse_date(&db_first_s).ok_or(LoaderError::NoCandles)?;
    let db_last = parse_date(&db_last_s).ok_or(LoaderError::NoCandles)?;

    // 3) Validasi as_of terhadap rentang KESELURUHAN (400) — sebelum cakupan & fetch.
    if let Some(d) = as_of {
        if d < db_first || d > db_last {
            return Err(LoaderError::DateOutOfRange {
                requested: d,
                first: db_first,
                last: db_last,
            });
        }
        let has_candle = sqlx::query("SELECT 1 FROM btc_daily_ohlcv WHERE date = ?")
            .bind(d.format("%Y-%m-%d").to_string())
            .fetch_optional(pool)
            .await?
            .is_some();
        if !has_candle {
            return Err(LoaderError::NoCandleForDate(d));
        }
    }

    // 4) Cakupan total DB (503) — after range check so out-of-range is 400 not 503.
    if total < min_required_candles {
        return Err(LoaderError::InsufficientCoverage {
            available: total,
            required: min_required_candles,
        });
    }

    // 5) Fetch candle subset (<= as_of) atau semua jika as_of None.
    let candles = fetch_candles_until(pool, as_of).await?;
    if candles.is_empty() {
        // Should not happen if bounds validated, but safety.
        return Err(LoaderError::NoCandles);
    }
    let first_candle = candles.first().map(|(d, _)| *d).unwrap_or(NaiveDate::MIN);
    let last_candle = candles.last().map(|(d, _)| *d).unwrap_or(NaiveDate::MIN);

    let as_of_date = match as_of {
        None => last_candle,
        Some(d) => d,
    };

    let spot = candles
        .last()
        .map(|(_, c)| *c)
        .ok_or(LoaderError::NoCandles)?;
    if !spot.is_finite() || spot <= 0.0 {
        return Err(LoaderError::InvalidPrice {
            date: as_of_date,
            value: spot,
        });
    }

    let closes: Vec<f64> = candles.iter().map(|(_, c)| *c).collect();
    let mut fields: Vec<FieldProvenance> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut input = MarketEngineInput {
        date: as_of_date,
        spot_price: spot,
        ..Default::default()
    };

    // --- 1. MA teknikal (derivasi lokal dari OHLCV) ---
    let push_ohlcv_field =
        |fields: &mut Vec<FieldProvenance>, key: FieldKey, value: Option<f64>| {
            let fresh = is_fresh(as_of_date, Some(last_candle), FRESHNESS_OHLCV_DAYS);
            fields.push(FieldProvenance {
                field: key.as_str(),
                latest_date: Some(last_candle.format("%Y-%m-%d").to_string()),
                staleness_days: staleness_days(as_of_date, Some(last_candle)),
                tolerance_days: FRESHNESS_OHLCV_DAYS,
                // Field tidak tersedia (riwayat kurang) bukan "stale", tetapi
                // dilaporkan dengan nilai None.
                is_stale: value.is_some() && !fresh,
                source: value
                    .is_some()
                    .then(|| "derived:ohlcv".to_string()),
                value,
            });
        };

    let ma_plan: [(FieldKey, usize); 7] = [
        (FieldKey::Sma50w, WINDOW_SMA_50W),
        (FieldKey::Sma30w, WINDOW_SMA_30W),
        (FieldKey::Sma200w, WINDOW_SMA_200W),
        (FieldKey::Sma350w, WINDOW_SMA_350W),
        (FieldKey::Sma10w, WINDOW_SMA_10W),
        (FieldKey::Sma20w, WINDOW_SMA_20W),
        (FieldKey::Sma2y, WINDOW_SMA_2Y),
    ];
    let mut ma_values: BTreeMap<FieldKey, Option<f64>> = BTreeMap::new();
    for (key, window) in ma_plan {
        let v = simple_moving_average(&closes, window);
        ma_values.insert(key, v);
        push_ohlcv_field(&mut fields, key, v);
    }
    input.sma_50w = ma_values.get(&FieldKey::Sma50w).copied().flatten();
    input.sma_30w = ma_values.get(&FieldKey::Sma30w).copied().flatten();
    input.sma_200w = ma_values.get(&FieldKey::Sma200w).copied().flatten();
    input.sma_350w = ma_values.get(&FieldKey::Sma350w).copied().flatten();
    input.sma_10w = ma_values.get(&FieldKey::Sma10w).copied().flatten();
    input.sma_20w = ma_values.get(&FieldKey::Sma20w).copied().flatten();
    input.sma_2y = ma_values.get(&FieldKey::Sma2y).copied().flatten();

    // --- 2. Matematis turunan OHLCV ---
    let gm = geomean_200d_series(&closes);
    push_ohlcv_field(&mut fields, FieldKey::Geomean200d, gm);
    input.geomean_200d = gm;

    let pct200 = price_to_200wma_percentile(&closes).map(|(p, _)| p);
    push_ohlcv_field(&mut fields, FieldKey::Ratio200wPercentile, pct200);
    input.ratio_200w_percentile = pct200;

    // --- 3. On-chain ---
    // Pemetaan nama metrik DB → field input engine. Metrik yang belum pernah
    // tersimpan (mis. butuh API key) tetap `None` dan dilaporkan MISSING.
    // Per 2026-11-26: supply_in_profit_pct dan lth_realized_pl_ratio
    // kini tersedia via BGeometrics free tier (profit-loss, dan derivasi dari
    // realized_profit_lth / realized_loss_lth).
    let onchain_plan: [(&str, FieldKey); 7] = [
            ("nupl", FieldKey::Nupl),
            ("true_market_mean", FieldKey::TrueMarketMean),
            ("aviv", FieldKey::Aviv),
            ("lth_realized_price", FieldKey::LthRealizedPrice),
            ("sth_mvrv", FieldKey::SthMvrv),
            ("supply_in_profit_pct", FieldKey::SupplyInProfit),
            ("lth_realized_pl_ratio", FieldKey::LthRealizedPlRatio),
        ];
    let mut resolved: BTreeMap<FieldKey, Option<f64>> = BTreeMap::new();
    for (metric, key) in onchain_plan {
        let got = fetch_latest_onchain(pool, metric, as_of_date).await?;
        let (latest, value, source) = match got {
            Some((d, v, s)) => (Some(d), Some(v), Some(s)),
            None => (None, None, None),
        };
        let fresh = is_fresh(as_of_date, latest, FRESHNESS_ONCHAIN_DAYS);
        let is_stale = value.is_some() && !fresh;
        if is_stale {
            if let Some(d) = latest {
                warnings.push(format!(
                    "metrik on-chain '{metric}' terakhir {d} ({} hari lalu) — data basi, dipakai nilai terakhir yang tersimpan",
                    staleness_days(as_of_date, Some(d)).unwrap_or_default()
                ));
            }
        }
        fields.push(FieldProvenance {
            field: key.as_str(),
            latest_date: latest.map(|d| d.format("%Y-%m-%d").to_string()),
            staleness_days: staleness_days(as_of_date, latest),
            tolerance_days: FRESHNESS_ONCHAIN_DAYS,
            is_stale,
            source,
            value,
        });
        resolved.insert(key, value);
    }

    input.nupl = resolved.get(&FieldKey::Nupl).copied().flatten();
    input.true_market_mean = resolved.get(&FieldKey::TrueMarketMean).copied().flatten();
    input.aviv = resolved.get(&FieldKey::Aviv).copied().flatten();
    input.lth_realized_price = resolved.get(&FieldKey::LthRealizedPrice).copied().flatten();
    input.sth_mvrv = resolved.get(&FieldKey::SthMvrv).copied().flatten();
    input.supply_in_profit_pct = resolved.get(&FieldKey::SupplyInProfit).copied().flatten();
    input.lth_realized_pl_ratio = resolved.get(&FieldKey::LthRealizedPlRatio).copied().flatten();

    // --- 4. Intermarket ---
    let gold = fetch_intermarket_until(pool, "XAU", as_of_date).await?;
    let spx = fetch_intermarket_until(pool, "GSPC", as_of_date).await?;
    let gold_last = gold.last().map(|(d, _)| *d);
    let spx_last = spx.last().map(|(d, _)| *d);
    input.gold_price = gold.last().map(|(_, v)| *v);
    input.spx_price = spx.last().map(|(_, v)| *v);

    for (label, key, series, last) in [
        ("XAU", FieldKey::BtcGoldRatioPercentile, &gold, gold_last),
        ("GSPC", FieldKey::BtcSpxRatioPercentile, &spx, spx_last),
    ] {
        let ratio = aligned_ratio_percentile(&candles, series);
        let value = ratio.map(|(p, _, _)| p);
        let ratio_last = ratio.map(|(_, d, _)| d);
        let latest = match (ratio_last, last) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let fresh = is_fresh(as_of_date, latest, FRESHNESS_INTERMARKET_DAYS);
        let is_stale = value.is_some() && !fresh;
        if value.is_none() && !series.is_empty() {
            warnings.push(format!(
                "rasio BTC/{label} tidak dapat dihitung: hanya {} observasi gabungan (min {MIN_RATIO_OBSERVATIONS})",
                series.len()
            ));
        }
        if is_stale {
            if let Some(d) = latest {
                warnings.push(format!(
                    "data intermarket '{label}' terakhir {d} — dipakai nilai terakhir yang tersimpan"
                ));
            }
        }
        fields.push(FieldProvenance {
            field: key.as_str(),
            latest_date: latest.map(|d| d.format("%Y-%m-%d").to_string()),
            staleness_days: staleness_days(as_of_date, latest),
            tolerance_days: FRESHNESS_INTERMARKET_DAYS,
            is_stale,
            source: (!series.is_empty()).then(|| "derived:yahoo".to_string()),
            value,
        });
    }
    input.btc_gold_ratio_percentile = fields
        .iter()
        .find(|f| f.field == FieldKey::BtcGoldRatioPercentile.as_str())
        .and_then(|f| f.value);
    input.btc_spx_ratio_percentile = fields
        .iter()
        .find(|f| f.field == FieldKey::BtcSpxRatioPercentile.as_str())
        .and_then(|f| f.value);

    // --- 5. Ringkasan staleness ---
    let stale_fields: Vec<&'static str> = fields
        .iter()
        .filter(|f| f.is_stale)
        .map(|f| f.field)
        .collect();
    let missing_fields: Vec<&'static str> = fields
        .iter()
        .filter(|f| f.value.is_none())
        .map(|f| f.field)
        .collect();

    // Transparansi provider: BGeometrics tier gratis menunda data 7 hari.
    // Jika ada field on-chain yang usianya >= 5 hari dari as_of, berikan warning
    // eksplisit agar pengguna tahu bahwa metrik psikologi bukan kondisi hari ini.
    let onchain_lag_fields: Vec<(&'static str, i64)> = fields
        .iter()
        .filter(|f| {
            matches!(
                f.field,
                "nupl" | "true_market_mean" | "aviv" | "lth_realized_price" | "sth_mvrv"
            )
        })
        .filter_map(|f| f.staleness_days.map(|d| (f.field, d)))
        .filter(|(_, d)| *d >= 5)
        .collect();
    if !onchain_lag_fields.is_empty() {
        let max_lag = onchain_lag_fields.iter().map(|(_, d)| *d).max().unwrap_or(0);
        warnings.push(format!(
            "Data on-chain BGeometrics (tier publik gratis) mengalami delay terstruktur provider ~{max_lag} hari (terbaru: {}). Metrik psikologi mencerminkan kondisi {} hari lalu.",
            fields.iter().find(|f| f.field == "nupl").and_then(|f| f.latest_date.as_deref()).unwrap_or("?"),
            max_lag
        ));
    }

    if !stale_fields.is_empty() {
        warnings.push(format!(
            "{} field memakai data basi (is_stale=true): {}",
            stale_fields.len(),
            stale_fields.join(", ")
        ));
    }

    Ok(LoadedMarketInput {
        input,
        as_of: as_of_date,
        first_candle,
        last_candle,
        candles_total: total,
        candles_used: candles.len(),
        is_stale: !stale_fields.is_empty(),
        stale_fields,
        missing_fields,
        fields,
        warnings,
    })
}

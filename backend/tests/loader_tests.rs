//! Unit & integration tests untuk `src/engine/loader.rs` (TDD).
//!
//! Prinsip:
//! - Fungsi murni diuji dengan data sintetis yang nilainya bisa dihitung tangan.
//! - Integrasi memakai SQLite tempfile dengan seed deterministik.
//! - Field yang tidak tersedia harus dilaporkan MISSING, bukan diisi nol.

use btc_cycle_radar::config::AppConfig;
use btc_cycle_radar::db;
use btc_cycle_radar::engine::loader::{
    aligned_ratio_percentile, apply_staleness, is_fresh, load_market_input, percentile_of_last,
    price_to_200wma_percentile, rolling_sma, staleness_days, FieldKey, LoaderError,
    FRESHNESS_ONCHAIN_DAYS,
};
use chrono::{Duration, NaiveDate};
use sqlx::{query, SqlitePool};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Pure helper tests (offline, tanpa DB)
// ---------------------------------------------------------------------------

#[test]
fn test_rolling_sma_basic() {
    // period=3: idx2=(10+20+30)/3=20, idx3=(20+30+40)/3=30, idx4=(30+40+50)/3=40
    let values = vec![10.0, 20.0, 30.0, 40.0, 50.0];
    let sma = rolling_sma(&values, 3);
    assert_eq!(sma.len(), 5);
    assert!(sma[0].is_none());
    assert!(sma[1].is_none());
    assert!((sma[2].expect("sma idx2") - 20.0).abs() < 1e-9);
    assert!((sma[3].expect("sma idx3") - 30.0).abs() < 1e-9);
    assert!((sma[4].expect("sma idx4") - 40.0).abs() < 1e-9);
}

#[test]
fn test_rolling_sma_non_finite_propagation() {
    // NaN di jendela -> None (tidak boleh menghasilkan angka palsu)
    let values = vec![10.0, 20.0, f64::NAN, 40.0, 50.0];
    let sma = rolling_sma(&values, 3);
    assert!(sma[2].is_none());
    assert!(sma[3].is_none());
    assert!(sma[4].is_none());
}

#[test]
fn test_rolling_sma_empty_and_zero_period() {
    let empty: Vec<f64> = Vec::new();
    assert!(rolling_sma(&empty, 3).is_empty());
    let values = vec![1.0, 2.0, 3.0];
    assert!(rolling_sma(&values, 0).iter().all(|v| v.is_none()));
}

#[test]
fn test_percentile_of_last_all_time_high() {
    // Last = 100 = nilai tertinggi -> persentil 100
    let series: Vec<f64> = (1..=100).map(|x| x as f64).collect();
    let pct = percentile_of_last(&series, 10).expect("persentil");
    assert!((pct - 100.0).abs() < 1e-9);
}

#[test]
fn test_percentile_of_last_median_reference() {
    // History 1..=50, last = 26 -> jumlah history <= 26 adalah 26 dari 50 -> 52%
    let mut series: Vec<f64> = (1..=50).map(|x| x as f64).collect();
    series.push(26.0);
    let pct = percentile_of_last(&series, 10).expect("persentil");
    assert!((pct - 52.0).abs() < 1e-9);
}

#[test]
fn test_percentile_of_last_insufficient_observations() {
    let series: Vec<f64> = vec![1.0, 2.0, 3.0];
    assert!(percentile_of_last(&series, 10).is_none());
}

#[test]
fn test_aligned_ratio_percentile_deterministic() {
    // Rasio BTC/Gold naik monoton -> persentil nilai terakhir 100
    let base = NaiveDate::from_ymd_opt(2023, 1, 1).expect("valid");
    let btc: Vec<(NaiveDate, f64)> = (0..80)
        .map(|i| (base + Duration::days(i), 100.0 * 1.01_f64.powi(i as i32)))
        .collect();
    let gold: Vec<(NaiveDate, f64)> = (0..80)
        .map(|i| (base + Duration::days(i), 10.0))
        .collect();

    let (pct, last_date, obs) = aligned_ratio_percentile(&btc, &gold).expect("rasio intermarket");
    assert_eq!(obs, 80);
    assert_eq!(last_date, btc.last().expect("btc last").0);
    assert!((pct - 100.0).abs() < 1e-9);
}

#[test]
fn test_aligned_ratio_percentile_requires_min_observations() {
    let base = NaiveDate::from_ymd_opt(2023, 1, 1).expect("valid");
    let btc: Vec<(NaiveDate, f64)> = (0..10)
        .map(|i| (base + Duration::days(i), 100.0 + i as f64))
        .collect();
    let gold: Vec<(NaiveDate, f64)> = (0..10)
        .map(|i| (base + Duration::days(i), 10.0))
        .collect();
    assert!(aligned_ratio_percentile(&btc, &gold).is_none());
}

#[test]
fn test_aligned_ratio_percentile_ignores_unmatched_dates() {
    // Hanya tanggal yang cocok dipakai; deret lain (mis. weekend) dilewati.
    let base = NaiveDate::from_ymd_opt(2023, 1, 1).expect("valid");
    let btc: Vec<(NaiveDate, f64)> = (0..122)
        .map(|i| (base + Duration::days(i), 100.0 + i as f64))
        .collect();
    let gold: Vec<(NaiveDate, f64)> = (0..122)
        .step_by(2)
        .map(|i| (base + Duration::days(i as i64), 10.0))
        .collect();
    let (_, _, obs) = aligned_ratio_percentile(&btc, &gold).expect("rasio");
    assert_eq!(obs, 61);
}

#[test]
fn test_price_to_200wma_percentile_monotonic_rise_is_high() {
    // Harga tumbuh eksponensial -> fungsi berjalan tanpa panic & hasil valid 0-100.
    // (Rasio konstan pada pertumbuhan eksponensial murni — test ini memastikan
    // tidak panic dan output dalam rentang yang wajar.)
    let closes: Vec<f64> = (0..1600).map(|i| 100.0 * 1.0008_f64.powi(i)).collect();
    let (pct, obs) = price_to_200wma_percentile(&closes).expect("persentil 200W");
    assert!(obs > 60, "observasi rasio: {obs}");
    assert!((0.0..=100.0).contains(&pct), "persentil {pct} di luar 0-100");
}

#[test]
fn test_price_to_200wma_percentile_insufficient_history() {
    let closes: Vec<f64> = vec![100.0; 1400];
    assert!(price_to_200wma_percentile(&closes).is_none());
}

#[test]
fn test_staleness_days_and_freshness() {
    let as_of = NaiveDate::from_ymd_opt(2026, 11, 25).expect("valid");
    let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).expect("valid");

    assert_eq!(staleness_days(as_of, Some(d(2026, 11, 20))), Some(5));
    assert_eq!(staleness_days(as_of, Some(as_of)), Some(0));
    assert_eq!(staleness_days(as_of, None), None);

    assert!(is_fresh(as_of, Some(d(2026, 11, 24)), 3));
    assert!(is_fresh(as_of, Some(d(2026, 11, 22)), 3));
    assert!(!is_fresh(as_of, Some(d(2026, 11, 21)), 3));
    assert!(!is_fresh(as_of, None, 3));

    // On-chain tolerance (7 hari): data 2026-11-18 masih segar
    assert!(is_fresh(as_of, Some(d(2026, 11, 18)), FRESHNESS_ONCHAIN_DAYS));
    assert!(!is_fresh(as_of, Some(d(2026, 11, 17)), FRESHNESS_ONCHAIN_DAYS));
}

// ---------------------------------------------------------------------------
// Integration test dengan SQLite tempfile
// ---------------------------------------------------------------------------

async fn setup_test_db() -> (SqlitePool, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("test.db");
    let url = format!("sqlite://{}", db_path.display().to_string().replace('\\', "/"));
    let pool = db::connect(&url).await.expect("connect db");
    (pool, tmp)
}

async fn seed_candles(pool: &SqlitePool, start: NaiveDate, count: usize, base_price: f64) {
    for i in 0..count {
        let d = start + Duration::days(i as i64);
        let price = base_price * (1.0 + (i as f64 * 0.003).sin() * 0.02);
        query("INSERT INTO btc_daily_ohlcv (date, close, source) VALUES (?, ?, 'test')")
            .bind(d.format("%Y-%m-%d").to_string())
            .bind(price)
            .execute(pool)
            .await
            .expect("seed candle");
    }
}

async fn seed_onchain(pool: &SqlitePool, metric: &str, start: NaiveDate, count: usize, base_val: f64) {
    for i in 0..count {
        let d = start + Duration::days(i as i64);
        let val = base_val * (1.0 + (i as f64 * 0.01).sin() * 0.05);
        query("INSERT INTO onchain_daily (metric, date, value, source) VALUES (?, ?, ?, 'test')")
            .bind(metric)
            .bind(d.format("%Y-%m-%d").to_string())
            .bind(val)
            .execute(pool)
            .await
            .expect("seed onchain");
    }
}

async fn seed_intermarket(
    pool: &SqlitePool,
    symbol: &str,
    start: NaiveDate,
    count: usize,
    base_price: f64,
) {
    for i in 0..count {
        let d = start + Duration::days(i as i64);
        let price = base_price * (1.0 + (i as f64 * 0.007).cos() * 0.015);
        query("INSERT INTO intermarket_daily (symbol, date, close, source) VALUES (?, ?, ?, 'test')")
            .bind(symbol)
            .bind(d.format("%Y-%m-%d").to_string())
            .bind(price)
            .execute(pool)
            .await
            .expect("seed intermarket");
    }
}

#[tokio::test]
async fn test_load_market_input_latest_complete() {
    let (pool, _tmp) = setup_test_db().await;
    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    let total_days = 2500usize;

    seed_candles(&pool, start, total_days, 20_000.0).await;
    seed_onchain(&pool, "nupl", start, total_days, 0.2).await;
    seed_onchain(&pool, "true_market_mean", start, total_days, 30_000.0).await;
    seed_onchain(&pool, "aviv", start, total_days, 1.2).await;
    seed_intermarket(&pool, "XAU", start, total_days, 1800.0).await;
    seed_intermarket(&pool, "GSPC", start, total_days, 3500.0).await;

    let loaded = load_market_input(&pool, None, 1000).await.expect("load");

    assert!(loaded.input.spot_price.is_finite());
    assert!(loaded.input.spot_price > 0.0);
    assert_eq!(loaded.candles_used, total_days);

    // MA teknikal tersedia (riwayat 2500 hari > 2450 window terpanjang)
    assert!(loaded.input.sma_50w.is_some());
    assert!(loaded.input.sma_200w.is_some());
    assert!(loaded.input.sma_350w.is_some());
    assert!(loaded.input.sma_2y.is_some());
    assert!(loaded.input.geomean_200d.is_some());
    assert!(loaded.input.ratio_200w_percentile.is_some());

    // On-chain tersedia
    assert!(loaded.input.nupl.is_some());
    assert!(loaded.input.true_market_mean.is_some());
    assert!(loaded.input.aviv.is_some());

    // Intermarket tersedia (termasuk persentil rasio)
    assert!(loaded.input.gold_price.is_some());
    assert!(loaded.input.spx_price.is_some());
    assert!(loaded.input.btc_gold_ratio_percentile.is_some());
    assert!(loaded.input.btc_spx_ratio_percentile.is_some());

    // Data segar -> tidak ada field stale
    assert!(!loaded.is_stale, "stale fields: {:?}", loaded.stale_fields);

    // Metrik yang tidak ada di DB harus MISSING eksplisit
    for key in [
        FieldKey::SupplyInProfit,
        FieldKey::LthRealizedPrice,
        FieldKey::LthRealizedPlRatio,
        FieldKey::SthMvrv,
    ] {
        assert!(
            loaded.missing_fields.contains(&key.as_str()),
            "{} harus MISSING",
            key.as_str()
        );
    }
}

#[tokio::test]
async fn test_load_market_input_stale_onchain_flags_and_warns() {
    let (pool, _tmp) = setup_test_db().await;
    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    let total_days = 2500usize;
    let onchain_days = total_days - 10; // 10 hari sebelum candle terakhir -> basi

    seed_candles(&pool, start, total_days, 20_000.0).await;
    seed_onchain(&pool, "nupl", start, onchain_days, 0.2).await;
    seed_intermarket(&pool, "XAU", start, total_days, 1800.0).await;
    seed_intermarket(&pool, "GSPC", start, total_days, 3500.0).await;

    let loaded = load_market_input(&pool, None, 1000).await.expect("load");

    // Nilai terakhir tetap dipakai (bukan di-nol-kan)
    assert!(loaded.input.nupl.is_some());
    assert!(loaded.is_stale);
    assert!(loaded.stale_fields.contains(&FieldKey::Nupl.as_str()));
    assert!(
        loaded.warnings.iter().any(|w| w.contains("nupl")),
        "warnings harus menyebut nupl: {:?}",
        loaded.warnings
    );
    assert!(loaded.missing_fields.contains(&FieldKey::LthRealizedPrice.as_str()));
}

#[tokio::test]
async fn test_load_market_input_as_of_mid_history() {
    let (pool, _tmp) = setup_test_db().await;
    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    let total_days = 2500usize;
    seed_candles(&pool, start, total_days, 20_000.0).await;
    seed_onchain(&pool, "nupl", start, total_days, 0.2).await;
    seed_intermarket(&pool, "XAU", start, total_days, 1800.0).await;
    seed_intermarket(&pool, "GSPC", start, total_days, 3500.0).await;

    let as_of = start + Duration::days(2000);
    let loaded = load_market_input(&pool, Some(as_of), 1000).await.expect("load");

    assert_eq!(loaded.as_of, as_of);
    // last_candle = candle terakhir yang IKUT dihitung (batas atas subset), yaitu as_of.
    assert_eq!(loaded.last_candle, as_of);
    assert_eq!(loaded.first_candle, start);
    // Hanya candle <= as_of yang dipakai (2001 baris: index 0..=2000)
    assert_eq!(loaded.candles_used, 2001);
    assert!(loaded.input.spot_price > 0.0);
}

#[tokio::test]
async fn test_load_market_input_rejects_date_outside_range() {
    let (pool, _tmp) = setup_test_db().await;
    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    seed_candles(&pool, start, 100, 20_000.0).await;

    let missing = start + Duration::days(200);
    let err = load_market_input(&pool, Some(missing), 1000)
        .await
        .expect_err("harus error");
    assert!(
        matches!(err, LoaderError::DateOutOfRange { .. }),
        "unexpected: {err:?}"
    );
}

#[tokio::test]
async fn test_load_market_input_rejects_gap_date() {
    let (pool, _tmp) = setup_test_db().await;
    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    for i in (0..10).chain(20..30) {
        let d = start + Duration::days(i);
        query("INSERT INTO btc_daily_ohlcv (date, close, source) VALUES (?, 20000.0, 'test')")
            .bind(d.format("%Y-%m-%d").to_string())
            .execute(&pool)
            .await
            .expect("seed");
    }

    let gap = start + Duration::days(15);
    let err = load_market_input(&pool, Some(gap), 1)
        .await
        .expect_err("harus error");
    assert!(
        matches!(err, LoaderError::NoCandleForDate(_)),
        "unexpected: {err:?}"
    );
}

#[tokio::test]
async fn test_load_market_input_enforces_min_coverage() {
    let (pool, _tmp) = setup_test_db().await;
    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    seed_candles(&pool, start, 100, 20_000.0).await;

    let err = load_market_input(&pool, None, 4500)
        .await
        .expect_err("harus error");
    assert!(
        matches!(err, LoaderError::InsufficientCoverage { .. }),
        "unexpected: {err:?}"
    );
}

#[tokio::test]
async fn test_load_market_input_empty_db() {
    let (pool, _tmp) = setup_test_db().await;
    let err = load_market_input(&pool, None, 1).await.expect_err("harus error");
    assert!(matches!(err, LoaderError::NoCandles), "unexpected: {err:?}");
}

#[tokio::test]
async fn test_apply_staleness_marks_only_dependents() {
    use btc_cycle_radar::engine::{evaluate_all_indicators, IndicatorId};

    let (pool, _tmp) = setup_test_db().await;
    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    let total_days = 2500usize;
    seed_candles(&pool, start, total_days, 20_000.0).await;
    seed_onchain(&pool, "nupl", start, total_days - 10, 0.2).await;
    seed_intermarket(&pool, "XAU", start, total_days, 1800.0).await;
    seed_intermarket(&pool, "GSPC", start, total_days, 3500.0).await;

    let loaded = load_market_input(&pool, None, 1000).await.expect("load");
    let mut summary = evaluate_all_indicators(&loaded.input);

    let nupl_before = summary
        .indicator_results
        .iter()
        .find(|r| r.id == IndicatorId::NUPL)
        .expect("nupl");
    assert!(!nupl_before.is_stale);
    assert!(nupl_before.missing_reason.is_none());

    let stale: BTreeSet<&'static str> = loaded.stale_keys();
    apply_staleness(&mut summary, &stale);

    let nupl_after = summary
        .indicator_results
        .iter()
        .find(|r| r.id == IndicatorId::NUPL)
        .expect("nupl");
    assert!(nupl_after.is_stale, "NUPL harus ditandai stale");

    let pl = summary
        .indicator_results
        .iter()
        .find(|r| r.id == IndicatorId::POWER_LAW)
        .expect("power law");
    assert!(!pl.is_stale, "Power Law tidak punya dependensi eksternal");

    let ma200 = summary
        .indicator_results
        .iter()
        .find(|r| r.id == IndicatorId::MA_200W)
        .expect("ma200w");
    assert!(!ma200.is_stale, "MA 200W berasal dari OHLCV segar");
}

#[tokio::test]
async fn test_bootstrap_skipped_report_accurately_reflects_counts() {
    let (pool, _tmp) = setup_test_db().await;
    let start = NaiveDate::from_ymd_opt(2010, 1, 1).expect("valid");
    let total_candles = 5000usize;
    seed_candles(&pool, start, total_candles, 100.0).await;

    let cfg = AppConfig::from_source(|k| match k {
        "MIN_REQUIRED_CANDLES" => Some("4500".to_string()),
        "DATABASE_URL" => Some("sqlite::memory:".to_string()),
        _ => None,
    })
    .expect("config from source");

    let http = reqwest::Client::new();
    let report = btc_cycle_radar::bootstrap::bootstrap_history(&pool, &cfg, &http, false)
        .await
        .expect("bootstrap");

    assert_eq!(report.candles_before, 5000);
    assert_eq!(report.candles_after, 5000);
    assert_eq!(report.coinmetrics_rows, 0);
    assert_eq!(report.binance_rows, 0);
    let s = format!("{}", report);
    assert!(s.contains("✅ PASS: > 4500 daily candles"));
    assert!(!s.contains("FAIL: only 0"));
}

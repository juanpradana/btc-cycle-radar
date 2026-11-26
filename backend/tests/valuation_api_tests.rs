//! Integration tests untuk endpoint HTTP /api/v1/valuation & /api/v1/indicators.
//!
//! Prinsip TDD: tes ditulis dulu (RED), baru implementasi (GREEN), lalu refactor.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use btc_cycle_radar::api::{router, AppState};
use btc_cycle_radar::config::AppConfig;
use btc_cycle_radar::db;
use chrono::{Duration, NaiveDate};
use http_body_util::BodyExt;
use sqlx::{query, SqlitePool};
use std::sync::Arc;
use tower::ServiceExt;

fn test_config(min_candles: i64) -> AppConfig {
    AppConfig::from_source(|k| match k {
        "MIN_REQUIRED_CANDLES" => Some(min_candles.to_string()),
        "DATABASE_URL" => Some("sqlite://unused.db".to_string()),
        _ => None,
    })
    .expect("config")
}

async fn setup_test(min_candles: i64) -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("api_test.db");
    let url = format!(
        "sqlite://{}",
        db_path.display().to_string().replace('\\', "/")
    );
    let pool = db::connect(&url).await.expect("connect db");
    seed_full_dataset(&pool).await;
    let cfg = test_config(min_candles);
    let state = Arc::new(AppState {
        pool,
        cfg,
        http: reqwest::Client::new(),
    });
    (router(state), tmp)
}

async fn seed_full_dataset(pool: &SqlitePool) {
    // 2500 candle daily (2020-01-01 s.d. ~2026-10-20) — cukup untuk SMA 350W (2450)
    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    let total_days = 2500usize;
    let mut price = 20_000.0;
    for i in 0..total_days {
        let d = start + Duration::days(i as i64);
        // Sinusoid ±2% agar tidak flat; inject spike di akhir untuk top-scenario
        let mut close = price * (1.0 + (i as f64 * 0.003).sin() * 0.02);
        if i > 2300 {
            // Simulasi naik ke ~$105k di akhir (puncak 2025)
            close = price * 5.0;
        }
        price = close;
        query("INSERT INTO btc_daily_ohlcv (date, close, source) VALUES (?, ?, 'test')")
            .bind(d.format("%Y-%m-%d").to_string())
            .bind(close)
            .execute(pool)
            .await
            .expect("seed candle");
    }

    // On-chain metrics (5 yang di-fetch loader)
    seed_onchain(pool, "nupl", start, total_days, 0.2).await;
    seed_onchain(pool, "true_market_mean", start, total_days, 30_000.0).await;
    seed_onchain(pool, "aviv", start, total_days, 1.2).await;
    seed_onchain(pool, "lth_realized_price", start, total_days, 18_000.0).await;
    seed_onchain(pool, "sth_mvrv", start, total_days, 1.0).await;

    // Intermarket (XAU & GSPC)
    seed_intermarket(pool, "XAU", start, total_days, 1800.0).await;
    seed_intermarket(pool, "GSPC", start, total_days, 3500.0).await;
}

async fn seed_onchain(
    pool: &SqlitePool,
    metric: &str,
    start: NaiveDate,
    count: usize,
    base_val: f64,
) {
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

async fn get_json(
    app: &axum::Router,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("build request");
    let resp = app.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn last_seeded_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid") + Duration::days(2499)
}

// ============================================================
// TESTS: /api/v1/valuation
// ============================================================

#[tokio::test]
async fn test_valuation_endpoint_returns_19_indicators_and_metadata() {
    let (app, _tmp) = setup_test(100).await;
    let (status, json) = get_json(&app, "/api/v1/valuation").await;

    assert_eq!(status, StatusCode::OK, "status: {:?}", json);
    assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));

    // 19 indikator
    let indicators = json.get("indicators").expect("indicators field").as_array().expect("indicators array");
    assert_eq!(indicators.len(), 19, "harus ada 19 indikator");

    // Setiap indikator punya field wajib
    for ind in indicators {
        assert!(ind.get("id").and_then(|v| v.as_str()).is_some(), "id missing: {:?}", ind);
        assert!(ind.get("name").and_then(|v| v.as_str()).is_some());
        assert!(ind.get("tier").and_then(|v| v.as_str()).is_some());
        assert!(ind.get("category").and_then(|v| v.as_str()).is_some());
        assert!(ind.get("status").and_then(|v| v.as_str()).is_some());
        assert!(ind.get("score").and_then(|v| v.as_f64()).is_some());
        assert!(ind.get("raw_value").is_some()); // Option<f64>
        assert!(ind.get("status_str").and_then(|v| v.as_str()).is_some());
        assert!(ind.get("is_stale").and_then(|v| v.as_bool()).is_some());
        assert!(ind.get("missing_reason").is_some()); // Option<string>
    }

    // Engine version
    assert_eq!(json.get("engine_version").and_then(|v| v.as_str()), Some("phase2-v1"));

    // Data quality
    let dq = json.get("data_quality").expect("data_quality");
    assert!(dq.get("is_stale").and_then(|v| v.as_bool()).is_some());
    assert!(dq.get("candles_used").and_then(|v| v.as_u64()).is_some());

    // Provenance
    let prov = json.get("provenance").expect("provenance").as_array().expect("provenance array");
    assert!(!prov.is_empty(), "provenance tidak boleh kosong");

    // As-of date
    let as_of = json.get("as_of").and_then(|v| v.as_str()).expect("as_of");
    assert_eq!(as_of, last_seeded_date().format("%Y-%m-%d").to_string());
}

#[tokio::test]
async fn test_valuation_endpoint_includes_provenance_for_ohlcv() {
    let (app, _tmp) = setup_test(100).await;
    let (_, json) = get_json(&app, "/api/v1/valuation").await;

    let prov = json.get("provenance").expect("provenance").as_array().expect("array");
    let sma200w = prov.iter().find(|f| f.get("field").and_then(|v| v.as_str()) == Some("sma_200w"))
        .expect("harus ada provenance sma_200w");

    assert_eq!(sma200w.get("source").and_then(|v| v.as_str()), Some("derived:ohlcv"));
    assert_eq!(sma200w.get("tolerance_days").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(sma200w.get("is_stale").and_then(|v| v.as_bool()), Some(false));
}

#[tokio::test]
async fn test_valuation_endpoint_as_of_param_mid_history() {
    let (app, _tmp) = setup_test(100).await;
    let as_of = "2023-06-15";
    let (status, json) = get_json(&app, &format!("/api/v1/valuation?as_of={}", as_of)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.get("as_of").and_then(|v| v.as_str()), Some(as_of));
    let candles_used = json.get("data_quality").and_then(|dq| dq.get("candles_used")).and_then(|v| v.as_u64());
    // 2020-01-01 s.d. 2023-06-15 ≈ 1261 hari
    assert!(candles_used.is_some_and(|c| c > 1200 && c < 1300), "candles_used: {:?}", candles_used);
}

#[tokio::test]
async fn test_valuation_endpoint_rejects_out_of_range_date() {
    let (app, _tmp) = setup_test(100).await;
    // Tanggal sebelum data (2020-01-01)
    let (status, json) = get_json(&app, "/api/v1/valuation?as_of=2019-12-31").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(json.get("error_code").and_then(|v| v.as_str()), Some("date_out_of_range"));
    assert!(json.get("error").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn test_valuation_endpoint_rejects_bad_date_format() {
    let (app, _tmp) = setup_test(100).await;
    let (status, json) = get_json(&app, "/api/v1/valuation?as_of=not-a-date").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json.get("error_code").and_then(|v| v.as_str()), Some("invalid_date"));
}

#[tokio::test]
async fn test_valuation_endpoint_no_candles_returns_503() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("empty.db");
    let url = format!("sqlite://{}", db_path.display().to_string().replace('\\', "/"));
    let pool = db::connect(&url).await.expect("connect");
    let cfg = test_config(100);
    let state = Arc::new(AppState { pool, cfg, http: reqwest::Client::new() });
    let app = router(state);

    let (status, json) = get_json(&app, "/api/v1/valuation").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json.get("error_code").and_then(|v| v.as_str()), Some("no_candles"));
}

#[tokio::test]
async fn test_valuation_endpoint_insufficient_coverage_returns_503() {
    let (app, _tmp) = setup_test(999_999).await; // min > available
    let (status, json) = get_json(&app, "/api/v1/valuation").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json.get("error_code").and_then(|v| v.as_str()), Some("insufficient_coverage"));
    let available = json.get("available").and_then(|v| v.as_i64());
    let required = json.get("required").and_then(|v| v.as_i64());
    assert!(available.is_some_and(|a| a > 0));
    assert_eq!(required, Some(999_999));
}

#[tokio::test]
async fn test_valuation_endpoint_marks_stale_onchain() {
    // Setup dengan on-chain 10 hari sebelum candle terakhir
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("stale.db");
    let url = format!("sqlite://{}", db_path.display().to_string().replace('\\', "/"));
    let pool = db::connect(&url).await.expect("connect");

    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    let total_days = 2500usize;
    let onchain_days = total_days - 10;

    for i in 0..total_days {
        let d = start + Duration::days(i as i64);
        let close = 20_000.0 * (1.0 + (i as f64 * 0.003).sin() * 0.02);
        query("INSERT INTO btc_daily_ohlcv (date, close, source) VALUES (?, ?, 'test')")
            .bind(d.format("%Y-%m-%d").to_string())
            .bind(close)
            .execute(&pool)
            .await
            .expect("seed");
    }
    seed_onchain(&pool, "nupl", start, onchain_days, 0.2).await;
    seed_intermarket(&pool, "XAU", start, total_days, 1800.0).await;
    seed_intermarket(&pool, "GSPC", start, total_days, 3500.0).await;

    let cfg = test_config(100);
    let state = Arc::new(AppState { pool, cfg, http: reqwest::Client::new() });
    let app = router(state);

    let (status, json) = get_json(&app, "/api/v1/valuation").await;
    assert_eq!(status, StatusCode::OK);

    // data_quality.is_stale = true
    assert_eq!(json.get("data_quality").and_then(|dq| dq.get("is_stale")).and_then(|v| v.as_bool()), Some(true));

    // stale_fields mengandung nupl
    let stale = json.get("data_quality").and_then(|dq| dq.get("stale_fields")).and_then(|v| v.as_array()).expect("stale_fields");
    assert!(stale.iter().any(|s| s.as_str() == Some("nupl")), "stale_fields: {:?}", stale);

    // NUPL indicator is_stale = true
    let nupl = json.get("indicators").and_then(|arr| arr.as_array())
        .and_then(|arr| arr.iter().find(|i| i.get("id").and_then(|v| v.as_str()) == Some("nupl")))
        .expect("nupl indicator");
    assert_eq!(nupl.get("is_stale").and_then(|v| v.as_bool()), Some(true));

    // MA_200W TIDAK stale (derivasi lokal OHLCV segar)
    let ma200 = json.get("indicators").and_then(|arr| arr.as_array())
        .and_then(|arr| arr.iter().find(|i| i.get("id").and_then(|v| v.as_str()) == Some("ma_200w")))
        .expect("ma_200w indicator");
    assert_eq!(ma200.get("is_stale").and_then(|v| v.as_bool()), Some(false));
}

/// Transparansi: data_quality.warnings wajib mengandung pemberitahuan
/// delay BGeometrics tier gratis ketika on-chain ≥ 5 hari basi.
/// Regresi: tanpa warning, UI menampilkan skor menggabungkan harga hari ini
/// dengan psikologi 6 hari lalu tanpa disclaim.
#[tokio::test]
async fn test_valuation_endpoint_warns_bgeometrics_free_tier_delay() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("delay.db");
    let url = format!("sqlite://{}", db_path.display().to_string().replace('\\', "/"));
    let pool = db::connect(&url).await.expect("connect");

    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    let total_days = 2500usize;
    let onchain_days = total_days - 6; // 6 hari basi → >= ambang 5 hari

    for i in 0..total_days {
        let d = start + Duration::days(i as i64);
        let close = 20_000.0 * (1.0 + (i as f64 * 0.003).sin() * 0.02);
        query("INSERT INTO btc_daily_ohlcv (date, close, source) VALUES (?, ?, 'test')")
            .bind(d.format("%Y-%m-%d").to_string())
            .bind(close)
            .execute(&pool)
            .await
            .expect("seed");
    }
    seed_onchain(&pool, "nupl", start, onchain_days, 0.2).await;
    seed_onchain(&pool, "true_market_mean", start, onchain_days, 30_000.0).await;
    seed_onchain(&pool, "aviv", start, onchain_days, 0.8).await;
    seed_onchain(&pool, "lth_realized_price", start, onchain_days, 40_000.0).await;
    seed_onchain(&pool, "sth_mvrv", start, onchain_days, 0.9).await;
    seed_intermarket(&pool, "XAU", start, total_days, 1800.0).await;
    seed_intermarket(&pool, "GSPC", start, total_days, 3500.0).await;

    let cfg = test_config(100);
    let state = Arc::new(AppState { pool, cfg, http: reqwest::Client::new() });
    let app = router(state);

    let (status, json) = get_json(&app, "/api/v1/valuation").await;
    assert_eq!(status, StatusCode::OK);

    let warnings = json.get("data_quality")
        .and_then(|dq| dq.get("warnings"))
        .and_then(|v| v.as_array())
        .expect("warnings array");
    let has_bgeo_warning = warnings.iter().any(|w| w.as_str().map(|s| s.contains("BGeometrics")).unwrap_or(false));
    assert!(has_bgeo_warning, "missing BGeometrics delay warning in warnings: {:?}", warnings);
}

#[tokio::test]
async fn test_valuation_snapshot_persisted_idempotent() {
    // Persistence ke indicator_snapshots tabel
    let (app, _tmp) = setup_test(100).await;

    // Panggil 2x
    let (_, json1) = get_json(&app, "/api/v1/valuation").await;
    let (_, json2) = get_json(&app, "/api/v1/valuation").await;

    // Snapshot date sama
    assert_eq!(json1.get("as_of"), json2.get("as_of"));
    // Query SQLite directly: the endpoint must persist exactly one idempotent row.
    let db_path = _tmp.path().join("api_test.db");
    let url = format!(
        "sqlite://{}",
        db_path.display().to_string().replace('\\', "/")
    );
    let pool = db::connect(&url).await.expect("reopen test db");
    let snapshot_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM indicator_snapshots WHERE snapshot_date = ? AND engine_version = 'phase2-v1'",
    )
    .bind(last_seeded_date().format("%Y-%m-%d").to_string())
    .fetch_one(&pool)
    .await
    .expect("count persisted snapshots");
    assert_eq!(snapshot_count, 1, "two identical calls should upsert one row");
}

// ============================================================
// TESTS: /api/v1/indicators
// ============================================================

#[tokio::test]
async fn test_indicators_endpoint_lists_registry_with_weights() {
    let (app, _tmp) = setup_test(100).await;
    let (status, json) = get_json(&app, "/api/v1/indicators").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));

    let list = json.get("indicators").expect("indicators").as_array().expect("array");
    assert_eq!(list.len(), 19, "harus 19 indikator");

    // Urut by rank
    for i in 0..19 {
        let rank = list[i].get("rank").and_then(|v| v.as_u64()).expect("rank");
        assert_eq!(rank, (i + 1) as u64, "rank urut di indeks {}", i);
    }

    // Field wajib per indikator
    for ind in list {
        assert!(ind.get("id").and_then(|v| v.as_str()).is_some());
        assert!(ind.get("name").and_then(|v| v.as_str()).is_some());
        assert!(ind.get("tier").and_then(|v| v.as_str()).is_some());
        assert!(ind.get("tier_weight").and_then(|v| v.as_f64()).is_some());
        assert!(ind.get("category").and_then(|v| v.as_str()).is_some());
        assert!(ind.get("category_weight").and_then(|v| v.as_f64()).is_some());
        assert!(ind.get("effective_weight").and_then(|v| v.as_f64()).is_some());
    }

    // category_weights map
    let cw = json.get("category_weights").expect("category_weights").as_object().expect("object");
    let sum: f64 = cw.values().filter_map(|v| v.as_f64()).sum();
    assert!((sum - 1.0).abs() < 1e-9, "category weights sum = {}", sum);
}

#[tokio::test]
async fn test_indicators_endpoint_as_of_param() {
    let (app, _tmp) = setup_test(100).await;
    let (status, json) = get_json(&app, "/api/v1/indicators?as_of=2023-06-15").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.get("as_of").and_then(|v| v.as_str()), Some("2023-06-15"));
}
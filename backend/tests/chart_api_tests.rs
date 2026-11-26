//! Integration tests untuk endpoint HTTP /api/v1/chart/*
//!
//! Menjamin endpoint chart mengembalikan data riil, validasi input berjalan,
//! dan tidak ada panic / unwrap pada jalur request.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use btc_cycle_radar::api::{router, AppState};
use btc_cycle_radar::config::AppConfig;
use btc_cycle_radar::db;
use chrono::{Duration, NaiveDate};
use http_body_util::BodyExt;
use serde_json::Value;
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

async fn setup_chart_test(total_candles: usize, min_candles: i64) -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("chart_api_test.db");
    let url = format!(
        "sqlite://{}",
        db_path.display().to_string().replace('\\', "/")
    );
    let pool = db::connect(&url).await.expect("connect db");
    seed_chart_dataset(&pool, total_candles).await;
    let cfg = test_config(min_candles);
    let state = Arc::new(AppState {
        pool,
        cfg,
        http: reqwest::Client::new(),
    });
    (router(state), tmp)
}

async fn seed_chart_dataset(pool: &SqlitePool, total_days: usize) {
    let start = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    let mut price = 10_000.0;
    for i in 0..total_days {
        let d = start + Duration::days(i as i64);
        let close = price * (1.0 + (i as f64 * 0.002).sin() * 0.015);
        price = close;
        query("INSERT INTO btc_daily_ohlcv (date, open, high, low, close, volume, source) VALUES (?, ?, ?, ?, ?, ?, 'test')")
            .bind(d.format("%Y-%m-%d").to_string())
            .bind(close * 0.99)
            .bind(close * 1.02)
            .bind(close * 0.98)
            .bind(close)
            .bind(1_000_000.0)
            .execute(pool)
            .await
            .expect("seed candle");
    }

    // Seed on-chain metrics: nupl, true_market_mean
    for i in 0..total_days {
        let d = start + Duration::days(i as i64);
        let date_str = d.format("%Y-%m-%d").to_string();
        let nupl_val = 0.35 + (i as f64 * 0.005).sin() * 0.2;
        query("INSERT INTO onchain_daily (metric, date, value, source) VALUES ('nupl', ?, ?, 'test')")
            .bind(&date_str)
            .bind(nupl_val)
            .execute(pool)
            .await
            .expect("seed nupl");
    }
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri(uri)
        .method("GET")
        .body(Body::empty())
        .expect("builder");
    let resp = app.clone().oneshot(req).await.expect("exec request");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("collect").to_bytes();
    let val: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, val)
}

#[tokio::test]
async fn test_price_chart_endpoint_success() {
    // 2500 candle daily agar 200W SMA terhitung
    let (app, _tmp) = setup_chart_test(2500, 2000).await;

    let (status, val) = get_json(&app, "/api/v1/chart/price").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(val["ok"], true);

    let points = val["points"].as_array().expect("points is array");
    assert!(!points.is_empty(), "points should not be empty");

    // Periksa titik terakhir memiliki close dan 200W SMA
    let last = points.last().unwrap();
    assert!(last["close"].as_f64().unwrap() > 0.0);
    assert!(last["date"].as_str().is_some());
    assert!(
        last["ma_200w"].as_f64().is_some(),
        "200W SMA harus terhitung pada titik terakhir setelah 2500 candle"
    );
}

#[tokio::test]
async fn test_price_chart_endpoint_days_filter() {
    let (app, _tmp) = setup_chart_test(500, 300).await;

    let (status, val) = get_json(&app, "/api/v1/chart/price?cadence=daily&days=45").await;
    assert_eq!(status, StatusCode::OK);
    let points = val["points"].as_array().expect("points");
    assert_eq!(points.len(), 45, "harus mengembalikan tepat 45 titik saat days=45");
}

#[tokio::test]
async fn test_onchain_chart_endpoint_valid_metric() {
    let (app, _tmp) = setup_chart_test(100, 50).await;

    let (status, val) = get_json(&app, "/api/v1/chart/onchain?metric=nupl").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(val["ok"], true);
    assert_eq!(val["metric"], "nupl");

    let points = val["points"].as_array().expect("points");
    assert_eq!(points.len(), 100);
    let first = &points[0];
    assert!(first["value"].as_f64().is_some());
}

#[tokio::test]
async fn test_onchain_chart_endpoint_unknown_metric_is_400() {
    let (app, _tmp) = setup_chart_test(10, 5).await;

    // Metrik yang tidak ada barisanya di onchain_daily bukan "kosong" — itu
    // permintaan yang tidak valid. Jangan balas 200 + array kosong.
    let (status, val) = get_json(&app, "/api/v1/chart/onchain?metric=unknown_random_metric").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(val["ok"], false);
    assert_eq!(val["error_code"], "chart_onchain_failed");
}

#[tokio::test]
async fn test_score_history_endpoint_snapshots_empty() {
    let (app, _tmp) = setup_chart_test(100, 50).await;

    let (status, val) = get_json(&app, "/api/v1/chart/score-history?method=snapshots").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(val["ok"], true);
    assert_eq!(val["method"], "snapshots");
    assert!(val["points"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_onchain_metrics_endpoint_lists_available_metrics() {
    let (app, _tmp) = setup_chart_test(20, 10).await;

    let (status, val) = get_json(&app, "/api/v1/chart/onchain-metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(val["ok"], true);

    let metrics = val["metrics"].as_array().expect("metrics array");
    assert!(!metrics.is_empty(), "seed test harus punya minimal satu metrik");
    let nupl = metrics
        .iter()
        .find(|m| m["metric"] == "nupl")
        .expect("nupl harus terdaftar");
    assert!(nupl["points"].as_i64().unwrap_or(0) > 0);
    assert!(nupl["first_date"].as_str().is_some());
    assert!(nupl["last_date"].as_str().is_some());
    assert!(nupl["sources"].as_array().is_some());

    // Hanya metrik yang benar-benar ada isinya — bukan daftar hardcoded.
    assert!(
        metrics.iter().all(|m| m["metric"] != "rhodl"),
        "metrik tanpa baris di DB tidak boleh muncul"
    );
}

#[tokio::test]
async fn test_score_history_prefers_materialized_table() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("materialized.db");
    let url = format!(
        "sqlite://{}",
        db_path.display().to_string().replace('\\', "/")
    );
    let pool = db::connect(&url).await.expect("connect");
    seed_chart_dataset(&pool, 60).await;

    // Materialisasi 8 titik: chart harus membacanya tanpa replay (instan).
    let dates = [
        "2020-01-10", "2020-01-20", "2020-01-30", "2020-02-09",
        "2020-02-19", "2020-02-29", "2020-03-10", "2020-03-20",
    ];
    for (i, d) in dates.iter().enumerate() {
        query(
            "INSERT INTO score_history (date, valuation_score, current_zone, spot_price, engine_version, step_days)
             VALUES (?, ?, 'TestZone', ?, 'test-engine', 30)",
        )
        .bind(*d)
        .bind(10.0 + i as f64)
        .bind(20_000.0 + i as f64 * 100.0)
        .execute(&pool)
        .await
        .expect("seed score_history");
    }

    let cfg = test_config(30);
    let state = Arc::new(AppState {
        pool,
        cfg,
        http: reqwest::Client::new(),
    });
    let app = router(state);

    let (status, val) = get_json(&app, "/api/v1/chart/score-history?method=auto&limit=50&step_days=30").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(val["ok"], true);
    assert_eq!(val["method"], "materialized");
    let pts = val["points"].as_array().expect("points");
    assert_eq!(pts.len(), 8);
    assert_eq!(pts[0]["date"], "2020-01-10");
    assert_eq!(pts[7]["date"], "2020-03-20");
    assert_eq!(pts[7]["spot_price"].as_f64(), Some(20_700.0));
    assert_eq!(pts[0]["current_zone"], "TestZone");
}

#[tokio::test]
async fn test_indicator_history_endpoint_single() {
    let (app, _tmp) = setup_chart_test(2500, 2000).await;

    // Single-indicator GET bersifat read-only: tanpa materialisasi → points kosong (graceful).
    let (status, val) = get_json(&app, "/api/v1/chart/indicator-history?id=nupl&step_days=30&limit=50").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(val["ok"], true);
    assert_eq!(val["indicator_id"], "nupl");
    assert!(val["points"].as_array().unwrap().is_empty());
    assert_eq!(val["has_history"], false);
}

#[tokio::test]
async fn test_indicator_history_endpoint_bulk() {
    let (app, _tmp) = setup_chart_test(100, 50).await;

    let (status, val) = get_json(&app, "/api/v1/chart/indicators-history?ids=nupl,ma_50w&step_days=30&limit=20").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(val["ok"], true);
    let series = val["series"].as_array().unwrap();
    assert_eq!(series.len(), 2);
    for s in series {
        assert!(s["indicator_id"].as_str().is_some());
        assert!(s["points"].as_array().is_some());
    }
}

#[tokio::test]
async fn test_indicator_specs_endpoint() {
    let (app, _tmp) = setup_chart_test(10, 5).await;

    let (status, val) = get_json(&app, "/api/v1/chart/indicator-specs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(val["ok"], true);
    assert_eq!(val["count"], 19);
    let indicators = val["indicators"].as_array().unwrap();
    assert_eq!(indicators.len(), 19);
    assert_eq!(indicators[0]["id"], "ma_50w");
    assert_eq!(indicators[0]["rank"], 1);
}

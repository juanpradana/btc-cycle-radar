//! Live bootstrap integration test (hits real APIs).
//! Run with: cargo test --test live_bootstrap -- --ignored --nocapture
//! Skipped by default (needs network).
use btc_cycle_radar::{bootstrap, config::AppConfig, db};
use tempfile::NamedTempFile;

#[tokio::test]
#[ignore]
async fn live_bootstrap_verifies_min_candles() {
    let tmp = NamedTempFile::new().expect("temp file");
    let db_url = format!("sqlite://{}", tmp.path().display());
    
    // env override untuk test
    std::env::set_var("DATABASE_URL", &db_url);
    std::env::set_var("MIN_REQUIRED_CANDLES", "4500");
    std::env::set_var("BOOTSTRAP_START", "2010-07-01");
    // use data-api.binance.vision (regional friendly)
    std::env::set_var("BINANCE_BASE_URLS", "https://data-api.binance.vision");

    let cfg = AppConfig::from_env().expect("config");
    let pool = db::connect(&cfg.database_url).await.expect("connect");
    let http = reqwest::Client::new();

    // run bootstrap (force=true untuk pasti fetch)
    let report = bootstrap::bootstrap_history(&pool, &cfg, &http, true).await
        .expect("bootstrap should succeed");

    println!("{}", report);

    // assertions
    assert!(report.coinmetrics_rows > 5000, "expected >5000 CoinMetrics rows, got {}", report.coinmetrics_rows);
    assert!(report.candles_after >= cfg.min_required_candles,
        "candles_after {} < min_required {}", report.candles_after, cfg.min_required_candles);
    assert!(report.first_date.is_some());
    assert!(report.last_date.is_some());
    
    // earliest should be around 2010-07-28
    if let Some(first) = report.first_date {
        assert!(first <= chrono::NaiveDate::from_ymd_opt(2010, 8, 1).unwrap(),
            "first date {} too late", first);
    }
}
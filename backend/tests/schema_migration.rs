//! Integration test: schema migration runs, tabel ada, dan upsert idempotent.
//! Memakai runtime query (`sqlx::query`) agar tidak butuh DATABASE_URL saat build.
use btc_cycle_radar::db;
use sqlx::Row;
use tempfile::NamedTempFile;

async fn fresh_pool() -> (sqlx::SqlitePool, NamedTempFile) {
    let tmp = NamedTempFile::new().expect("temp file");
    let db_url = format!("sqlite://{}", tmp.path().display());
    let pool = db::connect(&db_url).await.expect("connect");
    (pool, tmp)
}

#[tokio::test]
async fn migrations_run_and_tables_exist() {
    let (pool, _tmp) = fresh_pool().await;

    let tables = sqlx::query(
        "SELECT name FROM sqlite_master
         WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_sqlx_%'",
    )
    .fetch_all(&pool)
    .await
    .expect("query tables");
    let names: Vec<String> = tables.iter().map(|r| r.get::<String, _>("name")).collect();

    for expected in [
        "btc_daily_ohlcv",
        "intermarket_daily",
        "onchain_daily",
        "indicator_snapshots",
        "fetch_logs",
        "app_meta",
        "score_history",
        "indicator_history",
    ] {
        assert!(names.contains(&expected.to_string()), "missing table {expected}; got {names:?}");
    }

    // WAL mode aktif
    let pragma = sqlx::query("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .expect("pragma");
    let mode: String = pragma.get(0);
    assert_eq!(mode.to_lowercase(), "wal");

    // schema_version tercatat setelah migrasi
    let ver = db::get_meta(&pool, "schema_version").await.expect("get_meta");
    assert_eq!(ver.as_deref(), Some("0004"));
}

#[tokio::test]
async fn migration_is_idempotent() {
    let (pool, _tmp) = fresh_pool().await;
    // jalankan ulang migrasi -> tidak boleh error (semua IF NOT EXISTS)
    db::run_migrations(&pool).await.expect("second migration run");
    let ver = db::get_meta(&pool, "schema_version").await.expect("get_meta");
    assert_eq!(ver.as_deref(), Some("0004"));
}

#[tokio::test]
async fn upsert_daily_close_idempotent() {
    let (pool, _tmp) = fresh_pool().await;

    use btc_cycle_radar::config::DailyClose;
    use chrono::NaiveDate;

    let rows = vec![
        DailyClose { date: NaiveDate::from_ymd_opt(2021, 1, 1).unwrap(), close: 29000.0 },
        DailyClose { date: NaiveDate::from_ymd_opt(2021, 1, 2).unwrap(), close: 29500.0 },
    ];

    let n1 = btc_cycle_radar::market_data::upsert_daily_closes(&pool, &rows, "test")
        .await
        .expect("upsert1");
    assert_eq!(n1, 2);

    // insert ulang data sama -> update, bukan duplikat
    let n2 = btc_cycle_radar::market_data::upsert_daily_closes(&pool, &rows, "test2")
        .await
        .expect("upsert2");
    assert_eq!(n2, 2);

    let count = sqlx::query("SELECT COUNT(*) AS cnt FROM btc_daily_ohlcv")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count.get::<i64, _>("cnt"), 2);

    let src = sqlx::query("SELECT source FROM btc_daily_ohlcv WHERE date = '2021-01-01'")
        .fetch_one(&pool)
        .await
        .expect("fetch source");
    assert_eq!(src.get::<String, _>("source"), "test2");
}

#[tokio::test]
async fn upsert_daily_candle_overwrites_ohlcv() {
    let (pool, _tmp) = fresh_pool().await;

    use btc_cycle_radar::config::DailyCandle;
    use chrono::NaiveDate;

    let candle = DailyCandle {
        date: NaiveDate::from_ymd_opt(2021, 1, 1).unwrap(),
        open: Some(28000.0),
        high: Some(29500.0),
        low: Some(27800.0),
        close: 29000.0,
        volume: Some(1000.0),
    };

    let n1 = btc_cycle_radar::market_data::upsert_daily_candles(&pool, &[candle], "binance")
        .await
        .expect("upsert1");
    assert_eq!(n1, 1);

    let row = sqlx::query(
        "SELECT open, high, low, close, volume FROM btc_daily_ohlcv WHERE date = '2021-01-01'",
    )
    .fetch_one(&pool)
    .await
    .expect("fetch");
    assert!((row.get::<Option<f64>, _>("open").unwrap() - 28000.0).abs() < 1e-6);
    assert!((row.get::<Option<f64>, _>("high").unwrap() - 29500.0).abs() < 1e-6);
    assert!((row.get::<Option<f64>, _>("low").unwrap() - 27800.0).abs() < 1e-6);
    assert!((row.get::<f64, _>("close") - 29000.0).abs() < 1e-6);
    assert!((row.get::<Option<f64>, _>("volume").unwrap() - 1000.0).abs() < 1e-6);

    // provenance: baris yang di-upsert Binance harus tercatat source='binance'
    // (CoinMetrics hanya menyediakan close; Binance menyediakan OHLCV penuh)
    let src = sqlx::query("SELECT source FROM btc_daily_ohlcv WHERE date = '2021-01-01'")
        .fetch_one(&pool)
        .await
        .expect("fetch source");
    assert_eq!(src.get::<String, _>("source"), "binance");
}

#[tokio::test]
async fn upsert_candle_overwrites_source_after_close() {
    // simulasikan urutan bootstrap nyata: CoinMetrics close dulu, lalu Binance OHLCV
    let (pool, _tmp) = fresh_pool().await;

    use btc_cycle_radar::config::{DailyClose, DailyCandle};
    use chrono::NaiveDate;

    // 1) CoinMetrics insert close only
    let n1 = btc_cycle_radar::market_data::upsert_daily_closes(&pool, &[DailyClose {
        date: NaiveDate::from_ymd_opt(2021, 1, 1).unwrap(),
        close: 29000.0,
    }], "coinmetrics").await.expect("cm upsert");
    assert_eq!(n1, 1);

    let src = sqlx::query("SELECT source FROM btc_daily_ohlcv WHERE date = '2021-01-01'")
        .fetch_one(&pool).await.expect("fetch cm");
    assert_eq!(src.get::<String, _>("source"), "coinmetrics");

    // 2) Binance upsert candle full OHLCV — source should become 'binance'
    let n2 = btc_cycle_radar::market_data::upsert_daily_candles(&pool, &[DailyCandle {
        date: NaiveDate::from_ymd_opt(2021, 1, 1).unwrap(),
        open: Some(28000.0),
        high: Some(29500.0),
        low: Some(27800.0),
        close: 29200.0,
        volume: Some(1000.0),
    }], "binance").await.expect("binance upsert");
    assert_eq!(n2, 1);

    let src = sqlx::query("SELECT source FROM btc_daily_ohlcv WHERE date = '2021-01-01'")
        .fetch_one(&pool).await.expect("fetch binance");
    assert_eq!(src.get::<String, _>("source"), "binance");

    // OHLCV fields should be from Binance
    let row = sqlx::query(
        "SELECT open, high, low, close, volume FROM btc_daily_ohlcv WHERE date = '2021-01-01'",
    ).fetch_one(&pool).await.expect("fetch");
    assert!((row.get::<Option<f64>, _>("open").unwrap() - 28000.0).abs() < 1e-6);
    assert!((row.get::<f64, _>("close") - 29200.0).abs() < 1e-6);
}

#[tokio::test]
async fn log_fetch_and_quota_guard_counting() {
    use btc_cycle_radar::config::FetchLogEntry;
    use btc_cycle_radar::market_data::{fetches_today, log_fetch};

    let (pool, _tmp) = fresh_pool().await;

    log_fetch(&pool, &FetchLogEntry {
        source: "bgeometrics".into(),
        endpoint: "nupl".into(),
        status: "ok".into(),
        http_code: Some(200),
        rows: 1,
        message: None,
    })
    .await
    .expect("log_fetch");

    log_fetch(&pool, &FetchLogEntry {
        source: "bgeometrics".into(),
        endpoint: "mvrv".into(),
        status: "ok".into(),
        http_code: Some(200),
        rows: 1,
        message: None,
    })
    .await
    .expect("log_fetch");

    assert_eq!(fetches_today(&pool, "bgeometrics").await.expect("count"), 2);
    assert_eq!(fetches_today(&pool, "coinmetrics").await.expect("count"), 0);
}
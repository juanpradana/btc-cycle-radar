use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

/// Schema SQL di-embed saat compile (tanpa butuh DATABASE_URL di build time).
const SCHEMA_SQL: &str = include_str!("../migrations/0001_initial_schema.sql");
const SCHEMA_SQL_2: &str = include_str!("../migrations/0002_score_history.sql");
const SCHEMA_SQL_3: &str = include_str!("../migrations/0003_score_history_step.sql");

/// Koneksi SQLite pool dengan WAL mode, busy_timeout >= 30s, FK on.
pub async fn connect(database_url: &str) -> Result<SqlitePool> {
    ensure_sqlite_parent_dir(database_url)?;
    let opts = SqliteConnectOptions::from_str(database_url)
        .with_context(|| format!("invalid DATABASE_URL: {database_url}"))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(30))
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;
    run_migrations(&pool).await?;
    Ok(pool)
}

/// Terapkan schema (idempotent: semua statement IF NOT EXISTS).
pub async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(SCHEMA_SQL)
        .execute(pool)
        .await
        .context("apply schema migration 0001")?;
    sqlx::raw_sql(SCHEMA_SQL_2)
        .execute(pool)
        .await
        .context("apply schema migration 0002")?;
    // 0003: ALTER TABLE tidak punya IF NOT EXISTS di SQLite — cek dulu kolomnya.
    let has_step: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('score_history') WHERE name = 'step_days'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    if has_step == 0 {
        sqlx::raw_sql(SCHEMA_SQL_3)
            .execute(pool)
            .await
            .context("apply schema migration 0003")?;
    }
    set_meta(pool, "schema_version", "0003").await?;
    Ok(())
}

/// Ekstrak path file dari sqlite://... URL (kalau in-memory -> None)
fn sqlite_file_path(url: &str) -> Option<PathBuf> {
    let url = url.strip_prefix("sqlite:")?;
    let mut url = url.strip_prefix("//")?;
    if url.starts_with(":memory:") || url.is_empty() {
        return None;
    }
    // Normalisasi Windows: jika URL menghasilkan "/C:/foo", buang slash pembuka
    if url.starts_with('/') && url.len() > 2 && url.as_bytes()[2] == b':' {
        url = &url[1..];
    }
    // Potong query parameters jika ada (mis. sqlite://foo.db?mode=rwc)
    let path_str = url.split('?').next().unwrap_or(url);
    Some(PathBuf::from(path_str))
}

fn ensure_sqlite_parent_dir(database_url: &str) -> Result<()> {
    if let Some(path) = sqlite_file_path(database_url) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir for {}", path.display()))?;
        }
    }
    Ok(())
}

/// Baca nilai meta (key-value)
pub async fn get_meta(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM app_meta WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
}

/// Tulis nilai meta (upsert)
pub async fn set_meta(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO app_meta(key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Ringkasan jumlah & rentang candle
#[derive(Debug, Clone, Default)]
pub struct CandleBounds {
    pub count: i64,
    pub first_date: Option<String>,
    pub last_date: Option<String>,
}

pub async fn candle_bounds(pool: &SqlitePool) -> Result<CandleBounds> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS cnt, MIN(date) AS first_date, MAX(date) AS last_date
         FROM btc_daily_ohlcv",
    )
    .fetch_one(pool)
    .await?;
    Ok(CandleBounds {
        count: row.get::<i64, _>("cnt"),
        first_date: row.try_get::<Option<String>, _>("first_date")?,
        last_date: row.try_get::<Option<String>, _>("last_date")?,
    })
}

/// Hitung fetch hari ini untuk sumber tertentu (wrapper re-export dari market_data).
pub async fn fetches_today(pool: &SqlitePool, source: &str) -> Result<i64> {
    crate::market_data::fetches_today(pool, source).await
}

/// Ambil tanggal terbaru per metrik untuk sumber tertentu (mis. "bgeometrics").
pub async fn latest_onchain_by_source(
    pool: &SqlitePool,
    source: &str,
) -> Result<std::collections::HashMap<String, chrono::NaiveDate>> {
    let rows = sqlx::query(
        "SELECT metric, MAX(date) as max_date FROM onchain_daily
         WHERE source = ? GROUP BY metric",
    )
    .bind(source)
    .fetch_all(pool)
    .await?;

    let mut out = std::collections::HashMap::new();
    for r in rows {
        use sqlx::Row;
        let metric: String = r.get("metric");
        if let Ok(Some(d_str)) = r.try_get::<Option<String>, _>("max_date") {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(&d_str, "%Y-%m-%d") {
                out.insert(metric, d);
            }
        }
    }
    Ok(out)
}

/// Hitung jumlah baris per metrik untuk sumber tertentu (audit kelengkapan histori).
pub async fn onchain_counts_by_source(
    pool: &SqlitePool,
    source: &str,
) -> Result<std::collections::HashMap<String, i64>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT metric, COUNT(*) AS cnt FROM onchain_daily
         WHERE source = ? GROUP BY metric",
    )
    .bind(source)
    .fetch_all(pool)
    .await?;

    let mut out = std::collections::HashMap::new();
    for r in rows {
        let metric: String = r.get("metric");
        let cnt: i64 = r.get("cnt");
        out.insert(metric, cnt);
    }
    Ok(out)
}

/// Ambil seluruh seri harian untuk satu metrik (terurut naik tanggal).
/// Dipakai untuk derivasi on-chain (mis. lth_realized_pl_ratio = profit_lth / loss_lth).
pub async fn fetch_onchain_series(
    pool: &SqlitePool,
    metric: &str,
) -> Result<Vec<(chrono::NaiveDate, f64)>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT date, value FROM onchain_daily
         WHERE metric = ? AND value IS NOT NULL
         ORDER BY date ASC",
    )
    .bind(metric)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for r in rows {
        let date_str: String = r.get("date");
        let val: f64 = r.get("value");
        if let Ok(d) = chrono::NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") {
            if val.is_finite() {
                out.push((d, val));
            }
        }
    }
    Ok(out)
}
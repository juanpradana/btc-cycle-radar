// One-shot backfill: materialkan `score_history` + `indicator_history` (19 indikator)
// untuk step_days tertentu, dari evaluasi engine riil (tanpa angka palsu).
use sqlx::SqlitePool;
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let step_days: i64 = args
        .get(1)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(7, 365);

    let db_path = r"D:\Hermes\btc-cycle-radar\backend\data\verify.db";
    let url = format!("sqlite://{}", db_path.replace('\\', "/"));
    let pool = SqlitePool::connect(&url).await?;
    // Pastikan skema (termasuk 0004 indicator_history) sudah terpasang.
    btc_cycle_radar::db::run_migrations(&pool).await?;

    let min_candles = 4500;
    let engine_version = btc_cycle_radar::engine::service::CURRENT_ENGINE_VERSION;

    let (min_date, max_date): (String, String) =
        sqlx::query_as("SELECT MIN(date), MAX(date) FROM btc_daily_ohlcv")
            .fetch_one(&pool)
            .await?;

    let start = chrono::NaiveDate::parse_from_str(&min_date, "%Y-%m-%d")?;
    let end = chrono::NaiveDate::parse_from_str(&max_date, "%Y-%m-%d")?;

    println!("DB        : verify.db");
    println!("Range     : {} .. {}", start, end);
    println!("step_days : {}", step_days);

    let mut samples = Vec::new();
    let mut cur = start;
    while cur <= end {
        samples.push(cur);
        cur += chrono::Duration::days(step_days);
    }
    println!("Samples   : {}", samples.len());

    let start_t = Instant::now();
    let mut success = 0usize;
    let mut skipped = 0usize;
    let mut indicator_rows = 0usize;

    for as_of in samples {
        match btc_cycle_radar::engine::loader::load_market_input(&pool, Some(as_of), min_candles)
            .await
        {
            Ok(loaded) => {
                let summary = btc_cycle_radar::engine::evaluate_all_indicators(&loaded.input);
                let date_str = as_of.format("%Y-%m-%d").to_string();

                sqlx::query(
                    "INSERT OR REPLACE INTO score_history (date, valuation_score, current_zone, spot_price, engine_version, step_days)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(&date_str)
                .bind(summary.valuation_score)
                .bind(format!("{:?}", summary.current_zone))
                .bind(summary.spot_price)
                .bind(engine_version)
                .bind(step_days)
                .execute(&pool)
                .await?;

                for res in &summary.indicator_results {
                    btc_cycle_radar::chart::write_indicator_history(
                        &pool,
                        res.id.as_str(),
                        &as_of,
                        step_days,
                        engine_version,
                        summary.spot_price,
                        res,
                    )
                    .await?;
                    indicator_rows += 1;
                }

                success += 1;
                if success % 20 == 0 {
                    println!("  {} titik sampel...", success);
                }
            }
            Err(_e) => {
                skipped += 1;
            }
        }
    }

    println!(
        "Done! titik={}, dilewati={}, baris indicator_history={}, waktu={:.2}s",
        success,
        skipped,
        indicator_rows,
        start_t.elapsed().as_secs_f64()
    );

    let (cnt,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM score_history WHERE step_days = ?")
            .bind(step_days)
            .fetch_one(&pool)
            .await?;
    println!("score_history (step_days={}) : {} baris", step_days, cnt);

    let (icnt,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM indicator_history WHERE step_days = ?")
            .bind(step_days)
            .fetch_one(&pool)
            .await?;
    let (idistinct,): (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT indicator_id) FROM indicator_history WHERE step_days = ?",
    )
    .bind(step_days)
    .fetch_one(&pool)
    .await?;
    println!(
        "indicator_history (step_days={}) : {} baris, {} indikator unik",
        step_days, icnt, idistinct
    );

    Ok(())
}

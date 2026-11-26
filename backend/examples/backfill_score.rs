// One-shot backfill: run replay for step=30, limit=200 and populate score_history
use sqlx::SqlitePool;
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = r"D:\Hermes\btc-cycle-radar\backend\data\verify.db";
    let url = format!("sqlite://{}", db_path.replace('\\', "/"));
    let pool = SqlitePool::connect(&url).await?;
    
    // Load all engine deps
    let min_candles = 4500;
    let engine_version = btc_cycle_radar::engine::service::CURRENT_ENGINE_VERSION;
    
    // Get date range from btc_daily_ohlcv
    let (min_date, max_date): (String, String) = sqlx::query_as(
        "SELECT MIN(date), MAX(date) FROM btc_daily_ohlcv"
    ).fetch_one(&pool).await?;
    
    let start = chrono::NaiveDate::parse_from_str(&min_date, "%Y-%m-%d")?;
    let end = chrono::NaiveDate::parse_from_str(&max_date, "%Y-%m-%d")?;
    
    println!("Range: {} to {}", start, end);
    
    // Generate sample dates (every 30 days)
    let mut samples = Vec::new();
    let mut cur = start;
    while cur <= end {
        samples.push(cur);
        cur = cur + chrono::Duration::days(30);
    }
    println!("Total sample points: {}", samples.len());
    
    let start_t = Instant::now();
    let mut success = 0;
    let mut skipped = 0;
    
    for as_of in samples {
        match btc_cycle_radar::engine::loader::load_market_input(&pool, Some(as_of), min_candles).await {
            Ok(loaded) => {
                let summary = btc_cycle_radar::engine::evaluate_all_indicators(&loaded.input);
                let zone_str = format!("{:?}", summary.current_zone);
                let date_str = as_of.format("%Y-%m-%d").to_string();
                
                sqlx::query(
                    "INSERT OR REPLACE INTO score_history (date, valuation_score, current_zone, spot_price, engine_version, step_days)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(&date_str)
                .bind(summary.valuation_score)
                .bind(&zone_str)
                .bind(summary.spot_price)
                .bind(&engine_version)
                .bind(30)
                .execute(&pool)
                .await?;
                
                success += 1;
                if success % 20 == 0 { println!("  {} rows...", success); }
            }
            Err(_e) => {
                skipped += 1;
            }
        }
    }
    
    let dur = start_t.elapsed();
    println!("Done! Success: {}, Skipped: {}, Time: {:.2}s", success, skipped, dur.as_secs_f64());
    
    // Verify
    let (cnt,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM score_history").fetch_one(&pool).await?;
    println!("Total rows in score_history: {}", cnt);
    let (min_d, max_d): (String, String) = sqlx::query_as("SELECT MIN(date), MAX(date) FROM score_history").fetch_one(&pool).await?;
    println!("Date range: {} to {}", min_d, max_d);
    
    Ok(())
}

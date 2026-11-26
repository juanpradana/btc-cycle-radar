//! Connector FRED (Federal Reserve Economic Data) untuk series S&P 500 (SP500).
//!
//! API: https://fred.stlouisfed.org/graph/fredgraph.csv?id=SP500
//! - Tanpa API key (public CSV endpoint)
//! - Daily business days (senin-jumat, libur AS dikecualikan)
//! - Kolom: `observation_date,SP500`
//! - Nilai `.` = missing (hapus)
//! - Simbol internal: `GSPC` (standar Yahoo/Financial untuk S&P 500 index)

use super::{upsert_intermarket, IntermarketValue};
use crate::market_data::log_fetch;
use crate::config::FetchLogEntry;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use reqwest::Client;
use sqlx::SqlitePool;
use std::str::FromStr;

const FRED_BASE: &str = "https://fred.stlouisfed.org/graph/fredgraph.csv";
const SOURCE_NAME: &str = "fred";
const SP500_SERIES_ID: &str = "SP500";
const INTERNAL_SYMBOL: &str = "GSPC";

#[derive(Debug)]
struct FredRecord {
    date: NaiveDate,
    close: f64,
}

impl FromStr for FredRecord {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.trim().split(',').collect();
        if parts.len() != 2 {
            return Err(anyhow::anyhow!("expected 2 columns, got {}", parts.len()));
        }
        let date_str = parts[0].trim();
        let value_str = parts[1].trim();

        if value_str == "." || value_str.is_empty() {
            return Err(anyhow::anyhow!("missing value"));
        }

        let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
            .with_context(|| format!("parse date '{}'", date_str))?;
        let close = value_str.parse::<f64>()
            .with_context(|| format!("parse close '{}'", value_str))?;

        if !close.is_finite() || close <= 0.0 {
            return Err(anyhow::anyhow!("non-positive close: {}", close));
        }

        Ok(FredRecord { date, close })
    }
}

pub struct FredClient {
    client: Client,
    base_url: String,
    series_id: String,
}

impl FredClient {
    pub fn new(client: Client, base_url: String, series_id: String) -> Self {
        Self { client, base_url, series_id }
    }

    pub fn default_client(client: Client) -> Self {
        Self::new(client, FRED_BASE.to_string(), SP500_SERIES_ID.to_string())
    }

    /// Fetch FULL history (tanpa parameter `cosd` = sejak awal series).
    pub async fn fetch_and_save_full_history(&self, pool: &SqlitePool) -> Result<usize> {
        let url = format!("{}?id={}", self.base_url, self.series_id);
        self.fetch_csv_and_save(pool, &url).await
    }

    /// Incremental fetch: hanya dari `cosd` (observation start date) ke depan.
    pub async fn fetch_and_save_incremental(
        &self,
        pool: &SqlitePool,
        start_date: NaiveDate,
    ) -> Result<usize> {
        let url = format!(
            "{}?id={}&cosd={}",
            self.base_url,
            self.series_id,
            start_date.format("%Y-%m-%d")
        );
        self.fetch_csv_and_save(pool, &url).await
    }

    async fn fetch_csv_and_save(&self, pool: &SqlitePool, url: &str) -> Result<usize> {
        let res = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {}", url))?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: SOURCE_NAME.into(),
                    endpoint: format!("fred/{}", self.series_id),
                    status: "error".into(),
                    http_code: Some(status.as_u16() as i32),
                    rows: 0,
                    message: Some(format!("HTTP {}: {}", status, &body[..body.len().min(120)])),
                },
            )
            .await?;
            return Err(anyhow::anyhow!("FRED {} HTTP {}: {}", self.series_id, status, body));
        }

        let text = res.text().await.context("read FRED CSV response")?;

        let mut values = Vec::new();
        let mut parse_errors = 0;

        for (line_num, line) in text.lines().enumerate() {
            if line_num == 0 {
                // Header: "observation_date,SP500"
                continue;
            }
            match line.parse::<FredRecord>() {
                Ok(rec) => values.push(IntermarketValue {
                    date: rec.date,
                    close: rec.close,
                }),
                Err(_) => {
                    parse_errors += 1;
                    if parse_errors <= 5 {
                        tracing::warn!("FRED parse error line {}: {}", line_num + 1, line);
                    }
                }
            }
        }

        values.sort_by_key(|v| v.date);
        values.dedup_by_key(|v| v.date);

        let rows = values.len();
        upsert_intermarket(pool, INTERNAL_SYMBOL, SOURCE_NAME, &values).await?;

        log_fetch(
            pool,
            &FetchLogEntry {
                source: SOURCE_NAME.into(),
                endpoint: format!("fred/{}", self.series_id),
                status: "ok".into(),
                http_code: Some(200),
                rows: rows as i64,
                message: if parse_errors > 0 {
                    Some(format!("{} baris parse error (dilewati)", parse_errors))
                } else {
                    None
                },
            },
        )
        .await?;

        Ok(rows)
    }
}

/// Validasi unit test dengan data riil (verifikasi 2026-11-26).
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn parse_fred_record_valid() {
        let rec = "2026-11-25,5918.54".parse::<FredRecord>().expect("valid");
        assert_eq!(rec.date, NaiveDate::from_ymd_opt(2026, 11, 25).expect("valid"));
        assert!((rec.close - 5918.54).abs() < 0.01);
    }

    #[test]
    fn parse_fred_record_missing_dot() {
        let res = "2026-11-26,.".parse::<FredRecord>();
        assert!(res.is_err());
    }

    #[test]
    fn parse_fred_record_missing_empty() {
        let res = "2026-11-26,".parse::<FredRecord>();
        assert!(res.is_err());
    }

    #[test]
    fn parse_fred_record_header_skipped() {
        let res = "observation_date,SP500".parse::<FredRecord>();
        assert!(res.is_err());
    }
}
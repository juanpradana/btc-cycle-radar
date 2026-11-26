//! Connector Yahoo Finance chart API (`v8/finance/chart/<symbol>`).
//!
//! Gratis, tanpa API key. Simbol yang didukung:
//! - Gold: `GC=F` (Gold Futures di CME Comex) -> disimpan sebagai simbol `XAU`
//! - S&P 500: `^GSPC` -> disimpan sebagai simbol `GSPC`
//! - US Dollar Index: `DX-Y.NYB` (opsional) -> `DXY`

use super::{upsert_intermarket, IntermarketValue};
use crate::market_data::log_fetch;
use crate::config::FetchLogEntry;
use anyhow::{Context, Result};
use chrono::DateTime;
use reqwest::Client;
use serde::Deserialize;
use sqlx::SqlitePool;

pub const DEFAULT_YAHOO_BASE: &str = "https://query1.finance.yahoo.com/v8/finance/chart";

pub struct YahooFinanceClient {
    client: Client,
    base_url: String,
}

impl YahooFinanceClient {
    pub fn new(client: Client, base_url: String) -> Self {
        Self { client, base_url }
    }

    pub fn default_client(client: Client) -> Self {
        Self::new(client, DEFAULT_YAHOO_BASE.to_string())
    }

    /// Ambil candle harian untuk `symbol_query` (mis. "GC=F") dan simpan dengan `internal_symbol` (mis. "XAU").
    /// `range` bisa "1mo", "1y", "5y", "max".
    pub async fn fetch_and_save(
        &self,
        pool: &SqlitePool,
        symbol_query: &str,
        internal_symbol: &str,
        range: &str,
    ) -> Result<usize> {
        let url = format!(
            "{}/{}?interval=1d&range={}",
            self.base_url.trim_end_matches('/'),
            symbol_query,
            range
        );

        let res = self
            .client
            .get(&url)
            .header("User-Agent", "btc-cycle-radar/0.1")
            .send()
            .await?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "yahoo".into(),
                    endpoint: format!("chart/{}", symbol_query),
                    status: "error".into(),
                    http_code: Some(status.as_u16() as i32),
                    rows: 0,
                    message: Some(format!("HTTP {}: {}", status, &body[..body.len().min(120)])),
                },
            )
            .await?;
            return Err(anyhow::anyhow!("Yahoo Finance HTTP {}: {}", status, body));
        }

        let body = res.text().await.context("read Yahoo response")?;
        let parsed = parse_yahoo_chart(&body)?;
        let rows = parsed.len();

        upsert_intermarket(pool, internal_symbol, "yahoo", &parsed).await?;

        log_fetch(
            pool,
            &FetchLogEntry {
                source: "yahoo".into(),
                endpoint: format!("chart/{}", symbol_query),
                status: "ok".into(),
                http_code: Some(200),
                rows: rows as i64,
                message: None,
            },
        )
        .await?;

        Ok(rows)
    }

    /// Fetch standar: Gold ("GC=F" -> "XAU") dan SPX ("^GSPC" -> "GSPC") rentang 5 tahun.
    pub async fn fetch_standard_pair(&self, pool: &SqlitePool) -> Result<(usize, usize)> {
        let gold_rows = self.fetch_and_save(pool, "GC=F", "XAU", "5y").await?;
        let spx_rows = self.fetch_and_save(pool, "^GSPC", "GSPC", "5y").await?;
        Ok((gold_rows, spx_rows))
    }
}

// ---------------------------------------------------------------------------
// Parser JSON Yahoo v8 chart
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct YahooChartResponse {
    chart: YahooChart,
}

#[derive(Debug, Deserialize)]
struct YahooChart {
    result: Option<Vec<YahooResultItem>>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct YahooResultItem {
    timestamp: Option<Vec<i64>>,
    indicators: Option<YahooIndicators>,
}

#[derive(Debug, Deserialize)]
struct YahooIndicators {
    quote: Option<Vec<YahooQuote>>,
}

#[derive(Debug, Deserialize)]
struct YahooQuote {
    close: Option<Vec<Option<f64>>>,
}

pub fn parse_yahoo_chart(body: &str) -> Result<Vec<IntermarketValue>> {
    let resp: YahooChartResponse = serde_json::from_str(body).context("parse Yahoo chart JSON")?;
    if let Some(err) = resp.chart.error {
        return Err(anyhow::anyhow!("Yahoo error: {:?}", err));
    }

    let items = resp.chart.result.unwrap_or_default();
    let Some(first) = items.into_iter().next() else {
        return Ok(Vec::new());
    };

    let timestamps = first.timestamp.unwrap_or_default();
    let quotes = first.indicators.and_then(|ind| ind.quote).unwrap_or_default();
    let closes = quotes
        .into_iter()
        .next()
        .and_then(|q| q.close)
        .unwrap_or_default();

    let mut out = Vec::new();
    for (ts, close_opt) in timestamps.into_iter().zip(closes.into_iter()) {
        let Some(close) = close_opt else { continue };
        if !close.is_finite() || close <= 0.0 {
            continue;
        }
        let dt = DateTime::from_timestamp(ts, 0);
        let Some(dt) = dt else { continue };
        let date = dt.date_naive();
        out.push(IntermarketValue { date, close });
    }

    Ok(out)
}

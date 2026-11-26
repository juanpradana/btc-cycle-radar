//! Endpoint data chart untuk dashboard (read-only).
//!
//! Semua data berasal dari tabel riil (`btc_daily_ohlcv`, `onchain_daily`,
//! `indicator_snapshots`). Tidak ada angka sintetis: titik yang tidak bisa
//! dihitung (coverage kurang, metrik tidak tersedia) dikembalikan sebagai
//! `null`/dilewati, bukan diisi nilai palsu.

use anyhow::{Context, Result};
use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;

/// Satu titik chart harga (close + MA weekly).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceChartPoint {
    pub date: String,
    pub close: f64,
    pub ma_200w: Option<f64>,
    pub ma_50w: Option<f64>,
    pub ma_30w: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceChartResponse {
    pub ok: bool,
    pub points: Vec<PriceChartPoint>,
    pub first_date: String,
    pub last_date: String,
    pub cadence: String,
}

/// Satu titik chart metrik on-chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnchainChartPoint {
    pub date: String,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnchainChartResponse {
    pub ok: bool,
    pub metric: String,
    pub source: String,
    pub points: Vec<OnchainChartPoint>,
    pub first_date: Option<String>,
    pub last_date: Option<String>,
}

/// Satu titik historial skor valuasi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreHistoryPoint {
    pub date: String,
    pub valuation_score: Option<f64>,
    pub current_zone: Option<String>,
    pub spot_price: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreHistoryResponse {
    pub ok: bool,
    pub points: Vec<ScoreHistoryPoint>,
    pub engine_version: String,
    pub method: String,
    pub warnings: Vec<String>,
}

#[derive(Deserialize, Default)]
pub struct PriceChartQuery {
    /// Batasi N hari terakhir (opsional; default seluruh histori).
    pub days: Option<i64>,
    /// "weekly" (default) | "daily"
    pub cadence: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct OnchainChartQuery {
    /// Nama metrik internal (wajib), mis. `nupl`, `mvrv`, `supply_in_profit_pct`.
    pub metric: String,
    pub source: Option<String>,
    pub days: Option<i64>,
}

#[derive(Deserialize, Default)]
pub struct ScoreHistoryQuery {
    /// Interval sampel dalam hari (default 30, minimum 7).
    pub step_days: Option<i64>,
    /// Maksimum titik (default 90).
    pub limit: Option<i64>,
    /// "auto" (default) = pakai snapshot DB bila cukup, selain itu replay.
    /// "replay" = paksa hitung ulang, "snapshots" = hanya baca DB.
    pub method: Option<String>,
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

/// Simple moving average rolling atas seri `(date, close)`.
fn compute_sma(series: &[(NaiveDate, f64)], window: usize) -> Vec<(NaiveDate, f64)> {
    if window == 0 || series.len() < window {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(series.len() - window + 1);
    let mut sum: f64 = series.iter().take(window).map(|(_, v)| *v).sum();
    out.push((series[window - 1].0, sum / window as f64));
    for i in window..series.len() {
        sum += series[i].1 - series[i - window].1;
        out.push((series[i].0, sum / window as f64));
    }
    out
}

/// Ambil seluruh close harian (ascending). Close non-finite dibuang.
async fn fetch_daily_close(pool: &SqlitePool) -> Result<Vec<(NaiveDate, f64)>> {
    let rows = sqlx::query("SELECT date, close FROM btc_daily_ohlcv ORDER BY date ASC")
        .fetch_all(pool)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let d: String = r.get("date");
        let c: f64 = r.get("close");
        if let (Some(d), true) = (parse_date(&d), c.is_finite()) {
            out.push((d, c));
        }
    }
    Ok(out)
}

/// Ambil titik sampling setiap `step_days` hari, mundur dari titik terakhir.
fn sample_backwards(series: &[(NaiveDate, f64)], step_days: i64, limit: usize) -> Vec<(NaiveDate, f64)> {
    if series.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cursor = *series.last().map(|(d, _)| d).unwrap_or(&series[0].0);
    let first = series[0].0;
    let step = step_days.max(1);

    // Map tanggal -> close untuk lookup cepat (series harian penuh).
    let lookup: HashMap<NaiveDate, f64> = series.iter().cloned().collect();

    while cursor >= first && out.len() < limit {
        // Gunakan candle tepat pada tanggal itu; jika tidak ada (market tutup),
        // mundur maksimum 7 hari untuk menemukan candle terdekat.
        let mut probe = cursor;
        let mut found = None;
        for _ in 0..8 {
            if let Some(v) = lookup.get(&probe) {
                found = Some((probe, *v));
                break;
            }
            if probe <= first {
                break;
            }
            probe = probe - Duration::days(1);
        }
        if let Some(pt) = found {
            out.push(pt);
        }
        cursor = cursor - Duration::days(step);
    }
    out.reverse();
    out
}

// ---------------------------------------------------------------------------
// Chart harga + MA
// ---------------------------------------------------------------------------

/// GET /api/v1/chart/price
pub async fn get_price_chart(
    pool: &SqlitePool,
    query: PriceChartQuery,
) -> Result<PriceChartResponse> {
    let daily = fetch_daily_close(pool).await?;
    if daily.is_empty() {
        anyhow::bail!("tidak ada candle di btc_daily_ohlcv");
    }

    let cadence = query
        .cadence
        .as_deref()
        .unwrap_or("weekly")
        .to_ascii_lowercase();

    // MA dihitung pada seri harian memakai window minggu (7*N hari),
    // lalu titik chart diambil mingguan (Senin) atau harian.
    let ma_200w_series = compute_sma(&daily, 200 * 7);
    let ma_50w_series = compute_sma(&daily, 50 * 7);
    let ma_30w_series = compute_sma(&daily, 30 * 7);

    let ma200: HashMap<NaiveDate, f64> = ma_200w_series.into_iter().collect();
    let ma50: HashMap<NaiveDate, f64> = ma_50w_series.into_iter().collect();
    let ma30: HashMap<NaiveDate, f64> = ma_30w_series.into_iter().collect();

    let selected: Vec<(NaiveDate, f64)> = if cadence == "daily" {
        match query.days {
            Some(days) => sample_backwards(&daily, 1, days.max(1) as usize),
            None => daily.clone(),
        }
    } else {
        let mondays: Vec<(NaiveDate, f64)> = daily
            .iter()
            .filter(|(d, _)| d.weekday() == chrono::Weekday::Mon)
            .cloned()
            .collect();
        match query.days {
            Some(days) => {
                let limit = ((days / 7).max(1)) as usize;
                if mondays.len() > limit {
                    mondays[mondays.len() - limit..].to_vec()
                } else {
                    mondays
                }
            }
            None => mondays,
        }
    };

    let mut points = Vec::with_capacity(selected.len());
    for (date, close) in selected {
        points.push(PriceChartPoint {
            date: date.format("%Y-%m-%d").to_string(),
            close,
            ma_200w: ma200.get(&date).copied(),
            ma_50w: ma50.get(&date).copied(),
            ma_30w: ma30.get(&date).copied(),
        });
    }

    let first_date = points
        .first()
        .map(|p| p.date.clone())
        .unwrap_or_default();
    let last_date = points.last().map(|p| p.date.clone()).unwrap_or_default();

    Ok(PriceChartResponse {
        ok: true,
        points,
        first_date,
        last_date,
        cadence,
    })
}

// ---------------------------------------------------------------------------
// Chart metrik on-chain
// ---------------------------------------------------------------------------

/// GET /api/v1/chart/onchain
pub async fn get_onchain_chart(
    pool: &SqlitePool,
    query: OnchainChartQuery,
) -> Result<OnchainChartResponse> {
    let metric = query.metric.trim().to_string();
    if metric.is_empty() {
        anyhow::bail!("parameter `metric` wajib diisi");
    }

    let rows = if let Some(src) = query.source.as_deref() {
        sqlx::query(
            "SELECT date, value, source FROM onchain_daily
             WHERE metric = ? AND source = ? AND value IS NOT NULL
             ORDER BY date ASC",
        )
        .bind(&metric)
        .bind(src)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT date, value, source FROM onchain_daily
             WHERE metric = ? AND value IS NOT NULL
             ORDER BY date ASC",
        )
        .bind(&metric)
        .fetch_all(pool)
        .await?
    };

    let mut points = Vec::with_capacity(rows.len());
    let mut source_used = String::new();
    for r in rows {
        let d: String = r.get("date");
        let v: f64 = r.get("value");
        let s: String = r.get("source");
        if source_used.is_empty() {
            source_used = s;
        }
        if let Some(d) = parse_date(&d) {
            if v.is_finite() {
                points.push(OnchainChartPoint {
                    date: d.format("%Y-%m-%d").to_string(),
                    value: v,
                });
            }
        }
    }

    // Tidak ada baris = metrik tidak dikenal untuk sumber ini. Balas error
    // eksplisit, bukan 200 dengan array kosong yang ambigu.
    if points.is_empty() {
        anyhow::bail!("metrik `{metric}` tidak tersedia di onchain_daily");
    }

    if let Some(days) = query.days {
        if let Some(last) = points.last() {
            if let Ok(last_date) = NaiveDate::parse_from_str(&last.date, "%Y-%m-%d") {
                let cutoff = (last_date - Duration::days(days.max(1))).format("%Y-%m-%d").to_string();
                points.retain(|p| p.date >= cutoff);
            }
        }
    }

    let first_date = points.first().map(|p| p.date.clone());
    let last_date = points.last().map(|p| p.date.clone());

    Ok(OnchainChartResponse {
        ok: true,
        metric,
        source: source_used,
        points,
        first_date,
        last_date,
    })
}

// ---------------------------------------------------------------------------
// Daftar metrik on-chain yang benar-benar tersedia
// ---------------------------------------------------------------------------

/// Deskripsi satu metrik on-chain yang ada isinya di DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnchainMetricInfo {
    pub metric: String,
    pub sources: Vec<String>,
    pub points: i64,
    pub first_date: Option<String>,
    pub last_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnchainMetricListResponse {
    pub ok: bool,
    pub metrics: Vec<OnchainMetricInfo>,
}

/// GET /api/v1/chart/onchain-metrics
///
/// Daftar metrik yang **benar-benar ada** di `onchain_daily`, dipakai UI untuk
/// menyusun dropdown. Dengan begitu UI tidak pernah menawarkan metrik yang
/// tidak dimiliki database (mis. `rhodl`/`puell_multiple` yang belum tersedia).
pub async fn list_onchain_metrics(pool: &SqlitePool) -> Result<OnchainMetricListResponse> {
    let rows = sqlx::query(
        "SELECT metric, source, COUNT(*) AS points, MIN(date) AS first_date, MAX(date) AS last_date
         FROM onchain_daily
         WHERE value IS NOT NULL
         GROUP BY metric, source
         ORDER BY metric ASC, source ASC",
    )
    .fetch_all(pool)
    .await?;

    let mut map: HashMap<String, OnchainMetricInfo> = HashMap::new();
    for r in rows {
        let metric: String = r.get("metric");
        let source: String = r.get("source");
        let points: i64 = r.get("points");
        let first: Option<String> = r.try_get("first_date").ok().flatten();
        let last: Option<String> = r.try_get("last_date").ok().flatten();

        let entry = map.entry(metric.clone()).or_insert(OnchainMetricInfo {
            metric,
            sources: Vec::new(),
            points: 0,
            first_date: None,
            last_date: None,
        });
        entry.points += points;
        if !entry.sources.iter().any(|s| s == &source) {
            entry.sources.push(source);
        }
        match (&entry.first_date, &first) {
            (Some(cur), Some(new)) if new >= cur => {}
            _ => entry.first_date = first,
        }
        match (&entry.last_date, &last) {
            (Some(cur), Some(new)) if new <= cur => {}
            _ => entry.last_date = last,
        }
    }

    let mut metrics: Vec<OnchainMetricInfo> = map.into_values().collect();
    metrics.sort_by(|a, b| a.metric.cmp(&b.metric));

    Ok(OnchainMetricListResponse { ok: true, metrics })
}

// ---------------------------------------------------------------------------
// Historis skor valuasi
// ---------------------------------------------------------------------------

/// Baca riwayat skor yang sudah dimaterialisasi di tabel `score_history`.
async fn read_materialized_scores(
    pool: &SqlitePool,
    limit: i64,
    step_days: i64,
) -> Result<Vec<ScoreHistoryPoint>> {
    let rows = match sqlx::query(
        "SELECT date, valuation_score, current_zone, spot_price
         FROM score_history
         WHERE step_days = ?
         ORDER BY date DESC LIMIT ?",
    )
    .bind(step_days)
    .bind(limit)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()), // tabel belum ada (fallback aman)
    };

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let d: String = r.get("date");
        let score: f64 = r.get("valuation_score");
        let zone: String = r.get("current_zone");
        let spot: Option<f64> = r.try_get("spot_price").ok().flatten();
        out.push(ScoreHistoryPoint {
            date: d,
            valuation_score: Some(score),
            current_zone: Some(zone),
            spot_price: spot,
        });
    }
    out.reverse();
    Ok(out)
}

/// Baca snapshot skor dari DB (ascending).
async fn read_score_snapshots(
    pool: &SqlitePool,
    engine_version: &str,
    limit: i64,
) -> Result<Vec<ScoreHistoryPoint>> {
    let rows = sqlx::query(
        "SELECT snapshot_date, valuation_score, current_zone
         FROM indicator_snapshots
         WHERE engine_version = ?
         ORDER BY snapshot_date DESC LIMIT ?",
    )
    .bind(engine_version)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let d: String = r.get("snapshot_date");
        let score: Option<f64> = r.try_get("valuation_score")?;
        let zone: String = r.get("current_zone");
        if let Some(date) = parse_date(&d) {
            out.push(ScoreHistoryPoint {
                date: date.format("%Y-%m-%d").to_string(),
                valuation_score: score,
                current_zone: Some(zone),
                spot_price: None,
            });
        }
    }
    out.reverse();
    Ok(out)
}

/// GET /api/v1/chart/score-history
///
/// Read-only. Mode `auto`: pakai snapshot DB (hasil evaluasi asli engine) bila
/// jumlahnya mencukupi; selain itu replay evaluasi per sampel `step_days`.
/// Replay **tidak** menulis ke `indicator_snapshots` — tabel itu milik
/// `engine::service::persist_snapshot` dengan payload lengkap, dan menimpanya
/// dengan payload ringkas akan merusak audit historis.
pub async fn get_score_history(
    pool: &SqlitePool,
    query: ScoreHistoryQuery,
    min_required_candles: i64,
) -> Result<ScoreHistoryResponse> {
    let engine_version = crate::engine::service::CURRENT_ENGINE_VERSION.to_string();
    let step_days = query.step_days.unwrap_or(30).clamp(7, 365);
    let limit = query.limit.unwrap_or(90).clamp(5, 400);
    let method = query.method.as_deref().unwrap_or("auto").to_ascii_lowercase();

    // Mode snapshots murni (cepat, tanpa hitung).
    if method == "snapshots" {
        let points = read_score_snapshots(pool, &engine_version, limit).await?;
        return Ok(ScoreHistoryResponse {
            ok: true,
            points,
            engine_version,
            method: "snapshots".to_string(),
            warnings: Vec::new(),
        });
    }

    // auto: pakai tabel materialized bila sudah ada, selain itu cek snapshots asli.
    if method == "auto" {
        let materialized = read_materialized_scores(pool, limit, step_days).await?;
        if materialized.len() >= 8 {
            return Ok(ScoreHistoryResponse {
                ok: true,
                points: materialized,
                engine_version,
                method: "materialized".to_string(),
                warnings: Vec::new(),
            });
        }
        let existing = read_score_snapshots(pool, &engine_version, limit).await?;
        if existing.len() >= 8 {
            return Ok(ScoreHistoryResponse {
                ok: true,
                points: existing,
                engine_version,
                method: "snapshots".to_string(),
                warnings: Vec::new(),
            });
        }
    }

    // Replay: sampling tanggal valid (harus punya candle) mundur dari candle terakhir.
    // Batas bawah = tanggal candle ke-`min_required_candles`, karena loader menolak
    // evaluasi yang cakupan candle-nya kurang (akan dilaporkan sebagai warning).
    let bounds = sqlx::query("SELECT MIN(date) AS f, MAX(date) AS l, COUNT(*) AS c FROM btc_daily_ohlcv")
        .fetch_one(pool)
        .await?;
    let first_str: Option<String> = bounds.try_get("f")?;
    let last_str: Option<String> = bounds.try_get("l")?;
    let candle_count: i64 = bounds.try_get("c")?;
    let (Some(first_str), Some(last_str)) = (first_str, last_str) else {
        anyhow::bail!("tidak ada candle untuk replay skor");
    };
    let first_date = parse_date(&first_str).context("parse tanggal awal")?;
    let last_date = parse_date(&last_str).context("parse tanggal akhir")?;

    // Tanggal awal yang sudah memenuhi syarat cakupan minimum.
    let skip = (min_required_candles - 1).max(0);
    let coverage_start = if candle_count > skip {
        let row = sqlx::query("SELECT date FROM btc_daily_ohlcv ORDER BY date ASC LIMIT 1 OFFSET ?")
            .bind(skip)
            .fetch_optional(pool)
            .await?;
        match row {
            Some(r) => {
                let s: String = r.get("date");
                parse_date(&s).unwrap_or(first_date)
            }
            None => last_date,
        }
    } else {
        last_date
    };
    let mut warnings = Vec::new();
    if coverage_start > first_date {
        warnings.push(format!(
            "replay dibatasi mulai {coverage_start} (butuh minimal {min_required_candles} candle; DB punya {candle_count})"
        ));
    }

    let mut sample: Vec<NaiveDate> = Vec::new();
    let mut cursor = last_date;
    while cursor >= coverage_start && sample.len() < limit as usize {
        sample.push(cursor);
        cursor = cursor - Duration::days(step_days);
    }
    sample.reverse();

    let mut points = Vec::with_capacity(sample.len());
    for as_of in sample {
        match crate::engine::loader::load_market_input(pool, Some(as_of), min_required_candles).await {
            Ok(loaded) => {
                let summary = crate::engine::evaluate_all_indicators(&loaded.input);
                let zone_str = format!("{:?}", summary.current_zone);
                let date_str = as_of.format("%Y-%m-%d").to_string();

                // Simpan ke score_history agar pemanggilan berikutnya instan.
                // Abaikan error bila skema belum termigrasi.
                let _ = sqlx::query(
                    "INSERT OR REPLACE INTO score_history (date, valuation_score, current_zone, spot_price, engine_version, step_days)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(&date_str)
                .bind(summary.valuation_score)
                .bind(&zone_str)
                .bind(summary.spot_price)
                .bind(&engine_version)
                .bind(step_days)
                .execute(pool)
                .await;

                points.push(ScoreHistoryPoint {
                    date: date_str,
                    valuation_score: Some(summary.valuation_score),
                    current_zone: Some(zone_str),
                    spot_price: Some(summary.spot_price),
                });
            }
            Err(e) => {
                // Tanggal tanpa cukup data tidak diisi angka palsu: dicatat transparan.
                warnings.push(format!("{as_of}: {e}"));
            }
        }
    }

    Ok(ScoreHistoryResponse {
        ok: true,
        points,
        engine_version,
        method: "replay".to_string(),
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Unit test helper (offline)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("tanggal valid")
    }

    #[test]
    fn compute_sma_nilai_referensi() {
        // [1,2,3,4,5] window 3 -> [2,3,4]
        let series: Vec<(NaiveDate, f64)> = (1..=5)
            .map(|i| (d(&format!("2024-01-0{i}")), i as f64))
            .collect();
        let ma = compute_sma(&series, 3);
        assert_eq!(ma.len(), 3);
        assert_eq!(ma[0].1, 2.0);
        assert_eq!(ma[1].1, 3.0);
        assert_eq!(ma[2].1, 4.0);
    }

    #[test]
    fn compute_sma_kurang_data_kosong() {
        let series = vec![(d("2024-01-01"), 1.0)];
        assert!(compute_sma(&series, 3).is_empty());
    }

    #[test]
    fn sample_backwards_interval_benar() {
        let series: Vec<(NaiveDate, f64)> = (0..30)
            .map(|i| (d("2024-01-01") + Duration::days(i), i as f64))
            .collect();
        let pts = sample_backwards(&series, 10, 10);
        assert_eq!(pts.len(), 3); // hari ke-29, 19, 9
        assert_eq!(pts[0].0, d("2024-01-10"));
        assert_eq!(pts[2].0, d("2024-01-30"));
    }

    #[test]
    fn parse_date_valid_dan_invalid() {
        assert!(parse_date("2026-11-26").is_some());
        assert!(parse_date("2026-13-99").is_none());
        assert!(parse_date("bukan-tanggal").is_none());
    }
}
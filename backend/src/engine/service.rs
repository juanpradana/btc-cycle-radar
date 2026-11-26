//! Service orchestration layer untuk Engine 19 Indikator.
//!
//! Menghubungkan DB loader (`src/engine/loader.rs`) dan core evaluation (`src/engine/mod.rs`),
//! serta mengurus persistensi idempotent snapshot ke tabel `indicator_snapshots`.
//!
//! Menghasilkan DTO respons API yang aman, transparan (full provenance & staleness),
//! dan bebas data mock/palsu.

use crate::engine::loader::{apply_staleness, load_market_input, FieldProvenance, LoaderError};
use crate::engine::{
    evaluate_all_indicators, IndicatorCategory, IndicatorId, IndicatorResult, IndicatorTier,
    MarketZone, TrendRegime, INDICATOR_SPECS,
};
use chrono::NaiveDate;
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::{BTreeMap, BTreeSet};

pub const CURRENT_ENGINE_VERSION: &str = "phase2-v1";

/// Ringkasan kualitas & provenance data untuk transparansi UI.
#[derive(Debug, Clone, Serialize)]
pub struct DataQuality {
    pub is_stale: bool,
    pub candles_total: i64,
    pub candles_used: usize,
    pub first_candle: NaiveDate,
    pub last_candle: NaiveDate,
    pub stale_fields: Vec<&'static str>,
    pub missing_fields: Vec<&'static str>,
    pub warnings: Vec<String>,
}

/// DTO respons untuk endpoint `GET /api/v1/valuation`.
#[derive(Debug, Clone, Serialize)]
pub struct ValuationResponse {
    pub ok: bool,
    pub engine_version: &'static str,
    pub as_of: NaiveDate,
    pub spot_price: f64,
    pub valuation_score: f64,
    pub trend_regime: TrendRegime,
    pub current_zone: MarketZone,
    pub triggered_buy_count: usize,
    pub triggered_top_count: usize,
    pub evaluated_indicators_count: usize,
    pub missing_indicators: Vec<IndicatorId>,
    pub action_guide: String,
    pub category_scores: Vec<(IndicatorCategory, Option<f64>)>,
    pub indicators: Vec<IndicatorResult>,
    pub data_quality: DataQuality,
    pub provenance: Vec<FieldProvenance>,
}

/// DTO satu entri spesifikasi untuk `GET /api/v1/indicators`.
#[derive(Debug, Clone, Serialize)]
pub struct IndicatorMetadataItem {
    pub id: IndicatorId,
    pub name: &'static str,
    pub rank: u8,
    pub tier: IndicatorTier,
    pub tier_weight: f64,
    pub category: IndicatorCategory,
    pub category_weight: f64,
    pub effective_weight: f64,
    pub latest_result: Option<IndicatorResult>,
}

/// DTO respons untuk `GET /api/v1/indicators`.
#[derive(Debug, Clone, Serialize)]
pub struct IndicatorsListResponse {
    pub ok: bool,
    pub engine_version: &'static str,
    pub as_of: Option<NaiveDate>,
    pub category_weights: BTreeMap<&'static str, f64>,
    pub indicators: Vec<IndicatorMetadataItem>,
}

/// Ambil evaluasi valuasi lengkap (dengan staleness dan persistensi snapshot).
pub async fn compute_valuation(
    pool: &SqlitePool,
    as_of: Option<NaiveDate>,
    min_required_candles: i64,
) -> Result<ValuationResponse, LoaderError> {
    let loaded = load_market_input(pool, as_of, min_required_candles).await?;

    // Evaluasi 19 indikator
    let mut summary = evaluate_all_indicators(&loaded.input);

    // Terapkan staleness berdasarkan field yang stale
    let stale_keys: BTreeSet<&'static str> = loaded.stale_keys();
    apply_staleness(&mut summary, &stale_keys);

    let response = ValuationResponse {
        ok: true,
        engine_version: CURRENT_ENGINE_VERSION,
        as_of: loaded.as_of,
        spot_price: summary.spot_price,
        valuation_score: summary.valuation_score,
        trend_regime: summary.trend_regime,
        current_zone: summary.current_zone,
        triggered_buy_count: summary.triggered_buy_count,
        triggered_top_count: summary.triggered_top_count,
        evaluated_indicators_count: summary.evaluated_indicators_count,
        missing_indicators: summary.missing_indicators.clone(),
        action_guide: summary.action_guide.clone(),
        category_scores: summary.category_scores.clone(),
        indicators: summary.indicator_results.clone(),
        data_quality: DataQuality {
            is_stale: loaded.is_stale,
            candles_total: loaded.candles_total,
            candles_used: loaded.candles_used,
            first_candle: loaded.first_candle,
            last_candle: loaded.last_candle,
            stale_fields: loaded.stale_fields.clone(),
            missing_fields: loaded.missing_fields.clone(),
            warnings: loaded.warnings.clone(),
        },
        provenance: loaded.fields.clone(),
    };

    // Simpan snapshot ke database secara idempotent (non-blocking terhadap respons, tapi log error jika gagal)
    if let Err(e) = persist_snapshot(pool, &response).await {
        eprintln!(
            "[engine::service] WARN: gagal menyimpan indicator_snapshot (as_of={}): {e}",
            response.as_of
        );
    }

    Ok(response)
}

/// Simpan snapshot ke database dengan `INSERT OR REPLACE` (idempotent).
pub async fn persist_snapshot(
    pool: &SqlitePool,
    resp: &ValuationResponse,
) -> Result<(), sqlx::Error> {
    let payload = match serde_json::to_string(resp) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let as_of_str = resp.as_of.format("%Y-%m-%d").to_string();
    let zone_str = format!("{:?}", resp.current_zone);

    sqlx::query(
        "INSERT INTO indicator_snapshots (
            snapshot_date, engine_version, current_zone, valuation_score, payload
         ) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(snapshot_date, engine_version) DO UPDATE SET
            current_zone = excluded.current_zone,
            valuation_score = excluded.valuation_score,
            payload = excluded.payload,
            created_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(as_of_str)
    .bind(CURRENT_ENGINE_VERSION)
    .bind(zone_str)
    .bind(resp.valuation_score)
    .bind(payload)
    .execute(pool)
    .await?;

    Ok(())
}

/// Ambil daftar metadata 19 indikator beserta bobot dan hasil evaluasi (jika as_of diberikan atau available).
pub async fn list_indicators(
    pool: &SqlitePool,
    as_of: Option<NaiveDate>,
    min_required_candles: i64,
) -> Result<IndicatorsListResponse, LoaderError> {
    // Tanpa data/cakupan memadai, endpoint tetap dapat menyajikan registry statis.
    // Namun jika caller meminta `as_of`, atau DB gagal selain kekurangan data,
    // teruskan error agar API tidak menyamarkan kegagalan sebagai hasil sukses.
    let latest_eval = match compute_valuation(pool, as_of, min_required_candles).await {
        Ok(result) => Some(result),
        Err(LoaderError::NoCandles | LoaderError::InsufficientCoverage { .. })
            if as_of.is_none() =>
        {
            None
        }
        Err(err) => return Err(err),
    };

    let mut category_weights = BTreeMap::new();
    for cat in IndicatorCategory::all() {
        category_weights.insert(cat.as_str(), cat.weight());
    }

    let mut indicators = Vec::with_capacity(INDICATOR_SPECS.len());
    for spec in INDICATOR_SPECS.iter() {
        let tier_weight = spec.tier.weight();
        let cat_weight = spec.category.weight();
        let effective_weight = tier_weight * cat_weight;
        let latest_result = latest_eval.as_ref().and_then(|e| {
            e.indicators.iter().find(|r| r.id == spec.id).cloned()
        });

        indicators.push(IndicatorMetadataItem {
            id: spec.id,
            name: spec.name,
            rank: spec.rank,
            tier: spec.tier,
            tier_weight,
            category: spec.category,
            category_weight: cat_weight,
            effective_weight,
            latest_result,
        });
    }

    Ok(IndicatorsListResponse {
        ok: true,
        engine_version: CURRENT_ENGINE_VERSION,
        as_of: latest_eval.as_ref().map(|e| e.as_of).or(as_of),
        category_weights,
        indicators,
    })
}

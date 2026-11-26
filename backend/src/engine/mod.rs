//! Engine 19 Indikator — registry, kalkulator per-indikator, normalisasi, komposit,
//! Trend Gate, dan klasifikasi zona pasar (MarketRegimeSummary).
//!
//! Arsitektur:
//! - Indikator terdaftar di `INDICATOR_SPECS` (Rank #1–#19, Tier S/A/B/C, 4 Kategori).
//! - Setiap indikator: fungsi `calculate(input: &MarketEngineInput) -> IndicatorResult`.
//! - Normalisasi: status string -> skor 0–100 via `IndicatorStatus::score_from_status`.
//! - Komposit: bobot efektif = bobot_tier * bobot_kategori, re-weighting proporsional
//!   saat indikator hilang/stale (missing atau status UNKNOWN).
//! - Trend Gate: 50W MA & 30W MA -> BullTrend / EarlyRecovery / BearMarket / Unknown.
//! - MarketRegimeSummary: agregat skor, count buy/top, zona, action guide.

use crate::math::{
    ahr999::{calculate as ahr999_calculate, evaluate_status as ahr999_evaluate},
    clock::{analyze_cycle, CycleClock},
    ma::{
        evaluate_200w_zone, evaluate_investor_tool,
        evaluate_ma_cross, evaluate_trend_gate_50w,
    },
    power_law,
    power_law::{
        calculate_at_date, evaluate_power_law_status,
        PowerLawModel,
    },
    quantile::{evaluate_quantile_status},
    rainbow::{calculate_bands, classify_price, RainbowBand},
};
pub mod loader;
pub mod service;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Kategori indikator (4 total) — bobot kategori: 30 / 20 / 40 / 10.
///
/// Serialisasi `snake_case` agar konsisten dengan `IndicatorId` dan
/// nama field provenance yang dipakai UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndicatorCategory {
    LongTermTechnical,    // 30% - 7 indikator
    MathematicalCycle,    // 20% - 4 indikator
    OnChainPsychology,    // 40% - 6 indikator
    IntermarketTiming,    // 10% - 2 indikator
}

impl IndicatorCategory {
    /// Id stabil untuk serialisasi JSON & kunci map pada API.
    pub fn as_str(self) -> &'static str {
        match self {
            IndicatorCategory::LongTermTechnical => "long_term_technical",
            IndicatorCategory::MathematicalCycle => "mathematical_cycle",
            IndicatorCategory::OnChainPsychology => "on_chain_psychology",
            IndicatorCategory::IntermarketTiming => "intermarket_timing",
        }
    }

    /// Empat kategori dalam urutan baku (dipakai agregasi komposit).
    pub fn all() -> [IndicatorCategory; 4] {
        [
            IndicatorCategory::LongTermTechnical,
            IndicatorCategory::MathematicalCycle,
            IndicatorCategory::OnChainPsychology,
            IndicatorCategory::IntermarketTiming,
        ]
    }
}

impl IndicatorCategory {
    /// Bobot kategori (berjumlah 1.0).
    pub fn weight(self) -> f64 {
        match self {
            IndicatorCategory::LongTermTechnical => 0.30,
            IndicatorCategory::MathematicalCycle => 0.20,
            IndicatorCategory::OnChainPsychology => 0.40,
            IndicatorCategory::IntermarketTiming => 0.10,
        }
    }
}

/// Tier kepercayaan indikator — bobot tier: S=4.0, A=3.0, B=2.0, C=1.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IndicatorTier {
    S, // 4.0
    A, // 3.0
    B, // 2.0
    C, // 1.0
}

impl IndicatorTier {
    pub fn weight(self) -> f64 {
        match self {
            IndicatorTier::S => 4.0,
            IndicatorTier::A => 3.0,
            IndicatorTier::B => 2.0,
            IndicatorTier::C => 1.0,
        }
    }
}

/// Identifier unik untuk 19 indikator (Rank #1–#19).
///
/// Serialisasi JSON memakai id `snake_case` huruf kecil yang stabil agar
/// konsisten dengan nama field provenance (`sma_200w`) dan `stale_fields`,
/// sehingga UI dapat melakukan join tanpa normalisasi nama.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(non_camel_case_types)]
pub enum IndicatorId {
    // LongTermTechnical (7) - 30%
    #[serde(rename = "ma_50w")]
    MA_50W,
    #[serde(rename = "ma_30w")]
    MA_30W,
    #[serde(rename = "ma_200w")]
    MA_200W,
    #[serde(rename = "ma_350w")]
    MA_350W,
    #[serde(rename = "ma_cross_10w_20w")]
    MA_CROSS_10W_20W,
    #[serde(rename = "investor_tool_2y")]
    INVESTOR_TOOL_2Y,
    #[serde(rename = "quantile_200w")]
    QUANTILE_200W,

    // MathematicalCycle (4) - 20%
    #[serde(rename = "power_law")]
    POWER_LAW,
    #[serde(rename = "ahr999")]
    AHR999,
    #[serde(rename = "rainbow_chart")]
    RAINBOW_CHART,
    #[serde(rename = "bitcoin_clock")]
    BITCOIN_CLOCK,

    // OnChainPsychology (6) - 40%
    #[serde(rename = "nupl")]
    NUPL,
    #[serde(rename = "supply_in_profit")]
    SUPPLY_IN_PROFIT,
    #[serde(rename = "lth_realized_price")]
    LTH_REALIZED_PRICE,
    #[serde(rename = "lth_realized_pl_ratio")]
    LTH_REALIZED_PL_RATIO,
    #[serde(rename = "sth_mvrv")]
    STH_MVRV,
    #[serde(rename = "true_market_mean_aviv")]
    TRUE_MARKET_MEAN_AVIV,

    // IntermarketTiming (2) - 10%
    #[serde(rename = "btc_gold_spd_ratio")]
    BTC_GOLD_SPD_RATIO,
    #[serde(rename = "btc_spx_ratio")]
    BTC_SPX_RATIO,
}

/// Status evaluasi indikator individual.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndicatorStatus {
    DeepBottom, // <=15
    Buyzone,    // 15-35
    Neutral,    // 35-65
    Overheated, // 65-85
    TopZone,    // >=85
}

impl IndicatorStatus {
    /// Konversi status -> skor numerik 0–100 (titik tengah rentang).
    pub fn score(self) -> f64 {
        match self {
            IndicatorStatus::DeepBottom => 7.5,  // mid of 0–15
            IndicatorStatus::Buyzone => 25.0,    // mid of 15–35
            IndicatorStatus::Neutral => 50.0,    // mid of 35–65
            IndicatorStatus::Overheated => 75.0, // mid of 65–85
            IndicatorStatus::TopZone => 92.5,    // mid of 85–100
        }
    }

    /// Dari skor 0–100 ke status.
    pub fn from_score(score: f64) -> Self {
        if !score.is_finite() {
            return IndicatorStatus::Neutral;
        }
        if score <= 15.0 {
            IndicatorStatus::DeepBottom
        } else if score <= 35.0 {
            IndicatorStatus::Buyzone
        } else if score < 65.0 {
            IndicatorStatus::Neutral
        } else if score < 85.0 {
            IndicatorStatus::Overheated
        } else {
            IndicatorStatus::TopZone
        }
    }

    pub fn is_buy(self) -> bool {
        matches!(self, IndicatorStatus::DeepBottom | IndicatorStatus::Buyzone)
    }

    pub fn is_top(self) -> bool {
        matches!(self, IndicatorStatus::Overheated | IndicatorStatus::TopZone)
    }
}

/// Hasil evaluasi satu indikator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndicatorResult {
    pub id: IndicatorId,
    pub name: &'static str,
    pub tier: IndicatorTier,
    pub category: IndicatorCategory,
    pub status: IndicatorStatus,
    pub score: f64,               // 0–100
    pub raw_value: Option<f64>,   // nilai mentah (mis. rasio, persentil, AHR999, dll)
    pub status_str: String,       // status string dari modul matematika
    pub is_stale: bool,           // true jika data di luar jendela freshness
    pub missing_reason: Option<&'static str>, // None jika ada data
}

/// Spesifikasi metadata indikator (registry).
#[derive(Debug, Clone)]
pub struct IndicatorSpec {
    pub id: IndicatorId,
    pub name: &'static str,
    pub rank: u8, // 1–19
    pub tier: IndicatorTier,
    pub category: IndicatorCategory,
}

/// Registry 19 indikator — urut Rank #1–#19.
pub const INDICATOR_SPECS: &[IndicatorSpec] = &[
    // LongTermTechnical (30%) — 7 indikator
    IndicatorSpec {
        id: IndicatorId::MA_50W,
        name: "50-Week MA (Trend Gate)",
        rank: 1,
        tier: IndicatorTier::S,
        category: IndicatorCategory::LongTermTechnical,
    },
    IndicatorSpec {
        id: IndicatorId::MA_30W,
        name: "30-Week MA",
        rank: 2,
        tier: IndicatorTier::A,
        category: IndicatorCategory::LongTermTechnical,
    },
    IndicatorSpec {
        id: IndicatorId::MA_200W,
        name: "200-Week MA",
        rank: 3,
        tier: IndicatorTier::S,
        category: IndicatorCategory::LongTermTechnical,
    },
    IndicatorSpec {
        id: IndicatorId::MA_350W,
        name: "350-Week MA",
        rank: 4,
        tier: IndicatorTier::B,
        category: IndicatorCategory::LongTermTechnical,
    },
    IndicatorSpec {
        id: IndicatorId::MA_CROSS_10W_20W,
        name: "10W/20W MA Cross",
        rank: 5,
        tier: IndicatorTier::B,
        category: IndicatorCategory::LongTermTechnical,
    },
    IndicatorSpec {
        id: IndicatorId::INVESTOR_TOOL_2Y,
        name: "Investor Tool (2Y SMA)",
        rank: 6,
        tier: IndicatorTier::A,
        category: IndicatorCategory::LongTermTechnical,
    },
    IndicatorSpec {
        id: IndicatorId::QUANTILE_200W,
        name: "200W MA Quantile",
        rank: 7,
        tier: IndicatorTier::B,
        category: IndicatorCategory::LongTermTechnical,
    },
    // MathematicalCycle (20%) — 4 indikator
    IndicatorSpec {
        id: IndicatorId::POWER_LAW,
        name: "Power Law Model",
        rank: 8,
        tier: IndicatorTier::S,
        category: IndicatorCategory::MathematicalCycle,
    },
    IndicatorSpec {
        id: IndicatorId::AHR999,
        name: "AHR999 Index",
        rank: 9,
        tier: IndicatorTier::A,
        category: IndicatorCategory::MathematicalCycle,
    },
    IndicatorSpec {
        id: IndicatorId::RAINBOW_CHART,
        name: "Bitcoin Rainbow Chart",
        rank: 10,
        tier: IndicatorTier::C,
        category: IndicatorCategory::MathematicalCycle,
    },
    IndicatorSpec {
        id: IndicatorId::BITCOIN_CLOCK,
        name: "Bitcoin Clock / Cycle Timing",
        rank: 11,
        tier: IndicatorTier::C,
        category: IndicatorCategory::MathematicalCycle,
    },
    // OnChainPsychology (40%) — 6 indikator
    IndicatorSpec {
        id: IndicatorId::NUPL,
        name: "NUPL (Net Unrealized P/L)",
        rank: 12,
        tier: IndicatorTier::S,
        category: IndicatorCategory::OnChainPsychology,
    },
    IndicatorSpec {
        id: IndicatorId::SUPPLY_IN_PROFIT,
        name: "Supply in Profit (%)",
        rank: 13,
        tier: IndicatorTier::A,
        category: IndicatorCategory::OnChainPsychology,
    },
    IndicatorSpec {
        id: IndicatorId::LTH_REALIZED_PRICE,
        name: "LTH Realized Price",
        rank: 14,
        tier: IndicatorTier::S,
        category: IndicatorCategory::OnChainPsychology,
    },
    IndicatorSpec {
        id: IndicatorId::LTH_REALIZED_PL_RATIO,
        name: "LTH Realized P/L Ratio",
        rank: 15,
        tier: IndicatorTier::B,
        category: IndicatorCategory::OnChainPsychology,
    },
    IndicatorSpec {
        id: IndicatorId::STH_MVRV,
        name: "STH MVRV",
        rank: 16,
        tier: IndicatorTier::B,
        category: IndicatorCategory::OnChainPsychology,
    },
    IndicatorSpec {
        id: IndicatorId::TRUE_MARKET_MEAN_AVIV,
        name: "True Market Mean / AVIV",
        rank: 17,
        tier: IndicatorTier::A,
        category: IndicatorCategory::OnChainPsychology,
    },
    // IntermarketTiming (10%) — 2 indikator
    IndicatorSpec {
        id: IndicatorId::BTC_GOLD_SPD_RATIO,
        name: "BTC/Gold Ratio Percentile",
        rank: 18,
        tier: IndicatorTier::C,
        category: IndicatorCategory::IntermarketTiming,
    },
    IndicatorSpec {
        id: IndicatorId::BTC_SPX_RATIO,
        name: "BTC/SPX Ratio Percentile",
        rank: 19,
        tier: IndicatorTier::C,
        category: IndicatorCategory::IntermarketTiming,
    },
];

/// Regime tren dari Trend Gate (50W & 30W MA).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrendRegime {
    BullTrend,      // Price > 50W MA && 30W MA > 50W MA
    EarlyRecovery,  // Price > 30W MA && Price < 50W MA
    BearMarket,     // Price < 30W MA && Price < 50W MA
    Unknown,        // data tidak cukup
}

/// Zona pasar komposit (MarketRegimeSummary.current_zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketZone {
    DeepCapitulation,       // skor <= 20 ATAU triggered_buy >= 5 dari Tier S/A
    BuyzoneAccumulation,    // skor <= 35
    EarlyRecovery,          // skor > 35 && TrendRegime::EarlyRecovery
    MidBullTrend,           // skor 40–70 && TrendRegime::BullTrend
    EuphoriaDistribution,   // skor > 75
    BearRiskOff,            // TrendRegime::BearMarket && skor > 35
    Unknown,                // data tidak cukup
}

/// Input data pasar untuk engine (semua opsional kecuali spot_price & date).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MarketEngineInput {
    pub date: NaiveDate,
    pub spot_price: f64,

    // MA teknikal (dihitung dari DB / dihitung client-side)
    pub sma_50w: Option<f64>,
    pub sma_30w: Option<f64>,
    pub sma_200w: Option<f64>,
    pub sma_350w: Option<f64>,
    pub sma_10w: Option<f64>,
    pub sma_20w: Option<f64>,
    pub sma_2y: Option<f64>,

    // Matematis
    pub ratio_200w_percentile: Option<f64>, // persentil historis Price/200WMA
    pub geomean_200d: Option<f64>,          // geometric mean 200 hari

    // On-chain (CoinMetrics / BGeometrics)
    pub nupl: Option<f64>,
    pub supply_in_profit_pct: Option<f64>,
    pub lth_realized_price: Option<f64>,
    pub lth_realized_pl_ratio: Option<f64>,
    pub sth_mvrv: Option<f64>,
    pub true_market_mean: Option<f64>,
    pub aviv: Option<f64>,

    // Intermarket
    pub gold_price: Option<f64>,
    pub spx_price: Option<f64>,
    pub btc_gold_ratio_percentile: Option<f64>,
    pub btc_spx_ratio_percentile: Option<f64>,
}

/// Ringkasan regime pasar untuk API/dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct MarketRegimeSummary {
    pub date: NaiveDate,
    pub spot_price: f64,
    pub valuation_score: f64,               // 0–100 komposit
    pub trend_regime: TrendRegime,
    pub current_zone: MarketZone,
    pub category_scores: Vec<(IndicatorCategory, Option<f64>)>, // skor per kategori
    pub indicator_results: Vec<IndicatorResult>, // detail 19 indikator
    pub evaluated_indicators_count: usize,
    pub missing_indicators: Vec<IndicatorId>,
    pub triggered_buy_count: usize, // jumlah DeepBottom + Buyzone
    pub triggered_top_count: usize, // jumlah Overheated + TopZone
    pub action_guide: String,       // narasi singkat untuk UI
}

/// Evaluasi semua 19 indikator dan hasilkan MarketRegimeSummary.
pub fn evaluate_all_indicators(input: &MarketEngineInput) -> MarketRegimeSummary {
    let has_any_market_metric = input.sma_50w.is_some()
        || input.sma_30w.is_some()
        || input.sma_200w.is_some()
        || input.sma_350w.is_some()
        || input.sma_10w.is_some()
        || input.sma_20w.is_some()
        || input.sma_2y.is_some()
        || input.ratio_200w_percentile.is_some()
        || input.geomean_200d.is_some()
        || input.nupl.is_some()
        || input.supply_in_profit_pct.is_some()
        || input.lth_realized_price.is_some()
        || input.lth_realized_pl_ratio.is_some()
        || input.sth_mvrv.is_some()
        || input.true_market_mean.is_some()
        || input.aviv.is_some()
        || input.gold_price.is_some()
        || input.spx_price.is_some()
        || input.btc_gold_ratio_percentile.is_some()
        || input.btc_spx_ratio_percentile.is_some();

    if !has_any_market_metric || !input.spot_price.is_finite() || input.spot_price <= 0.0 {
        let results: Vec<IndicatorResult> = INDICATOR_SPECS
            .iter()
            .map(|spec| missing_indicator(spec.id, "No market data available"))
            .collect();
        let missing: Vec<IndicatorId> = INDICATOR_SPECS.iter().map(|s| s.id).collect();
        let cat_scores = vec![
            (IndicatorCategory::LongTermTechnical, None),
            (IndicatorCategory::MathematicalCycle, None),
            (IndicatorCategory::OnChainPsychology, None),
            (IndicatorCategory::IntermarketTiming, None),
        ];
        return MarketRegimeSummary {
            date: input.date,
            spot_price: input.spot_price,
            valuation_score: 50.0,
            trend_regime: TrendRegime::Unknown,
            current_zone: MarketZone::Unknown,
            category_scores: cat_scores,
            indicator_results: results,
            evaluated_indicators_count: 0,
            missing_indicators: missing,
            triggered_buy_count: 0,
            triggered_top_count: 0,
            action_guide: "DATA TIDAK CUKUP: Tidak ada metrik pasar yang tersedia.".to_string(),
        };
    }

    // ==== 1. Evaluasi individu per indikator ====
    let mut results: Vec<IndicatorResult> = Vec::with_capacity(19);
    let mut missing: Vec<IndicatorId> = Vec::new();

    // Power law model (dibutuhkan beberapa indikator)
    let pl_model = calculate_at_date(input.date);

    // Rainbow bands
    let rainbow_bands = calculate_bands(power_law::days_since_genesis(input.date));

    // Cycle clock
    let cycle_clock = analyze_cycle(input.date);

    // --- LongTermTechnical ---
    // MA_50W: Trend Gate
    let r = evaluate_ma_50w(input);
    if r.is_none() {
        missing.push(IndicatorId::MA_50W);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::MA_50W, "sma_50w missing")));

    // MA_30W
    let r = evaluate_ma_30w(input);
    if r.is_none() {
        missing.push(IndicatorId::MA_30W);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::MA_30W, "sma_30w missing")));

    // MA_200W: 200W MA Zone
    let r = evaluate_ma_200w(input);
    if r.is_none() {
        missing.push(IndicatorId::MA_200W);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::MA_200W, "sma_200w missing")));

    // MA_350W
    let r = evaluate_ma_350w(input);
    if r.is_none() {
        missing.push(IndicatorId::MA_350W);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::MA_350W, "sma_350w missing")));

    // MA_CROSS_10W_20W
    let r = evaluate_ma_cross_10w_20w(input);
    if r.is_none() {
        missing.push(IndicatorId::MA_CROSS_10W_20W);
    }
    results.push(
        r.unwrap_or_else(|| missing_indicator(IndicatorId::MA_CROSS_10W_20W, "sma_10w/20w missing"))
    );

    // INVESTOR_TOOL_2Y
    let r = evaluate_investor_tool_2y(input);
    if r.is_none() {
        missing.push(IndicatorId::INVESTOR_TOOL_2Y);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::INVESTOR_TOOL_2Y, "sma_2y missing")));

    // QUANTILE_200W
    let r = evaluate_quantile_200w(input);
    if r.is_none() {
        missing.push(IndicatorId::QUANTILE_200W);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::QUANTILE_200W, "quantile missing")));

    // --- MathematicalCycle ---
    // POWER_LAW
    let r = evaluate_power_law(input, &pl_model);
    if r.is_none() {
        missing.push(IndicatorId::POWER_LAW);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::POWER_LAW, "power law calc failed")));

    // AHR999
    let r = evaluate_ahr999(input);
    if r.is_none() {
        missing.push(IndicatorId::AHR999);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::AHR999, "geomean_200d missing")));

    // RAINBOW_CHART
    let r = evaluate_rainbow(input, &rainbow_bands);
    if r.is_none() {
        missing.push(IndicatorId::RAINBOW_CHART);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::RAINBOW_CHART, "rainbow calc failed")));

    // BITCOIN_CLOCK
    let r = evaluate_bitcoin_clock(&cycle_clock);
    if r.is_none() {
        missing.push(IndicatorId::BITCOIN_CLOCK);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::BITCOIN_CLOCK, "cycle clock calc failed")));

    // --- OnChainPsychology ---
    // NUPL
    let r = evaluate_nupl(input);
    if r.is_none() {
        missing.push(IndicatorId::NUPL);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::NUPL, "nupl missing")));

    // SUPPLY_IN_PROFIT
    let r = evaluate_supply_in_profit(input);
    if r.is_none() {
        missing.push(IndicatorId::SUPPLY_IN_PROFIT);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::SUPPLY_IN_PROFIT, "supply in profit missing")));

    // LTH_REALIZED_PRICE
    let r = evaluate_lth_realized_price(input);
    if r.is_none() {
        missing.push(IndicatorId::LTH_REALIZED_PRICE);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::LTH_REALIZED_PRICE, "lth_realized_price missing")));

    // LTH_REALIZED_PL_RATIO
    let r = evaluate_lth_realized_pl_ratio(input);
    if r.is_none() {
        missing.push(IndicatorId::LTH_REALIZED_PL_RATIO);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::LTH_REALIZED_PL_RATIO, "lth_realized_pl_ratio missing")));

    // STH_MVRV
    let r = evaluate_sth_mvrv(input);
    if r.is_none() {
        missing.push(IndicatorId::STH_MVRV);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::STH_MVRV, "sth_mvrv missing")));

    // TRUE_MARKET_MEAN_AVIV (gabungan true_market_mean + aviv)
    let r = evaluate_true_market_mean_aviv(input);
    if r.is_none() {
        missing.push(IndicatorId::TRUE_MARKET_MEAN_AVIV);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::TRUE_MARKET_MEAN_AVIV, "true_market_mean/aviv missing")));

    // --- IntermarketTiming ---
    // BTC_GOLD_SPD_RATIO
    let r = evaluate_btc_gold_ratio(input);
    if r.is_none() {
        missing.push(IndicatorId::BTC_GOLD_SPD_RATIO);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::BTC_GOLD_SPD_RATIO, "btc/gold percentile missing")));

    // BTC_SPX_RATIO
    let r = evaluate_btc_spx_ratio(input);
    if r.is_none() {
        missing.push(IndicatorId::BTC_SPX_RATIO);
    }
    results.push(r.unwrap_or_else(|| missing_indicator(IndicatorId::BTC_SPX_RATIO, "btc/spx percentile missing")));

    // ==== 2. Trend Gate (50W & 30W MA) ====
    let trend_regime = compute_trend_regime(input);

    // ==== 3. Komposit skor anti-kolinearitas ====
    // Bobot efektif = tier_weight * category_weight.
    // Re-weighting: jika indikator missing/UNKNOWN, bobotnya didistribusikan ke indikator
    // tersedia di kategori yang sama, lalu ke kategori lain secara proporsional.
    let (valuation_score, category_scores) = compute_composite_score(&results);

    // ==== 4. Klasifikasi zona ====
    let triggered_buy = results.iter().filter(|r| r.status.is_buy()).count();
    let triggered_top = results.iter().filter(|r| r.status.is_top()).count();
    let current_zone = classify_market_zone(
        valuation_score,
        triggered_buy,
        triggered_top,
        trend_regime,
        &results,
    );

    let action_guide = build_action_guide(valuation_score, current_zone, trend_regime, triggered_buy, triggered_top);

    MarketRegimeSummary {
        date: input.date,
        spot_price: input.spot_price,
        valuation_score,
        trend_regime,
        current_zone,
        category_scores,
        indicator_results: results,
        evaluated_indicators_count: 19 - missing.len(),
        missing_indicators: missing,
        triggered_buy_count: triggered_buy,
        triggered_top_count: triggered_top,
        action_guide,
    }
}

// ============================================================
// Indikator evaluator functions (private)
// ============================================================

fn spec(id: IndicatorId) -> &'static IndicatorSpec {
    INDICATOR_SPECS.iter().find(|s| s.id == id).expect("spec exists")
}

fn missing_indicator(id: IndicatorId, reason: &'static str) -> IndicatorResult {
    let s = spec(id);
    IndicatorResult {
        id: s.id,
        name: s.name,
        tier: s.tier,
        category: s.category,
        status: IndicatorStatus::Neutral,
        score: 50.0,
        raw_value: None,
        status_str: "MISSING".to_string(),
        is_stale: true,
        missing_reason: Some(reason),
    }
}

fn make_result(
    id: IndicatorId,
    status: IndicatorStatus,
    raw_value: Option<f64>,
    status_str: &str,
    is_stale: bool,
    missing_reason: Option<&'static str>,
) -> IndicatorResult {
    let s = spec(id);
    IndicatorResult {
        id: s.id,
        name: s.name,
        tier: s.tier,
        category: s.category,
        status,
        score: status.score(),
        raw_value,
        status_str: status_str.to_string(),
        is_stale,
        missing_reason,
    }
}

// --- LongTermTechnical ---
fn evaluate_ma_50w(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let (price, sma) = (input.spot_price, input.sma_50w?);
    let status_str = evaluate_trend_gate_50w(price, sma);
    let ratio = price / sma;
    let status = match status_str {
        "BULL_TREND" => {
            // Price above 50W: degree of overbought
            if ratio > 1.3 {
                IndicatorStatus::TopZone
            } else if ratio > 1.15 {
                IndicatorStatus::Overheated
            } else {
                IndicatorStatus::Neutral  // slightly above = neutral
            }
        }
        "RISK_OFF" => IndicatorStatus::Neutral,   // below 50W = risk-off (bearish)
        _ => IndicatorStatus::Neutral,
    };
    Some(make_result(IndicatorId::MA_50W, status, Some(ratio), status_str, false, None))
}

fn evaluate_ma_30w(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let (price, sma) = (input.spot_price, input.sma_30w?);
    let ratio = price / sma;
    let status = if ratio > 1.3 {
        IndicatorStatus::TopZone
    } else if ratio > 1.15 {
        IndicatorStatus::Overheated
    } else if ratio > 1.0 {
        IndicatorStatus::Buyzone  // above 30W but not extreme = buyzone (recovery)
    } else {
        IndicatorStatus::Neutral  // below 30W = bearish
    };
    Some(make_result(
        IndicatorId::MA_30W,
        status,
        Some(ratio),
        if ratio > 1.3 { "WELL_ABOVE_30W" } else if ratio > 1.15 { "ABOVE_30W" } else if ratio > 1.0 { "ABOVE_30W" } else { "BELOW_30W" },
        false,
        None,
    ))
}

fn evaluate_ma_200w(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let (price, sma) = (input.spot_price, input.sma_200w?);
    let ratio = price / sma;
    let status_str = evaluate_200w_zone(price, sma);
    let status = match status_str {
        "DEEP_BOTTOM" => IndicatorStatus::DeepBottom,
        "BUYZONE" => IndicatorStatus::Buyzone,
        "NEUTRAL" => {
            // At extreme tops (ratio > 2.5), 200W MA is historically TopZone
            if ratio > 2.5 {
                IndicatorStatus::TopZone
            } else if ratio > 1.5 {
                IndicatorStatus::Overheated
            } else {
                IndicatorStatus::Neutral
            }
        }
        _ => IndicatorStatus::Neutral,
    };
    Some(make_result(IndicatorId::MA_200W, status, Some(ratio), status_str, false, None))
}

fn evaluate_ma_350w(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let (price, sma) = (input.spot_price, input.sma_350w?);
    let ratio = price / sma;
    // 350W = extreme bottom line; di bawah = very bullish
    let status = if price < sma {
        IndicatorStatus::DeepBottom
    } else if price <= sma * 1.1 {
        IndicatorStatus::Buyzone
    } else if ratio > 3.0 {
        IndicatorStatus::TopZone
    } else if ratio > 2.5 {
        IndicatorStatus::Overheated
    } else {
        IndicatorStatus::Neutral
    };
    Some(make_result(
        IndicatorId::MA_350W,
        status,
        Some(ratio),
        if price < sma {
            "DEEP_BOTTOM"
        } else if price <= sma * 1.1 {
            "BUYZONE"
        } else if ratio > 3.0 {
            "TOP_ZONE"
        } else {
            "NEUTRAL"
        },
        false,
        None,
    ))
}

fn evaluate_ma_cross_10w_20w(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let (fast, slow) = (input.sma_10w?, input.sma_20w?);
    let status_str = evaluate_ma_cross(fast, slow);
    let ratio = fast / slow;
    let status = match status_str {
        "GOLDEN_CROSS" => {
            // Golden cross: if spread > 10%, momentum is overheated/extended
            if ratio > 1.10 {
                IndicatorStatus::Overheated
            } else {
                IndicatorStatus::Neutral
            }
        }
        "DEATH_CROSS" => IndicatorStatus::Neutral,  // death cross = bearish momentum, BUKAN overheated
        _ => IndicatorStatus::Neutral,
    };
    Some(make_result(IndicatorId::MA_CROSS_10W_20W, status, Some(ratio), status_str, false, None))
}

fn evaluate_investor_tool_2y(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let (price, sma) = (input.spot_price, input.sma_2y?);
    let status_str = evaluate_investor_tool(price, sma);
    let status = match status_str {
        "BUYZONE" => IndicatorStatus::Buyzone,
        "OVERHEATED" => IndicatorStatus::Overheated,
        _ => IndicatorStatus::Neutral,
    };
    Some(make_result(IndicatorId::INVESTOR_TOOL_2Y, status, Some(price / sma), status_str, false, None))
}

fn evaluate_quantile_200w(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let pct = input.ratio_200w_percentile?;
    let status_str = evaluate_quantile_status(pct);
    let status = match status_str {
        "BUYZONE" => IndicatorStatus::Buyzone,
        "TOP_ZONE" => IndicatorStatus::TopZone,
        _ => IndicatorStatus::Neutral,
    };
    Some(make_result(IndicatorId::QUANTILE_200W, status, Some(pct), status_str, false, None))
}

// --- MathematicalCycle ---
fn evaluate_power_law(input: &MarketEngineInput, model: &PowerLawModel) -> Option<IndicatorResult> {
    let price = input.spot_price;
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    let status_str = evaluate_power_law_status(price, model);
    let status = match status_str {
        "DEEP_BOTTOM" => IndicatorStatus::DeepBottom,
        "BUYZONE" => IndicatorStatus::Buyzone,
        "TOP_ZONE" => IndicatorStatus::TopZone,
        _ => IndicatorStatus::Neutral,
    };
    Some(make_result(IndicatorId::POWER_LAW, status, Some(price / model.fair_price), status_str, false, None))
}

fn evaluate_ahr999(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let price = input.spot_price;
    let gm = input.geomean_200d?;
    if !price.is_finite() || price <= 0.0 || !gm.is_finite() || gm <= 0.0 {
        return None;
    }
    let days = power_law::days_since_genesis(input.date);
    let ahr = ahr999_calculate(price, gm, days);
    if !ahr.is_finite() || ahr <= 0.0 {
        return None;
    }
    let status_str = ahr999_evaluate(ahr);
    let status = match status_str {
        "DEEP_BOTTOM" => IndicatorStatus::DeepBottom,
        "DCA_ZONE" => IndicatorStatus::Buyzone,
        "WAIT_OVERHEATED" => IndicatorStatus::Overheated,
        _ => IndicatorStatus::Neutral,
    };
    Some(make_result(IndicatorId::AHR999, status, Some(ahr), status_str, false, None))
}

fn evaluate_rainbow(input: &MarketEngineInput, bands: &[RainbowBand]) -> Option<IndicatorResult> {
    let price = input.spot_price;
    if !price.is_finite() || price <= 0.0 || bands.is_empty() {
        return None;
    }
    let pos = classify_price(price, bands);
    let status = match pos.zone {
        "BUYZONE" => IndicatorStatus::Buyzone,
        "TOP_ZONE" => IndicatorStatus::TopZone,
        _ => IndicatorStatus::Neutral,
    };
    Some(make_result(IndicatorId::RAINBOW_CHART, status, Some(pos.percentile), pos.zone, false, None))
}

fn evaluate_bitcoin_clock(clock: &CycleClock) -> Option<IndicatorResult> {
    // Phase -> status mapping
    let (status, status_str) = match clock.phase {
        "EARLY_ACCUMULATION" | "MID_ACCUMULATION" => (IndicatorStatus::Buyzone, "ACCUMULATION"),
        "PRE_ATH_EXPANSION" | "POST_ATH_BULL_RUN" => (IndicatorStatus::Neutral, "BULL_RUN"),
        "LATE_BULL_TOPPING" => (IndicatorStatus::Overheated, "TOPPING"),
        "BEAR_MARKET" => (IndicatorStatus::Buyzone, "BEAR_MARKET"),  // bear = accumulation
        _ => (IndicatorStatus::Neutral, "UNKNOWN"),
    };
    Some(make_result(
        IndicatorId::BITCOIN_CLOCK,
        status,
        Some(clock.days_since_last_bottom as f64),
        status_str,
        false,
        None,
    ))
}

// --- OnChainPsychology ---
fn evaluate_nupl(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let v = input.nupl?;
    let status = if v < 0.0 {
        IndicatorStatus::DeepBottom
    } else if v < 0.25 {
        IndicatorStatus::Buyzone
    } else if v < 0.50 {
        IndicatorStatus::Neutral
    } else if v < 0.75 {
        IndicatorStatus::Overheated
    } else {
        IndicatorStatus::TopZone
    };
    Some(make_result(IndicatorId::NUPL, status, Some(v), status_str_nupl(v), false, None))
}

fn status_str_nupl(v: f64) -> &'static str {
    if v < 0.0 { "DEEP_BOTTOM" } else if v < 0.25 { "BUYZONE" } else if v < 0.50 { "NEUTRAL" } else if v < 0.75 { "OVERHEATED" } else { "TOP_ZONE" }
}

fn evaluate_supply_in_profit(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let v = input.supply_in_profit_pct?;
    let status = if v <= 53.0 {
        IndicatorStatus::Buyzone
    } else if v >= 95.0 {
        IndicatorStatus::TopZone
    } else {
        IndicatorStatus::Neutral
    };
    Some(make_result(
        IndicatorId::SUPPLY_IN_PROFIT,
        status,
        Some(v),
        if v <= 53.0 { "BUYZONE" } else if v >= 95.0 { "TOP_ZONE" } else { "NEUTRAL" },
        false,
        None,
    ))
}

fn evaluate_lth_realized_price(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let (price, lth_rp) = (input.spot_price, input.lth_realized_price?);
    let ratio = price / lth_rp;
    // LTH Realized Price = cost basis long-term holders. Price < LTH_RP = capitulation.
    let status = if price < lth_rp {
        IndicatorStatus::DeepBottom
    } else if ratio < 1.5 {
        IndicatorStatus::Buyzone
    } else if ratio > 5.0 {
        IndicatorStatus::TopZone
    } else if ratio > 3.0 {
        IndicatorStatus::Overheated
    } else {
        IndicatorStatus::Neutral
    };
    Some(make_result(IndicatorId::LTH_REALIZED_PRICE, status, Some(ratio), status_str_lth_rp(ratio), false, None))
}

fn status_str_lth_rp(ratio: f64) -> &'static str {
    if ratio < 1.0 { "DEEP_BOTTOM" } else if ratio < 1.5 { "BUYZONE" } else if ratio > 5.0 { "TOP_ZONE" } else if ratio > 3.0 { "OVERHEATED" } else { "NEUTRAL" }
}

fn evaluate_lth_realized_pl_ratio(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let v = input.lth_realized_pl_ratio?;
    // LTH Realized P/L > 10 = euforia, < 1 = belum profit
    let status = if v < 1.0 {
        IndicatorStatus::Buyzone
    } else if v > 20.0 {
        IndicatorStatus::TopZone
    } else if v > 10.0 {
        IndicatorStatus::Overheated
    } else {
        IndicatorStatus::Neutral
    };
    Some(make_result(
        IndicatorId::LTH_REALIZED_PL_RATIO,
        status,
        Some(v),
        if v < 1.0 { "BUYZONE" } else if v > 20.0 { "TOP_ZONE" } else if v > 10.0 { "OVERHEATED" } else { "NEUTRAL" },
        false,
        None,
    ))
}

fn evaluate_sth_mvrv(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let v = input.sth_mvrv?;
    // STH MVRV: < 0.85 = buyzone, > 1.3 = overheated
    let status = if v < 0.85 {
        IndicatorStatus::Buyzone
    } else if v > 1.5 {
        IndicatorStatus::TopZone
    } else if v > 1.3 {
        IndicatorStatus::Overheated
    } else {
        IndicatorStatus::Neutral
    };
    Some(make_result(
        IndicatorId::STH_MVRV,
        status,
        Some(v),
        if v < 0.85 { "BUYZONE" } else if v > 1.5 { "TOP_ZONE" } else if v > 1.3 { "OVERHEATED" } else { "NEUTRAL" },
        false,
        None,
    ))
}

fn evaluate_true_market_mean_aviv(input: &MarketEngineInput) -> Option<IndicatorResult> {
    // Gabungan True Market Mean (mean basis seluruh supply) dan AVIV (aktivitas holder).
    // Kalau keduanya ada, ambil rata-rata skor; kalau cuma satu, pakai yang ada.
    let mut scores = Vec::new();
    let mut parts: Vec<String> = Vec::new();

    if let Some(tmm) = input.true_market_mean {
        if tmm > 0.0 && tmm.is_finite() {
            let ratio = input.spot_price / tmm;
            // TMM = mean cost basis seluruh holder. Price < TMM = akumulasi.
            let status = if ratio < 1.0 {
                IndicatorStatus::Buyzone
            } else if ratio > 3.0 {
                IndicatorStatus::TopZone
            } else if ratio > 2.0 {
                IndicatorStatus::Overheated
            } else {
                IndicatorStatus::Neutral
            };
            scores.push(status.score());
            parts.push(format!("TMM_ratio={ratio:.2}"));
        }
    }

    if let Some(aviv) = input.aviv {
        if aviv.is_finite() {
            // AVIV: < 0.8 = akumulasi, > 2.5 = distribusi
            let status = if aviv < 0.8 {
                IndicatorStatus::Buyzone
            } else if aviv > 2.5 {
                IndicatorStatus::TopZone
            } else if aviv > 1.5 {
                IndicatorStatus::Overheated
            } else {
                IndicatorStatus::Neutral
            };
            scores.push(status.score());
            parts.push(format!("AVIV={aviv:.2}"));
        }
    }

    if scores.is_empty() {
        return None;
    }

    let avg_score = scores.iter().sum::<f64>() / scores.len() as f64;
    let status = IndicatorStatus::from_score(avg_score);
    let label = parts.join("; ");
    Some(make_result(
        IndicatorId::TRUE_MARKET_MEAN_AVIV,
        status,
        Some(avg_score),
        &label,
        false,
        None,
    ))
}

// --- IntermarketTiming ---
fn evaluate_btc_gold_ratio(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let pct = input.btc_gold_ratio_percentile?;
    let status = if pct < 20.0 {
        IndicatorStatus::Buyzone
    } else if pct > 80.0 {
        IndicatorStatus::TopZone
    } else {
        IndicatorStatus::Neutral
    };
    Some(make_result(
        IndicatorId::BTC_GOLD_SPD_RATIO,
        status,
        Some(pct),
        if pct < 20.0 { "BUYZONE" } else if pct > 80.0 { "TOP_ZONE" } else { "NEUTRAL" },
        false,
        None,
    ))
}

fn evaluate_btc_spx_ratio(input: &MarketEngineInput) -> Option<IndicatorResult> {
    let pct = input.btc_spx_ratio_percentile?;
    let status = if pct < 20.0 {
        IndicatorStatus::Buyzone
    } else if pct > 80.0 {
        IndicatorStatus::TopZone
    } else {
        IndicatorStatus::Neutral
    };
    Some(make_result(
        IndicatorId::BTC_SPX_RATIO,
        status,
        Some(pct),
        if pct < 20.0 { "BUYZONE" } else if pct > 80.0 { "TOP_ZONE" } else { "NEUTRAL" },
        false,
        None,
    ))
}

// ============================================================
// Trend Gate & Composite
// ============================================================

fn compute_trend_regime(input: &MarketEngineInput) -> TrendRegime {
    let (price, sma_50w, sma_30w) = (input.spot_price, input.sma_50w, input.sma_30w);

    match (sma_50w, sma_30w) {
        (Some(s50), Some(s30)) if s50 > 0.0 && s30 > 0.0 && price.is_finite() && price > 0.0 => {
            if price > s50 && s30 > s50 {
                TrendRegime::BullTrend
            } else if price > s30 && price < s50 {
                TrendRegime::EarlyRecovery
            } else {
                TrendRegime::BearMarket
            }
        }
        _ => TrendRegime::Unknown,
    }
}

fn compute_composite_score(
    results: &[IndicatorResult],
) -> (f64, Vec<(IndicatorCategory, Option<f64>)>) {
    // Step 1: hitung bobot nominal per indikator (tier * category)
    // Step 2: bagi per kategori
    // Step 3: untuk kategori yang punya indikator valid, hitung rata-rata tertimbang
    // Step 4: kategori tanpa indikator valid -> skor None, bobotnya di-redistribusikan

    use std::collections::HashMap;

    let mut cat_scores: HashMap<IndicatorCategory, Vec<(&IndicatorResult, f64)>> = HashMap::new();

    for r in results {
        if r.missing_reason.is_some() || r.status_str == "UNKNOWN" || r.status_str == "MISSING" {
            continue; // skip missing/unknown
        }
        let wt = r.tier.weight() * r.category.weight();
        cat_scores.entry(r.category).or_default().push((r, wt));
    }

    let mut cat_score_map: HashMap<IndicatorCategory, Option<f64>> = HashMap::new();

    for cat in [
        IndicatorCategory::LongTermTechnical,
        IndicatorCategory::MathematicalCycle,
        IndicatorCategory::OnChainPsychology,
        IndicatorCategory::IntermarketTiming,
    ] {
        if let Some(items) = cat_scores.get(&cat) {
            if !items.is_empty() {
                let wsum: f64 = items.iter().map(|(_, w)| *w).sum();
                let vsum: f64 = items.iter().map(|(r, w)| r.score * w).sum();
                let cat_score = vsum / wsum;
                cat_score_map.insert(cat, Some(cat_score));
            } else {
                cat_score_map.insert(cat, None);
            }
        } else {
            cat_score_map.insert(cat, None);
        }
    }

    // Normalisasi: jika ada kategori missing, bobot kategori tersebut di-redistribusikan
    // proporsional ke kategori yang punya data.
    let mut available_weight = 0.0;
    for (cat, score_opt) in &cat_score_map {
        if score_opt.is_some() {
            available_weight += cat.weight();
        }
    }

    // Skor akhir = sum( cat_score * cat_weight ) / available_weight
    let mut final_score = 0.0;
    if available_weight > 0.0 {
        for (cat, score_opt) in &cat_score_map {
            if let Some(s) = score_opt {
                final_score += s * cat.weight() / available_weight;
            }
        }
    } else {
        // tidak ada data sama sekali -> netral
        final_score = 50.0;
    }

    // Clamp
    final_score = final_score.clamp(0.0, 100.0);

    let category_scores = [
        IndicatorCategory::LongTermTechnical,
        IndicatorCategory::MathematicalCycle,
        IndicatorCategory::OnChainPsychology,
        IndicatorCategory::IntermarketTiming,
    ]
    .iter()
    .map(|c| (*c, cat_score_map.get(c).copied().flatten()))
    .collect();

    (final_score, category_scores)
}

fn classify_market_zone(
    score: f64,
    _triggered_buy: usize,
    _triggered_top: usize,
    trend: TrendRegime,
    results: &[IndicatorResult],
) -> MarketZone {
    // Tier S/A buyzone count
    let tier_sa_buy = results
        .iter()
        .filter(|r| (r.tier == IndicatorTier::S || r.tier == IndicatorTier::A) && r.status.is_buy())
        .count();

    if score <= 20.0 || tier_sa_buy >= 5 {
        MarketZone::DeepCapitulation
    } else if score <= 35.0 {
        MarketZone::BuyzoneAccumulation
    } else if score > 75.0 {
        MarketZone::EuphoriaDistribution
    } else if trend == TrendRegime::EarlyRecovery && score > 35.0 {
        MarketZone::EarlyRecovery
    } else if trend == TrendRegime::BullTrend && score >= 40.0 && score <= 70.0 {
        MarketZone::MidBullTrend
    } else if trend == TrendRegime::BearMarket && score > 35.0 {
        MarketZone::BearRiskOff
    } else {
        MarketZone::Unknown
    }
}

fn build_action_guide(
    score: f64,
    zone: MarketZone,
    trend: TrendRegime,
    buy_cnt: usize,
    top_cnt: usize,
) -> String {
    let trend_str = match trend {
        TrendRegime::BullTrend => "Bull Trend",
        TrendRegime::EarlyRecovery => "Early Recovery",
        TrendRegime::BearMarket => "Bear Market",
        TrendRegime::Unknown => "Unknown",
    };

    match zone {
        MarketZone::DeepCapitulation => {
            format!(
                "DEEP CAPITULATION (score {score:.0}): Harga di zona dasar sejarah. {buy_cnt}/19 indikator BUY. Akumulasi agresif DCA. Trend: {trend_str}."
            )
        }
        MarketZone::BuyzoneAccumulation => {
            format!(
                "BUYZONE ACCUMULATION (score {score:.0}): Zona beli historis. {buy_cnt}/19 indikator BUY. DCA bertahap. Trend: {trend_str}."
            )
        }
        MarketZone::EarlyRecovery => {
            format!(
                "EARLY RECOVERY (score {score:.0}): Pemulihan awal di atas 30W MA. Posisi core, tambah saat pullback. Trend: {trend_str}."
            )
        }
        MarketZone::MidBullTrend => {
            format!(
                "MID BULL TREND (score {score:.0}): Tren naik sehat. Tahan posisi, trailing stop. {top_cnt} indikator OVERHEATED. Trend: {trend_str}."
            )
        }
        MarketZone::EuphoriaDistribution => {
            format!(
                "EUPHORIA DISTRIBUTION (score {score:.0}): Zona jual/distribusi. {top_cnt}/19 indikator TOP. Realisasi profit bertahap. Trend: {trend_str}."
            )
        }
        MarketZone::BearRiskOff => {
            format!(
                "BEAR RISK-OFF (score {score:.0}): Bear rally tanpa validasi tren. {buy_cnt} BUY tapi trend BEAR. Kurangi eksposur, hindari FOMO. Trend: {trend_str}."
            )
        }
        MarketZone::Unknown => {
            format!(
                "DATA TIDAK CUKUP (score {score:.0}): Minimal metrik untuk evaluasi. Butuh data OHLCV + on-chain. Trend: {trend_str}."
            )
        }
    }
}

// ============================================================
// Unit Tests (inline untuk development cepat)
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn d(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn base() -> MarketEngineInput {
        MarketEngineInput { date: d(2026, 11, 24), spot_price: 60_000.0, ..Default::default() }
    }

    #[test]
    fn specs_count() {
        assert_eq!(INDICATOR_SPECS.len(), 19);
    }

    #[test]
    fn tier_weights() {
        assert_eq!(IndicatorTier::S.weight(), 4.0);
        assert_eq!(IndicatorTier::A.weight(), 3.0);
        assert_eq!(IndicatorTier::B.weight(), 2.0);
        assert_eq!(IndicatorTier::C.weight(), 1.0);
    }

    #[test]
    fn category_weights_sum_to_one() {
        let sum: f64 = [
            IndicatorCategory::LongTermTechnical,
            IndicatorCategory::MathematicalCycle,
            IndicatorCategory::OnChainPsychology,
            IndicatorCategory::IntermarketTiming,
        ]
        .iter()
        .map(|c| c.weight())
        .sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }

    #[test]
    fn status_thresholds() {
        assert_eq!(IndicatorStatus::from_score(0.0), IndicatorStatus::DeepBottom);
        assert_eq!(IndicatorStatus::from_score(15.0), IndicatorStatus::DeepBottom);
        assert_eq!(IndicatorStatus::from_score(15.1), IndicatorStatus::Buyzone);
        assert_eq!(IndicatorStatus::from_score(35.0), IndicatorStatus::Buyzone);
        assert_eq!(IndicatorStatus::from_score(35.1), IndicatorStatus::Neutral);
        assert_eq!(IndicatorStatus::from_score(64.9), IndicatorStatus::Neutral);
        assert_eq!(IndicatorStatus::from_score(65.0), IndicatorStatus::Overheated);
        assert_eq!(IndicatorStatus::from_score(84.9), IndicatorStatus::Overheated);
        assert_eq!(IndicatorStatus::from_score(85.0), IndicatorStatus::TopZone);
        assert_eq!(IndicatorStatus::from_score(100.0), IndicatorStatus::TopZone);
        assert_eq!(IndicatorStatus::from_score(f64::NAN), IndicatorStatus::Neutral);
    }

    #[test]
    fn trend_regime_logic() {
        let mut inp = base();
        inp.spot_price = 24_000.0;
        inp.sma_50w = Some(28_000.0);
        inp.sma_30w = Some(21_000.0);
        assert_eq!(compute_trend_regime(&inp), TrendRegime::EarlyRecovery);

        inp.spot_price = 35_000.0;
        inp.sma_50w = Some(30_000.0);
        inp.sma_30w = Some(32_000.0);
        assert_eq!(compute_trend_regime(&inp), TrendRegime::BullTrend);

        inp.spot_price = 20_000.0;
        inp.sma_50w = Some(30_000.0);
        inp.sma_30w = Some(25_000.0);
        assert_eq!(compute_trend_regime(&inp), TrendRegime::BearMarket);

        inp.sma_50w = None;
        assert_eq!(compute_trend_regime(&inp), TrendRegime::Unknown);
    }

    #[test]
    fn missing_indicators_handled() {
        let inp = base();
        let summary = evaluate_all_indicators(&inp);
        assert_eq!(summary.evaluated_indicators_count, 0);
        assert_eq!(summary.missing_indicators.len(), 19);
        assert!(summary.valuation_score.is_finite());
        assert_eq!(summary.trend_regime, TrendRegime::Unknown);
    }

    #[test]
    fn partial_onchain_missing_reweights() {
        let mut inp = base();
        inp.spot_price = 58_000.0;
        inp.sma_50w = Some(55_000.0);
        inp.sma_30w = Some(56_000.0);
        inp.sma_200w = Some(40_000.0);
        inp.sma_350w = Some(28_000.0);
        inp.sma_10w = Some(59_000.0);
        inp.sma_20w = Some(57_000.0);
        inp.sma_2y = Some(50_000.0);
        inp.ratio_200w_percentile = Some(50.0);
        inp.geomean_200d = Some(55_000.0);
        // on-chain all None
        inp.gold_price = Some(2600.0);
        inp.spx_price = Some(5800.0);
        inp.btc_gold_ratio_percentile = Some(50.0);
        inp.btc_spx_ratio_percentile = Some(50.0);

        let summary = evaluate_all_indicators(&inp);
        assert_eq!(summary.evaluated_indicators_count, 13);
        assert_eq!(summary.missing_indicators.len(), 6);
        assert!(summary.valuation_score.is_finite());
        // OnChainPsychology category score should be None
        let onchain = summary
            .category_scores
            .iter()
            .find(|(c, _)| *c == IndicatorCategory::OnChainPsychology)
            .unwrap();
        assert!(onchain.1.is_none());
    }
}
//! Integration & Unit tests untuk Engine 19 Indikator & Composite Valuation.
//!
//! TDD test suite yang memvalidasi:
//! 1. Metadata 19 indikator (Rank #1 - #19, Tier S/A/B/C, 4 kategori, bobot kategori 30/20/40/10).
//! 2. Ambang batas status indikator (DeepBottom/Buyzone/Neutral/Overheated/TopZone) dari skor 0-100.
//! 3. Skenario ekstrem bear (bottom Nov 2022) -> valuasi murah total.
//! 4. Skenario ekstrem bull (puncak siklus) -> valuasi mahal, mayoritas TopZone.
//! 5. Bear rally (harga masih di bawah 50W & 30W MA tapi bukan lagi murah) -> BearRiskOff.
//! 6. Trend Gate 50W/30W MA -> BullTrend / EarlyRecovery / BearMarket.
//! 7. Toleransi metrik on-chain hilang (None): re-weighting proporsional, tanpa panic, tanpa data palsu.

use btc_cycle_radar::engine::{
    evaluate_all_indicators, IndicatorCategory, IndicatorStatus, IndicatorTier, MarketEngineInput,
    MarketZone, TrendRegime, INDICATOR_SPECS,
};
use chrono::NaiveDate;

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).expect("tanggal konstan valid")
}

/// Input dengan seluruh metrik opsional = None (hanya harga spot yang ada).
fn base_input() -> MarketEngineInput {
    MarketEngineInput {
        date: d(2026, 11, 24),
        spot_price: 60_000.0,
        sma_50w: None,
        sma_30w: None,
        sma_200w: None,
        sma_350w: None,
        sma_10w: None,
        sma_20w: None,
        sma_2y: None,
        ratio_200w_percentile: None,
        geomean_200d: None,
        nupl: None,
        supply_in_profit_pct: None,
        lth_realized_price: None,
        lth_realized_pl_ratio: None,
        sth_mvrv: None,
        true_market_mean: None,
        aviv: None,
        gold_price: None,
        spx_price: None,
        btc_gold_ratio_percentile: None,
        btc_spx_ratio_percentile: None,
    }
}

#[test]
fn test_indicator_specs_count_and_uniqueness() {
    assert_eq!(INDICATOR_SPECS.len(), 19, "harus ada tepat 19 indikator");

    let mut ids = std::collections::HashSet::new();
    let mut ranks = std::collections::HashSet::new();
    for spec in INDICATOR_SPECS.iter() {
        assert!(ids.insert(spec.id), "ID indikator duplikat: {:?}", spec.id);
        assert!(ranks.insert(spec.rank), "Rank indikator duplikat: {}", spec.rank);
        assert!((1..=19).contains(&spec.rank), "Rank harus 1..=19, got {}", spec.rank);
    }
}

#[test]
fn test_tier_distribution() {
    let count = |t: IndicatorTier| INDICATOR_SPECS.iter().filter(|s| s.tier == t).count();
    // Tier S: 50W MA, 200W MA, Power Law, NUPL, LTH Realized Price
    assert_eq!(count(IndicatorTier::S), 5, "Tier S harus 5 indikator");
    // Tier A: 30W MA, AHR999, True Market Mean/AVIV, Investor Tool 2Y, Supply in Profit
    assert_eq!(count(IndicatorTier::A), 5, "Tier A harus 5 indikator");
    // Tier B: 350W MA, Quantile 200W, 10W/20W Cross, LTH Realized P/L, STH MVRV
    assert_eq!(count(IndicatorTier::B), 5, "Tier B harus 5 indikator");
    // Tier C: Bitcoin Clock, Rainbow Chart, BTC/Gold & BTC/SPX, Custom Reversion Score
    assert_eq!(count(IndicatorTier::C), 4, "Tier C harus 4 indikator");
}

#[test]
fn test_category_distribution_and_weights() {
    let count = |c: IndicatorCategory| INDICATOR_SPECS.iter().filter(|s| s.category == c).count();
    assert_eq!(count(IndicatorCategory::LongTermTechnical), 7);
    assert_eq!(count(IndicatorCategory::MathematicalCycle), 4);
    assert_eq!(count(IndicatorCategory::OnChainPsychology), 6);
    assert_eq!(count(IndicatorCategory::IntermarketTiming), 2);

    // Bobot kategori penelitian: 30 / 20 / 40 / 10
    let total: f64 = [
        IndicatorCategory::LongTermTechnical,
        IndicatorCategory::MathematicalCycle,
        IndicatorCategory::OnChainPsychology,
        IndicatorCategory::IntermarketTiming,
    ]
    .iter()
    .map(|c| c.weight())
    .sum();
    assert!((total - 1.0).abs() < 1e-9, "bobot kategori harus 1.0, got {total}");
    assert!((IndicatorCategory::OnChainPsychology.weight() - 0.40).abs() < 1e-9);
    assert!((IndicatorCategory::IntermarketTiming.weight() - 0.10).abs() < 1e-9);
}

#[test]
fn test_status_thresholds_from_score() {
    // Ambang: <=15 DeepBottom, <=35 Buyzone, <65 Neutral, <85 Overheated, >=85 TopZone
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

    // Skor non-finite tidak boleh panic dan tidak boleh diklaim murah.
    assert_eq!(IndicatorStatus::from_score(f64::NAN), IndicatorStatus::Neutral);

    assert!(IndicatorStatus::DeepBottom.is_buy());
    assert!(IndicatorStatus::Buyzone.is_buy());
    assert!(!IndicatorStatus::Neutral.is_buy());
    assert!(IndicatorStatus::Overheated.is_top());
    assert!(IndicatorStatus::TopZone.is_top());
    assert!(!IndicatorStatus::Neutral.is_top());
}

fn bear_bottom_input() -> MarketEngineInput {
    // Bottom siklus Nov 2022: harga $16k jauh di bawah seluruh MA panjang,
    // NUPL negatif (capitulation), belum ada satu pun metrik on-chain di zona mahal.
    MarketEngineInput {
        date: d(2022, 11, 21),
        spot_price: 16_000.0,
        sma_50w: Some(35_000.0),
        sma_30w: Some(25_000.0),
        sma_200w: Some(24_000.0),
        sma_350w: Some(17_000.0),
        sma_10w: Some(19_000.0),
        sma_20w: Some(21_000.0),
        sma_2y: Some(38_000.0),
        ratio_200w_percentile: Some(2.5),
        geomean_200d: Some(20_000.0),
        nupl: Some(-0.18),
        supply_in_profit_pct: Some(48.0),
        lth_realized_price: Some(18_500.0),
        lth_realized_pl_ratio: Some(0.4),
        sth_mvrv: Some(0.78),
        true_market_mean: Some(22_000.0),
        aviv: Some(0.65),
        gold_price: Some(1_750.0),
        spx_price: Some(3_950.0),
        btc_gold_ratio_percentile: Some(15.0),
        btc_spx_ratio_percentile: Some(12.0),
    }
}

#[test]
fn test_extreme_bear_market_scenario_evaluates_deep_capitulation() {
    let summary = evaluate_all_indicators(&bear_bottom_input());

    assert_eq!(summary.evaluated_indicators_count, 19, "semua metrik tersedia");
    assert!(
        summary.valuation_score < 25.0,
        "valuasi di bottom siklus harus < 25, got {}",
        summary.valuation_score
    );
    assert!(
        summary.triggered_buy_count >= 10,
        "di bottom minimal 10 indikator Buyzone/DeepBottom, got {}",
        summary.triggered_buy_count
    );
    assert_eq!(summary.triggered_top_count, 0, "tidak boleh ada indikator zona atas");
    assert_eq!(summary.trend_regime, TrendRegime::BearMarket);
    assert_eq!(summary.current_zone, MarketZone::DeepCapitulation);
    assert!(!summary.action_guide.is_empty(), "action_guide wajib terisi");
}

#[test]
fn test_extreme_top_euphoria_scenario() {
    // Puncak siklus 2025-10-06 (anchor ATH proyek): seluruh MA panjang di bawah harga,
    // on-chain di zona euforia (NUPL 0.78, supply in profit 98.5%).
    //
    // Catatan kalibrasi: skor komposit mendarat ~70-75, bukan >85, karena dua indikator
    // matematis (Power Law & Rainbow Chart) memang menempatkan harga 2025 *di bawah*
    // kanal tren-nya. Ketidaksepakatan antar-indikator adalah perilaku yang diinginkan
    // dari komposit; karena itu ambang EuphoriaDistribution = 70.
    let input = MarketEngineInput {
        date: d(2025, 10, 6),
        spot_price: 105_000.0,
        sma_50w: Some(72_000.0),
        sma_30w: Some(85_000.0),
        sma_200w: Some(38_000.0),
        sma_350w: Some(28_000.0),
        sma_10w: Some(98_000.0),
        sma_20w: Some(88_000.0),
        sma_2y: Some(55_000.0),
        ratio_200w_percentile: Some(96.0),
        geomean_200d: Some(82_000.0),
        nupl: Some(0.78),
        supply_in_profit_pct: Some(98.5),
        lth_realized_price: Some(32_000.0),
        lth_realized_pl_ratio: Some(28.0),
        sth_mvrv: Some(1.45),
        true_market_mean: Some(58_000.0),
        aviv: Some(2.80),
        gold_price: Some(2_650.0),
        spx_price: Some(5_800.0),
        btc_gold_ratio_percentile: Some(92.0),
        btc_spx_ratio_percentile: Some(94.0),
    };

    let summary = evaluate_all_indicators(&input);

    assert!(summary.valuation_score > 70.0,
        "valuasi di puncak siklus harus > 70, got {}", summary.valuation_score);
    assert!(
        summary.triggered_top_count >= 8,
        "di puncak minimal 8 indikator Overheated/TopZone, got {}",
        summary.triggered_top_count
    );

    // Kategori on-chain (bobot 40%) di zona euforia kuat (>= 80).
    // Catatan: LTH-RP ratio 3.28x, STH-MVRV 1.45, TMM 1.81, AVIV 2.8 → empat indikator Overheated (75),
    // dua TopZone (92.5) → rata-rata 83.75. Ambang 80 mencerminkan overheat nyata tanpa memaksa TopZone
    // pada metrik yang secara historis belum memasuki distribusi ekstrem (LTH-RP < 5x, AVIV < 3.0).
    let onchain = summary
        .category_scores
        .iter()
        .find(|(c, _)| *c == IndicatorCategory::OnChainPsychology)
        .and_then(|(_, s)| *s)
        .expect("kategori on-chain tersedia");
    assert!(onchain >= 80.0, "skor kategori on-chain di puncak harus >= 80, got {onchain}");

    assert_eq!(summary.trend_regime, TrendRegime::BullTrend);
    assert_eq!(summary.current_zone, MarketZone::EuphoriaDistribution);
}

#[test]
fn test_bear_rally_is_risk_off_not_buyzone() {
    // Bear rally: harga sudah naik jauh dari dasar tapi masih di bawah 50W MA dan
    // 30W MA, sementara valuasi on-chain sudah netral (bukan lagi murah).
    // Ini wajib diklasifikasi Risk-Off, bukan Buyzone.
    let input = MarketEngineInput {
        date: d(2023, 8, 1),
        spot_price: 50_000.0,
        sma_50w: Some(55_000.0),
        sma_30w: Some(52_000.0),
        sma_200w: Some(30_000.0),
        sma_350w: Some(22_000.0),
        sma_10w: Some(52_000.0),
        sma_20w: Some(49_500.0),
        sma_2y: Some(38_000.0),
        ratio_200w_percentile: Some(62.0),
        geomean_200d: Some(45_000.0),
        nupl: Some(0.42),
        supply_in_profit_pct: Some(82.0),
        lth_realized_price: Some(28_000.0),
        lth_realized_pl_ratio: Some(3.0),
        sth_mvrv: Some(1.12),
        true_market_mean: Some(40_000.0),
        aviv: Some(1.35),
        gold_price: Some(1_950.0),
        spx_price: Some(4_580.0),
        btc_gold_ratio_percentile: Some(60.0),
        btc_spx_ratio_percentile: Some(60.0),
    };

    let summary = evaluate_all_indicators(&input);

    assert_eq!(summary.trend_regime, TrendRegime::BearMarket);
    assert!(
        summary.valuation_score >= 38.0,
        "valuasi bear rally tidak boleh dianggap akumulasi (<38), got {}",
        summary.valuation_score
    );
    assert_eq!(summary.current_zone, MarketZone::BearRiskOff);
}

#[test]
fn test_recovery_regime_detection() {
    // Recovery: harga di atas 30W MA tetapi masih di bawah 50W MA.
    let mut input = base_input();
    input.date = d(2023, 3, 1);
    input.spot_price = 24_000.0;
    input.sma_50w = Some(28_000.0);
    input.sma_30w = Some(21_000.0);

    let summary = evaluate_all_indicators(&input);
    assert_eq!(summary.trend_regime, TrendRegime::EarlyRecovery);
}

#[test]
fn test_resilient_to_missing_onchain_indicators() {
    // Enam metrik on-chain hilang (None). Sistem wajib tetap menghasilkan skor finite
    // dan mencatat indikator yang hilang - tanpa panic dan tanpa mengarang angka.
    let mut input = base_input();
    input.sma_50w = Some(58_000.0);
    input.sma_30w = Some(59_000.0);
    input.sma_200w = Some(42_000.0);
    input.sma_350w = Some(30_000.0);
    input.sma_10w = Some(61_000.0);
    input.sma_20w = Some(60_000.0);
    input.sma_2y = Some(52_000.0);
    input.ratio_200w_percentile = Some(55.0);
    input.geomean_200d = Some(59_000.0);
    input.gold_price = Some(2_650.0);
    input.spx_price = Some(5_900.0);
    input.btc_gold_ratio_percentile = Some(50.0);
    input.btc_spx_ratio_percentile = Some(50.0);

    let summary = evaluate_all_indicators(&input);

    assert!(summary.valuation_score.is_finite());
    assert!((0.0..=100.0).contains(&summary.valuation_score));
    assert_eq!(
        summary.evaluated_indicators_count, 13,
        "7 teknikal + 4 matematis + 2 intermarket"
    );
    assert_eq!(summary.missing_indicators.len(), 6, "6 indikator on-chain hilang");

    // Kategori on-chain tanpa satu pun indikator tersedia -> skor None, bobotnya
    // di-redistribusi proporsional ke kategori lain (total bobot efektif tetap 1.0).
    let onchain = summary
        .category_scores
        .iter()
        .find(|(c, _)| *c == IndicatorCategory::OnChainPsychology)
        .expect("entri kategori on-chain selalu ada");
    assert!(
        onchain.1.is_none(),
        "kategori tanpa data harus None, bukan 0 (0 berarti sangat murah)"
    );
}

#[test]
fn test_all_metrics_missing_does_not_panic_or_fabricate() {
    // Tanpa satu pun metrik: skor harus finite, semua indikator tercatat hilang,
    // dan tidak ada kesimpulan valuasi yang diklaim.
    let summary = evaluate_all_indicators(&base_input());

    assert!(summary.valuation_score.is_finite());
    assert_eq!(summary.evaluated_indicators_count, 0);
    assert_eq!(summary.missing_indicators.len(), 19);
    assert_eq!(summary.triggered_buy_count, 0);
    assert_eq!(summary.triggered_top_count, 0);
    assert_eq!(summary.trend_regime, TrendRegime::Unknown);
}

#[test]
fn test_zero_spot_price_is_rejected_not_extrapolated() {
    // Harga spot tidak valid: engine harus menolak mengevaluasi indikator berbasis
    // harga, bukan menghasilkan skor dari pembagian nol.
    let mut input = base_input();
    input.spot_price = 0.0;
    input.sma_50w = Some(50_000.0);
    input.sma_200w = Some(30_000.0);
    input.nupl = Some(0.5);

    let summary = evaluate_all_indicators(&input);
    assert!(summary.valuation_score.is_finite());
    assert!(summary.evaluated_indicators_count < 19);
}

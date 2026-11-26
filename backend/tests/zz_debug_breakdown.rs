//! Debug harness sementara — mencetak rincian per-indikator untuk kalibrasi skenario.
//! Jalankan: cargo test --test zz_debug_breakdown -- --nocapture

use btc_cycle_radar::engine::{evaluate_all_indicators, MarketEngineInput, MarketRegimeSummary};
use chrono::NaiveDate;

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).expect("tanggal valid")
}

fn dump(label: &str, s: &MarketRegimeSummary) {
    println!("\n===== {label} =====");
    println!(
        "score={:.2} trend={:?} zone={:?} eval={} missing={} buy={} top={}",
        s.valuation_score,
        s.trend_regime,
        s.current_zone,
        s.evaluated_indicators_count,
        s.missing_indicators.len(),
        s.triggered_buy_count,
        s.triggered_top_count
    );
    for (c, v) in &s.category_scores {
        println!("  kategori {:?} = {:?}", c, v);
    }
    for r in &s.indicator_results {
        println!(
            "  {:?} tier={:?} status={:?} score={:.1} raw={:?} str={} miss={:?}",
            r.id, r.tier, r.status, r.score, r.raw_value, r.status_str, r.missing_reason
        );
    }
}

#[test]
fn dbg_peak() {
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
    dump("PEAK 2025-10-06 @105k", &evaluate_all_indicators(&input));
}

#[test]
fn dbg_bear_rally() {
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
    dump("BEAR RALLY 2023-08-01 @50k (sma_30w=52k)", &evaluate_all_indicators(&input));
}

#[test]
fn dbg_nov2022_bottom() {
    let input = MarketEngineInput {
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
        btc_gold_ratio_percentile: Some(12.0),
        btc_spx_ratio_percentile: Some(10.0),
    };
    dump("BOTTOM 2022-11-21 @16k", &evaluate_all_indicators(&input));
}

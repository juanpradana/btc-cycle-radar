//! Unit test untuk Phase 2 Math Engine.
//! Memvalidasi seluruh kalkulasi finansial & matematis terhadap nilai referensi.
use btc_cycle_radar::math::{ahr999, clock, ma, power_law, quantile, rainbow};
use chrono::NaiveDate;

#[test]
fn test_power_law_santostasi_reference_values() {
    // Tanggal Genesis BTC: 2009-01-03
    // Uji hari ke-d tertentu:
    // d = 5000 (sekitar 2022-09-11)
    // Fair value formula: P_fair = 10^(-17.016) * d^5.845
    // log10(P_fair) = -17.016 + 5.845 * log10(5000)
    // 5000^5.845: log10(5000) ≈ 3.69897 -> 5.845 * 3.69897 ≈ 21.61748
    // log10(P_fair) ≈ 4.60148 -> P_fair ≈ 39,946 USD
    let d = 5000;
    let model = power_law::calculate_at_day(d);

    assert!(model.fair_price > 35000.0 && model.fair_price < 45000.0, "fair_price: {}", model.fair_price);
    assert!((model.support_price - (0.42 * model.fair_price)).abs() < 1e-4);
    assert!(model.top_price > model.fair_price * 3.0);

    // Test helper tanggal
    let date_genesis = NaiveDate::from_ymd_opt(2009, 1, 3).unwrap();
    assert_eq!(power_law::days_since_genesis(date_genesis), 0);

    let date_halving_4 = NaiveDate::from_ymd_opt(2024, 4, 19).unwrap();
    let days_h4 = power_law::days_since_genesis(date_halving_4);
    assert!(days_h4 > 5500 && days_h4 < 5600);

    // Evaluasi status harga
    let status_bottom = power_law::evaluate_power_law_status(10000.0, &model);
    assert_eq!(status_bottom, "DEEP_BOTTOM");

    let status_buyzone = power_law::evaluate_power_law_status(model.support_price * 1.05, &model);
    assert_eq!(status_buyzone, "BUYZONE");

    let status_neutral = power_law::evaluate_power_law_status(model.fair_price, &model);
    assert_eq!(status_neutral, "NEUTRAL");

    let status_top = power_law::evaluate_power_law_status(model.top_price * 1.1, &model);
    assert_eq!(status_top, "TOP_ZONE");
}

#[test]
fn test_ahr999_reference_values() {
    let date = NaiveDate::from_ymd_opt(2022, 11, 21).unwrap(); // Bottom 2022 ~$15.8k
    let d = power_law::days_since_genesis(date);
    let spot_price = 15800.0;
    let geomean_200d = 20000.0; // Perkiraan geometric mean saat bear market 2022

    let val = ahr999::calculate(spot_price, geomean_200d, d);
    assert!(val > 0.0 && val < 2.0, "ahr999 value: {}", val);

    // Di bottom ekstrem 2022, AHR999 berada di area buyzone / deep bottom (< 0.45)
    let status = ahr999::evaluate_status(0.35);
    assert_eq!(status, "DEEP_BOTTOM");

    let status_dca = ahr999::evaluate_status(0.8);
    assert_eq!(status_dca, "DCA_ZONE");

    let status_wait = ahr999::evaluate_status(1.5);
    assert_eq!(status_wait, "WAIT_OVERHEATED");
}

#[test]
fn test_geometric_mean_calculation() {
    // GeoMean dari [2, 8] = sqrt(16) = 4
    let values = vec![2.0, 8.0];
    let gm = ahr999::geometric_mean(&values).expect("geomean");
    assert!((gm - 4.0).abs() < 1e-6);

    // Empty atau nilai <= 0 mengembalikan None
    assert!(ahr999::geometric_mean(&[]).is_none());
    assert!(ahr999::geometric_mean(&[10.0, -5.0]).is_none());
}

#[test]
fn test_moving_averages_calculation() {
    let prices = vec![10.0, 20.0, 30.0, 40.0, 50.0];
    let sma3 = ma::simple_moving_average(&prices, 3).expect("sma3");
    assert_eq!(sma3, 40.0); // (30 + 40 + 50) / 3

    assert!(ma::simple_moving_average(&prices, 6).is_none()); // Kurang data

    // 10W (~70D) vs 20W (~140D) Cross
    assert_eq!(ma::evaluate_ma_cross(100.0, 90.0), "GOLDEN_CROSS");
    assert_eq!(ma::evaluate_ma_cross(85.0, 90.0), "DEATH_CROSS");
    assert_eq!(ma::evaluate_ma_cross(90.0, 90.0), "NEUTRAL");

    // Investor Tool (2Y SMA = 730 hari)
    let spot = 25000.0;
    let sma_730 = 30000.0;
    assert_eq!(ma::evaluate_investor_tool(spot, sma_730), "BUYZONE"); // < 2Y SMA
    assert_eq!(ma::evaluate_investor_tool(160000.0, 30000.0), "OVERHEATED"); // > 5 * 2Y SMA
    assert_eq!(ma::evaluate_investor_tool(50000.0, 30000.0), "NEUTRAL");
}

#[test]
fn test_rainbow_chart_bands() {
    let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
    let d = power_law::days_since_genesis(date);
    let bands = rainbow::calculate_bands(d);

    assert_eq!(bands.len(), 8);
    // Band harus terurut naik
    for i in 1..bands.len() {
        assert!(bands[i].price > bands[i - 1].price, "Bands must be ascending");
    }

    let low_status = rainbow::classify_price(bands[0].price * 0.9, &bands);
    assert_eq!(low_status.zone, "BUYZONE");

    let mid_status = rainbow::classify_price((bands[3].price + bands[4].price) / 2.0, &bands);
    assert_eq!(mid_status.zone, "NEUTRAL");

    let high_status = rainbow::classify_price(bands[7].price * 1.2, &bands);
    assert_eq!(high_status.zone, "TOP_ZONE");
}

#[test]
fn test_quantile_regression_percentile() {
    // Seri rasio sintetis: 1.0 s.d. 10.0
    let history: Vec<f64> = (1..=10).map(|x| x as f64).collect();
    // Nilai 1.0 adalah yang terendah -> persentil ~10%
    let p1 = quantile::calculate_percentile(1.0, &history).expect("percentile");
    assert!(p1 <= 10.0);

    // Nilai 10.0 adalah yang tertinggi -> persentil 100%
    let p10 = quantile::calculate_percentile(10.0, &history).expect("percentile");
    assert_eq!(p10, 100.0);

    // Nilai 5.5 ada di tengah -> persentil ~50%
    let p5 = quantile::calculate_percentile(5.5, &history).expect("percentile");
    assert!((p5 - 50.0).abs() <= 5.0);

    assert_eq!(quantile::evaluate_quantile_status(8.0), "BUYZONE"); // < 10
    assert_eq!(quantile::evaluate_quantile_status(95.0), "TOP_ZONE"); // > 90
    assert_eq!(quantile::evaluate_quantile_status(50.0), "NEUTRAL");
}

#[test]
fn test_bitcoin_clock_and_cycle() {
    let date = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
    let clock_info = clock::analyze_cycle(date);

    // Bottom terakhir sebelum Nov 2026 adalah 2022-11-21 (~1465 hari lalu)
    assert!(clock_info.days_since_last_bottom > 1400);
    // ATH terakhir adalah Nov 2024 atau Nov 2021
    assert!(clock_info.days_since_last_ath > 0);

    // 2026 adalah tahun pemilu sela (US Midterm Election Year)
    assert!(clock_info.is_us_midterm_year);

    // 2025 bukan tahun midterm
    let date_2025 = NaiveDate::from_ymd_opt(2025, 5, 1).unwrap();
    assert!(!clock::analyze_cycle(date_2025).is_us_midterm_year);
}

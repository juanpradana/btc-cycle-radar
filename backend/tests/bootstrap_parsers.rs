//! Unit tests for parsing CoinMetrics & Binance JSON (no network).
use btc_cycle_radar::market_data::{parse_cm_page, parse_klines};
use chrono::NaiveDate;

#[test]
fn parse_cm_page_happy_path() {
    let json = r#"{
        "data": [
            {"asset":"btc","time":"2010-07-28T00:00:00.000000000Z","PriceUSD":"0.0589110987726476"},
            {"asset":"btc","time":"2010-07-29T00:00:00.000000000Z","PriceUSD":0.0699}
        ],
        "next_page_token": "abc123"
    }"#;
    let (rows, token) = parse_cm_page(json).expect("parse ok");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].date, NaiveDate::from_ymd_opt(2010, 7, 28).unwrap());
    assert!((rows[0].close - 0.0589110987726476).abs() < 1e-12);
    assert_eq!(rows[1].date, NaiveDate::from_ymd_opt(2010, 7, 29).unwrap());
    assert!((rows[1].close - 0.0699).abs() < 1e-12);
    assert_eq!(token, Some("abc123".to_string()));
}

#[test]
fn parse_cm_page_empty_data() {
    let json = r#"{"data":[],"next_page_token":null}"#;
    let (rows, token) = parse_cm_page(json).expect("parse ok");
    assert!(rows.is_empty());
    assert_eq!(token, None);
}

#[test]
fn parse_cm_page_error_field() {
    let json = r#"{"error":{"code":429,"message":"rate limited"}}"#;
    let err = parse_cm_page(json).expect_err("should error");
    assert!(err.to_string().contains("CoinMetrics API error"));
}

#[test]
fn parse_klines_happy_path() {
    // Binance klines format: [openTime, open, high, low, close, volume, closeTime, ...]
    let json = r#"[
        [1609459200000, "29000.0", "29500.0", "28800.0", "29200.0", "1234.56", 1609545599999, "36000000", 1000, "600.0", "18000000", "0"],
        [1609545600000, "29200.0", "29800.0", "29100.0", "29600.0", "1500.00", 1609631999999, "44000000", 1200, "700.0", "21000000", "0"]
    ]"#;
    let rows = parse_klines(json).expect("parse ok");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].date, NaiveDate::from_ymd_opt(2021, 1, 1).unwrap());
    assert!((rows[0].open.unwrap() - 29000.0).abs() < 1e-6);
    assert!((rows[0].high.unwrap() - 29500.0).abs() < 1e-6);
    assert!((rows[0].low.unwrap() - 28800.0).abs() < 1e-6);
    assert!((rows[0].close - 29200.0).abs() < 1e-6);
    assert!((rows[0].volume.unwrap() - 1234.56).abs() < 1e-6);
}

#[test]
fn parse_klines_malformed_row_skipped() {
    // row with < 6 elements should be skipped, not panic
    let json = r#"[[1609459200000, "29000.0"]]"#;
    let rows = parse_klines(json).expect("parse ok");
    assert!(rows.is_empty());
}

#[test]
fn parse_klines_empty_array() {
    let rows = parse_klines("[]").expect("parse ok");
    assert!(rows.is_empty());
}
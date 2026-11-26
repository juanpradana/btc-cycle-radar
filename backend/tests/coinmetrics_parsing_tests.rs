//! Tes TDD untuk parsing payload CoinMetrics dan keputusan resumabilitas BGeometrics.
//!
//! Dua bug produksi yang direproduksi di sini:
//! 1. CoinMetrics mengembalikan `null` untuk metrik sebelum coverage-nya dimulai
//!    (mis. `CapMVRVCur` pada 2010). Deserialisasi ke `HashMap<String, String>`
//!    gagal → SELURUH full history dibatalkan. Harus toleran null.
//! 2. Rate limit BGeometrics 10 request/jam memaksa full history 7 endpoint
//!    dibagi lintas beberapa jendela jam. Full sync harus melewati (skip) metrik
//!    yang datanya sudah lengkap agar tidak membuang kuota.

use btc_cycle_radar::onchain::coinmetrics::{
    build_onchain_series, parse_coinmetrics_timeseries,
};
use btc_cycle_radar::sync::{decide_metric_sync, MetricSyncDecision};
use chrono::NaiveDate;

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("tanggal valid")
}

/// Payload riil-shape dari CoinMetrics: baris awal punya banyak `null`
/// karena metrik belum punya coverage pada 2010-2011.
const PAYLOAD_WITH_NULLS: &str = r#"{
  "data": [
    {
      "asset": "btc",
      "time": "2010-07-01T00:00:00.000000000Z",
      "CapMVRVCur": null,
      "CapMrktCurUSD": null,
      "PriceUSD": null,
      "SplyCur": "3188850"
    },
    {
      "asset": "btc",
      "time": "2010-07-19T00:00:00.000000000Z",
      "CapMVRVCur": null,
      "CapMrktCurUSD": "1000000.5",
      "PriceUSD": "0.08584",
      "SplyCur": "3400000"
    },
    {
      "asset": "btc",
      "time": "2011-01-01T00:00:00.000000000Z",
      "CapMVRVCur": "1.5",
      "CapMrktCurUSD": "5000000.25",
      "PriceUSD": "0.3",
      "SplyCur": "5000000"
    }
  ]
}"#;

#[test]
fn parse_coinmetrics_tolerates_null_metric_values() {
    let payload = parse_coinmetrics_timeseries(PAYLOAD_WITH_NULLS)
        .expect("null harus ditoleransi, bukan error deserialisasi");
    assert_eq!(payload.data.len(), 3, "semua baris terbaca");

    let first = &payload.data[0];
    assert_eq!(first.time, "2010-07-01T00:00:00.000000000Z");
    assert_eq!(first.num("SplyCur"), Some(3_188_850.0));
    assert_eq!(first.num("CapMVRVCur"), None, "null → None");
    assert_eq!(first.num("TidakAdaField"), None, "field absen → None");
}

#[test]
fn parse_coinmetrics_rejects_malformed_body() {
    assert!(
        parse_coinmetrics_timeseries("{ not json").is_err(),
        "body rusak harus jadi Err, bukan data kosong"
    );
}

#[test]
fn build_onchain_series_skips_uncovered_metrics_and_derives_realized_price() {
    let payload = parse_coinmetrics_timeseries(PAYLOAD_WITH_NULLS).expect("parse");
    let series = build_onchain_series(&payload.data, d("2010-07-01"), d("2011-12-31"));

    // Supply punya coverage sejak awal
    let supply = series.get("supply").expect("supply ada");
    assert_eq!(supply.len(), 3);
    assert!((supply[0].value - 3_188_850.0).abs() < 1e-6);

    // MVRV hanya tanggal yang punya nilai (1 baris, 2011-01-01)
    let mvrv = series.get("mvrv_cm").expect("mvrv_cm ada");
    assert_eq!(mvrv.len(), 1, "hanya baris dengan nilai non-null");
    assert_eq!(mvrv[0].date, d("2011-01-01"));
    assert!((mvrv[0].value - 1.5).abs() < 1e-9);

    // Realized Price = Price / MVRV → 0.3 / 1.5 = 0.2
    let rp = series.get("realized_price_cm").expect("realized_price_cm ada");
    assert_eq!(rp.len(), 1);
    assert!(
        (rp[0].value - 0.2).abs() < 1e-9,
        "expected 0.2, got {}",
        rp[0].value
    );

    // series hasil derivasi tidak boleh menyimpan price mentah
    assert!(
        !series.contains_key("price_cm"),
        "price_cm adalah perantara, jangan disimpan"
    );
}

#[test]
fn build_onchain_series_respects_date_window() {
    let payload = parse_coinmetrics_timeseries(PAYLOAD_WITH_NULLS).expect("parse");
    let series = build_onchain_series(&payload.data, d("2011-01-01"), d("2011-12-31"));
    let supply = series.get("supply").expect("supply ada");
    assert_eq!(supply.len(), 1, "hanya baris dalam window");
    assert_eq!(supply[0].date, d("2011-01-01"));
}

#[test]
fn decide_metric_sync_fetches_when_empty() {
    let today = d("2026-11-25");
    assert_eq!(
        decide_metric_sync(0, None, today),
        MetricSyncDecision::Fetch,
        "belum ada data → fetch"
    );
}

#[test]
fn decide_metric_sync_fetches_when_incomplete() {
    let today = d("2026-11-25");
    // 120 baris & data terbaru → tetap fetch, riwayatnya masih pendek
    assert_eq!(
        decide_metric_sync(120, Some(d("2026-11-24")), today),
        MetricSyncDecision::Fetch,
        "riwayat < MIN_HISTORY_ROWS → fetch (backfill)"
    );
}

#[test]
fn decide_metric_sync_skips_when_complete_and_fresh() {
    let today = d("2026-11-25");
    assert_eq!(
        decide_metric_sync(1400, Some(d("2026-11-24")), today),
        MetricSyncDecision::SkipFresh,
        "sudah lengkap & segar → skip demi hemat kuota 10/jam"
    );
}

#[test]
fn decide_metric_sync_fetches_when_stale_even_if_complete() {
    let today = d("2026-11-25");
    assert_eq!(
        decide_metric_sync(1400, Some(d("2026-11-10")), today),
        MetricSyncDecision::Fetch,
        "data lengkap tapi basi 15 hari → fetch"
    );
}

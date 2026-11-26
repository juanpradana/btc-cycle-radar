//! Unit test untuk logika pipeline sinkronisasi on-chain & intermarket (offline, no network).
//! Prinsip TDD: tes ini ditulis dulu (RED) sebelum `src/sync.rs` dibuat.

use btc_cycle_radar::config::AppConfig;
use btc_cycle_radar::onchain::MetricFetchOutcome;
use btc_cycle_radar::sync::{
    bgeometrics_endpoints, build_sync_plan, primary_intermarket_symbols,
    summarize_metric_outcomes, yahoo_standard_symbols, StalenessReport, SyncReport,
};
use chrono::{NaiveDate, Utc};
use std::collections::HashMap;

#[test]
fn test_yahoo_standard_symbols_only_dxy_fallback() {
    let symbols = yahoo_standard_symbols();
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0], ("DX-Y.NYB", "DXY"));
}

#[test]
fn test_primary_intermarket_symbols_xau_gspc() {
    let symbols = primary_intermarket_symbols();
    assert_eq!(symbols.len(), 2);
    assert!(symbols.contains(&"XAU"));
    assert!(symbols.contains(&"GSPC"));
}

#[test]
fn test_bgeometrics_endpoints_matches_verified_8() {
    let eps = bgeometrics_endpoints();
    assert_eq!(eps.len(), 8);
    // verify exact mapping from VERIFIED_METRICS
    // (sth-realized-price & realized-price removed — not used by engine)
    let expected = vec![
        ("nupl", "nupl"),
        ("true-market-mean", "true_market_mean"),
        ("lth-realized-price", "lth_realized_price"),
        ("aviv", "aviv"),
        ("sth-mvrv", "sth_mvrv"),
        ("profit-loss", "supply_in_profit_pct"),
        ("realized_profit_lth", "realized_profit_lth"),
        ("realized_loss_lth", "realized_loss_lth"),
    ];
    for (slug, metric) in expected {
        assert!(eps.iter().any(|(s, m)| *s == slug && *m == metric));
    }
}

#[test]
fn test_summarize_metric_outcomes_counts_by_status() {
    let outcomes = vec![
        MetricFetchOutcome::ok("mvrv", 100),
        MetricFetchOutcome::ok("realized_price", 95),
        MetricFetchOutcome::ok("supply", 100),
        MetricFetchOutcome::failed("aviv", Some(500), "server error"),
        MetricFetchOutcome::rate_limited("nupl", "429 RATE_LIMIT_HOUR_EXCEEDED"),
        MetricFetchOutcome::skipped("pd", "daily cap reached"),
    ];
    let summary = summarize_metric_outcomes(&outcomes);
    assert_eq!(summary.total, 6);
    assert_eq!(summary.ok, 3);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.rate_limited, 1);
    assert_eq!(summary.total_rows, 295);
}

#[test]
fn test_staleness_report_calculates_days_since_latest() {
    let today = Utc::now().date_naive();
    let mut latest_by_metric: HashMap<String, NaiveDate> = HashMap::new();
    latest_by_metric.insert("mvrv".into(), today - chrono::Duration::days(1));
    latest_by_metric.insert("realized_price".into(), today - chrono::Duration::days(7));
    latest_by_metric.insert("old_metric".into(), today - chrono::Duration::days(120));

    let report = StalenessReport::from_latest(latest_by_metric, today);

    let mvrv = report.per_metric.get("mvrv").unwrap();
    assert_eq!(mvrv.days_since_latest, 1);
    assert!(!mvrv.is_stale);

    let rp = report.per_metric.get("realized_price").unwrap();
    assert_eq!(rp.days_since_latest, 7);
    assert!(!rp.is_stale); // < 30 hari default

    let old = report.per_metric.get("old_metric").unwrap();
    assert_eq!(old.days_since_latest, 120);
    assert!(old.is_stale);
}

#[test]
fn test_sync_report_aggregation() {
    let today = Utc::now().date_naive();
    let report = SyncReport {
        sync_date: today,
        coinmetrics_outcomes: vec![
            MetricFetchOutcome::ok("mvrv_cm", 2000),
            MetricFetchOutcome::ok("realized_price_cm", 2000),
        ],
        bgeometrics_outcomes: vec![
            MetricFetchOutcome::ok("nupl", 365),
            MetricFetchOutcome::skipped("pd", "hourly cap reached"),
        ],
        intermarket_symbols: vec!["XAU".into(), "GSPC".into(), "DXY".into()],
        intermarket_rows: vec![("XAU".into(), 1260), ("GSPC".into(), 1260), ("DXY".into(), 1260)],
        btc_candles_total: 5975,
    };

    let summary = report.summary();
    assert_eq!(summary.coinmetrics_ok, 2);
    assert_eq!(summary.bgeometrics_ok, 1);
    assert_eq!(summary.bgeometrics_skipped, 1);
    assert_eq!(summary.intermarket_symbols, 3);
    assert_eq!(summary.intermarket_total_rows, 3780);
    assert_eq!(summary.btc_candles_total, 5975);
    assert!(!summary.has_critical_errors);
}

#[test]
fn test_sync_plan_includes_bgeometrics_without_api_key() {
    // BGeometrics tier gratis TIDAK butuh API key — sync harus tetap jalan.
    let cfg = AppConfig::from_source(|_| None).expect("config");
    assert!(cfg.bgeometrics_api_key.is_none(), "precondition: no API key");
    let plan = build_sync_plan(&cfg);
    assert!(plan.bgeometrics, "BGeometrics harus termasuk walau tanpa API key");
    assert!(plan.coinmetrics, "CoinMetrics selalu included");
    assert_eq!(plan.intermarket.len(), 3, "XAU, GSPC, DXY");
    assert_eq!(plan.bgeometrics_slugs.len(), 8, "8 endpoint terverifikasi");
}

#[test]
fn test_sync_plan_bgeometrics_slugs_match_verified_8() {
    let cfg = AppConfig::from_source(|_| None).expect("config");
    let plan = build_sync_plan(&cfg);
    let expected = vec![
        "nupl",
        "true-market-mean",
        "lth-realized-price",
        "aviv",
        "sth-mvrv",
        "profit-loss",
        "realized_profit_lth",
        "realized_loss_lth",
    ];
    for slug in expected {
        assert!(
            plan.bgeometrics_slugs.iter().any(|s| s == slug),
            "missing slug: {slug}"
        );
    }
}

// ============================================================
// TESTS: mode fetch BGeometrics (regresi kuota)
// ============================================================

/// Regresi: full sync pernah memakai `fetch_full_history` untuk SEMUA metrik
/// setiap run, sehingga kuota 8/jam habis sebelum endpoint terakhir tersentuh
/// (akibatnya lth_realized_price/aviv/sth_mvrv tidak pernah tersimpan).
/// Aturan: riwayat sudah lengkap → cukup ambil /last, bukan seluruh riwayat.
#[test]
fn test_decide_fetch_mode_uses_latest_only_when_history_complete() {
    use btc_cycle_radar::sync::{decide_fetch_mode, MetricFetchMode};

    // Belum ada data → perlu riwayat penuh.
    assert_eq!(decide_fetch_mode(0), MetricFetchMode::FullHistory);
    // Ada data tapi belum mencapai ambang pelengkap → backfill riwayat penuh.
    assert_eq!(decide_fetch_mode(299), MetricFetchMode::FullHistory);
    // Sudah lengkap → cukup /last (hemat kuota).
    assert_eq!(decide_fetch_mode(300), MetricFetchMode::LatestOnly);
    assert_eq!(decide_fetch_mode(1455), MetricFetchMode::LatestOnly);
}

/// BGeometrics tier gratis menunda data 7 hari (terverifikasi dari field
/// `delayed: true` + pesan subscription pada `/v1/nupl/last` 2026-11-26).
/// Angka ini harus eksplisit di kode agar UI bisa menyatakan bahwa input
/// on-chain bukan kondisi hari ini.
#[test]
fn test_bgeometrics_free_tier_delay_is_declared() {
    use btc_cycle_radar::sync::BGEOMETRICS_FREE_TIER_DELAY_DAYS;
    assert_eq!(BGEOMETRICS_FREE_TIER_DELAY_DAYS, 7);
}

// ============================================================
// TESTS: derivasi LTH Realized P/L Ratio (matematika finansial)
// ============================================================

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).expect("tanggal valid")
}

/// Nilai referensi: ratio = profit / loss.
/// Contoh historis skala riil (USD): profit 1.50e9 / loss 1.00e9 → 1.5.
#[test]
fn test_derive_lth_pl_ratio_reference_values() {
    use btc_cycle_radar::sync::derive_lth_realized_pl_ratio;

    let profit = vec![
        (d(2021, 1, 1), 1_500_000_000.0),
        (d(2021, 1, 2), 3_000_000_000.0),
        (d(2021, 1, 3), 500_000_000.0),
    ];
    let loss = vec![
        (d(2021, 1, 1), 1_000_000_000.0),
        (d(2021, 1, 2), 1_500_000_000.0),
        (d(2021, 1, 3), 2_000_000_000.0),
    ];

    let out = derive_lth_realized_pl_ratio(&profit, &loss);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0], (d(2021, 1, 1), 1.5));
    assert_eq!(out[1], (d(2021, 1, 2), 2.0));
    assert_eq!(out[2], (d(2021, 1, 3), 0.25));
}

/// Join harus TEPAT tanggal: tanggal tanpa pasangan di seri lain dibuang
/// (bukan di-forward-fill, bukan diisi nol).
#[test]
fn test_derive_lth_pl_ratio_joins_on_matching_dates_only() {
    use btc_cycle_radar::sync::derive_lth_realized_pl_ratio;

    let profit = vec![d(2021, 1, 1), d(2021, 1, 2), d(2021, 1, 3)]
        .into_iter()
        .map(|x| (x, 2.0e9))
        .collect::<Vec<_>>();
    let loss = vec![(d(2021, 1, 2), 1.0e9)];

    let out = derive_lth_realized_pl_ratio(&profit, &loss);
    assert_eq!(out.len(), 1, "hanya 1 tanggal yang berpasangan");
    assert_eq!(out[0], (d(2021, 1, 2), 2.0));
}

/// Penyebut zero atau NaN → dibuang.
/// Kerugian negatif (format riil BGeometrics) diperlakukan sebagai magnitudo via `.abs()`.
#[test]
fn test_derive_lth_pl_ratio_discards_zero_and_nan_loss_and_supports_negative() {
    use btc_cycle_radar::sync::derive_lth_realized_pl_ratio;

    let profit = vec![
        (d(2021, 1, 1), 1.0e9), // loss = 0 → buang
        (d(2021, 1, 2), 1.0e9), // loss < 0 (format BGeo) → abs() → 1.0e9 / 2.0e9 = 0.5
        (d(2021, 1, 3), 1.0e9), // loss NaN → buang
        (d(2021, 1, 4), 1.0e9), // loss valid positif → 1.0e9 / 2.0e9 = 0.5
    ];
    let loss = vec![
        (d(2021, 1, 1), 0.0),
        (d(2021, 1, 2), -2.0e9),
        (d(2021, 1, 3), f64::NAN),
        (d(2021, 1, 4), 2.0e9),
    ];

    let out = derive_lth_realized_pl_ratio(&profit, &loss);
    assert_eq!(out.len(), 2, "zero dan NaN dibuang, negatif diubah ke abs");
    assert_eq!(out[0], (d(2021, 1, 2), 0.5));
    assert_eq!(out[1], (d(2021, 1, 4), 0.5));
}

/// Profit NaN/±inf → dibuang (bukan menghasilkan rasio sampah).
#[test]
fn test_derive_lth_pl_ratio_discards_nonfinite_profit() {
    use btc_cycle_radar::sync::derive_lth_realized_pl_ratio;

    let profit = vec![
        (d(2021, 1, 1), f64::NAN),
        (d(2021, 1, 2), f64::INFINITY),
        (d(2021, 1, 3), 1.0e9),
    ];
    let loss = vec![
        (d(2021, 1, 1), 1.0e9),
        (d(2021, 1, 2), 1.0e9),
        (d(2021, 1, 3), 1.0e9),
    ];

    let out = derive_lth_realized_pl_ratio(&profit, &loss);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].1, 1.0);
}

/// Penyebut sangat kecil (< 1000 USD) menghasilkan rasio eksplosif → dibuang.
/// Terbukti dari data riil: rasio mencapai 1.99e11 saat realized_loss mendekati nol.
#[test]
fn test_derive_lth_pl_ratio_discards_tiny_loss_denominator() {
    use btc_cycle_radar::sync::derive_lth_realized_pl_ratio;

    let profit = vec![
        (d(2021, 1, 1), 1.0e9), // loss = 1 USD → dibuang (rasio akan 1e9)
        (d(2021, 1, 2), 1.0e9), // loss = 999 USD → dibuang (di bawah ambang)
        (d(2021, 1, 3), 1.0e9), // loss = 1000 USD → diterima (rasio 1e6, besar tapi sah)
    ];
    let loss = vec![
        (d(2021, 1, 1), 1.0),
        (d(2021, 1, 2), 999.0),
        (d(2021, 1, 3), 1_000.0),
    ];

    let out = derive_lth_realized_pl_ratio(&profit, &loss);
    assert_eq!(out.len(), 1, "hanya loss >= 1000 USD yang diterima");
    assert_eq!(out[0], (d(2021, 1, 3), 1.0e6));
}

/// Profit nol/negatif dibuang: rasio P/L negatif tidak bermakna untuk indikator psikologi.
#[test]
fn test_derive_lth_pl_ratio_discards_nonpositive_profit() {
    use btc_cycle_radar::sync::derive_lth_realized_pl_ratio;

    let profit = vec![
        (d(2021, 1, 1), 0.0),
        (d(2021, 1, 2), -5.0e9),
        (d(2021, 1, 3), 2.0e9),
    ];
    let loss = vec![
        (d(2021, 1, 1), 1.0e9),
        (d(2021, 1, 2), 1.0e9),
        (d(2021, 1, 3), 1.0e9),
    ];

    let out = derive_lth_realized_pl_ratio(&profit, &loss);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], (d(2021, 1, 3), 2.0));
}

/// Output selalu naik tanggal, walau input tidak terurut (SQL ORDER BY dijamin,
/// tapi fungsi murni tidak boleh bergantung pada itu) & duplikat dibuang.
#[test]
fn test_derive_lth_pl_ratio_sorts_and_dedups() {
    use btc_cycle_radar::sync::derive_lth_realized_pl_ratio;

    let profit = vec![
        (d(2021, 1, 3), 3.0e9),
        (d(2021, 1, 1), 1.0e9),
        (d(2021, 1, 2), 2.0e9),
        (d(2021, 1, 2), 2.0e9), // duplikat
    ];
    let loss = vec![
        (d(2021, 1, 1), 1.0e9),
        (d(2021, 1, 2), 1.0e9),
        (d(2021, 1, 3), 1.0e9),
    ];

    let out = derive_lth_realized_pl_ratio(&profit, &loss);
    assert_eq!(out.len(), 3, "duplikat tanggal dibuang");
    let dates: Vec<_> = out.iter().map(|(dt, _)| *dt).collect();
    let mut sorted = dates.clone();
    sorted.sort();
    assert_eq!(dates, sorted, "output terurut naik");
}

// ============================================================
// KEBIJAKAN INDIKATOR: nonaktif secara eksplisit, bukan di-comment.
// ============================================================

/// `lth_realized_pl_ratio` dinonaktifkan karena penyebut dari tier gratis
/// BGeometrics kolaps saat bull market (median realized_loss LTH harian hanya
/// ~$2.090 di 2025; 241/361 hari < $100k) sehingga rasio meledak ke 1e8–1e11
/// dan SELALU memicu `> 20 = TopZone` — sinyal palsu permanen.
///
/// Kebijakan proyek: indikator yang tak dapat dihitung dari data layak
/// dilaporkan MISSING, bukan diisi angka menyesatkan. Fungsi derivasi tetap
/// ada & teruji (bila sumber tepercaya tersedia, tinggal flip flag ini).
///
/// Tes ini mengunci keputusan itu supaya tidak ada yang diam-diam
/// mengaktifkannya kembali tanpa bukti sumber data yang lebih baik.
#[test]
fn test_lth_realized_pl_ratio_disabled_pending_trustworthy_source() {
    use btc_cycle_radar::sync::LTH_REALIZED_PL_RATIO_ENABLED;

    assert!(
        !LTH_REALIZED_PL_RATIO_ENABLED,
        "lth_realized_pl_ratio harus TETAP nonaktif: penyebut free-tier \
         BGeometrics degenerate saat bull market. Aktifkan hanya bila sumber \
         entity-adjusted (mis. Glassnode Professional) tersedia."
    );
}

/// Saat flag nonaktif, fungsi derivasi TIDAK boleh dipanggil produksi.
/// Diverifikasi lewat `production_metric_for_slug`: slug komponen LTH tetap
/// di-fetch sebagai bukti audit, tetapi tidak ada metrik turunan yang disimpan.
#[test]
fn test_derive_gate_matches_flag() {
    use btc_cycle_radar::sync::{should_derive_lth_realized_pl_ratio, LTH_REALIZED_PL_RATIO_ENABLED};

    assert_eq!(
        should_derive_lth_realized_pl_ratio(),
        LTH_REALIZED_PL_RATIO_ENABLED,
        "gate runtime harus konsisten dengan konstanta kebijakan"
    );
}
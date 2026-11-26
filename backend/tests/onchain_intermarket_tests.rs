use btc_cycle_radar::db;
use btc_cycle_radar::intermarket::yahoo::parse_yahoo_chart;
use btc_cycle_radar::intermarket::{
    find_closest_before, intermarket_series, upsert_intermarket, IntermarketValue,
};
use btc_cycle_radar::onchain::bgeometrics::parse_bgeometrics_payload;
use btc_cycle_radar::onchain::{requests_in_window, upsert_onchain_values, OnchainValue};
use chrono::NaiveDate;
use sqlx::{Row, SqlitePool};
use tempfile::NamedTempFile;

async fn fresh_pool() -> (SqlitePool, NamedTempFile) {
    let tmp = NamedTempFile::new().expect("temp file");
    let db_url = format!("sqlite://{}", tmp.path().display());
    let pool = db::connect(&db_url).await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    (pool, tmp)
}

#[tokio::test]
async fn rate_limit_guard_counts_only_quota_consuming_calls() {
    let (pool, _tmp) = fresh_pool().await;

    // Hanya ok/error/rate_limited yang memakan kuota; skipped tidak.
    for (status, msg) in [
        ("ok", None),
        ("ok", None),
        ("error", Some("HTTP 500")),
        ("rate_limited", Some("reset_epoch=1770000000")),
        ("skipped", Some("hourly cap reached")),
        ("skipped", Some("hourly cap reached")),
    ] {
        sqlx::query(
            "INSERT INTO fetch_logs (source, endpoint, status, http_code, rows, message)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("bgeometrics")
        .bind("realized-price")
        .bind(status)
        .bind(200)
        .bind(1)
        .bind(msg)
        .execute(&pool)
        .await
        .expect("insert fetch_log");
    }

    let used = requests_in_window(&pool, "bgeometrics", 3600)
        .await
        .expect("count");
    assert_eq!(used, 4, "skipped tidak boleh menghabiskan kuota");
}

#[tokio::test]
async fn rate_limit_reset_epoch_is_readable() {
    let (pool, _tmp) = fresh_pool().await;
    sqlx::query(
        "INSERT INTO fetch_logs (source, endpoint, status, http_code, rows, message)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind("bgeometrics")
    .bind("realized-price")
    .bind("rate_limited")
    .bind(429)
    .bind(0)
    .bind("RATE_LIMIT_HOUR_EXCEEDED reset_epoch=1770000000")
    .execute(&pool)
    .await
    .expect("insert");

    let reset = btc_cycle_radar::onchain::last_rate_limit_reset(&pool, "bgeometrics")
        .await
        .expect("reset");
    assert_eq!(reset, Some(1770000000));
}

#[tokio::test]
async fn onchain_upsert_is_idempotent_and_updates_source() {
    let (pool, _tmp) = fresh_pool().await;
    let d1 = NaiveDate::from_ymd_opt(2026, 1, 1).expect("valid");
    let d2 = NaiveDate::from_ymd_opt(2026, 1, 2).expect("valid");

    let initial = vec![
        OnchainValue { date: d1, value: 50000.0 },
        OnchainValue { date: d2, value: 51000.0 },
    ];
    let inserted = upsert_onchain_values(&pool, "realized_price", "coinmetrics", &initial)
        .await
        .expect("insert");
    assert_eq!(inserted, 2);

    // Timpa tanggal 2 dengan nilai & sumber berbeda
    let update = vec![OnchainValue { date: d2, value: 51500.0 }];
    let updated = upsert_onchain_values(&pool, "realized_price", "bgeometrics", &update)
        .await
        .expect("update");
    assert_eq!(updated, 1);

    let row = sqlx::query(
        "SELECT value, source FROM onchain_daily \
         WHERE metric = 'realized_price' AND date = '2026-01-02'",
    )
    .fetch_one(&pool)
    .await
    .expect("select");
    let val: f64 = row.get("value");
    let src: String = row.get("source");
    assert!((val - 51500.0).abs() < 1e-6, "nilai harus ter-overwrite");
    assert_eq!(src, "bgeometrics", "source harus ikut ter-overwrite");
}

#[tokio::test]
async fn onchain_upsert_skips_non_finite_values() {
    let (pool, _tmp) = fresh_pool().await;
    let vals = vec![
        OnchainValue {
            date: NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid"),
            value: f64::NAN,
        },
        OnchainValue {
            date: NaiveDate::from_ymd_opt(2026, 3, 2).expect("valid"),
            value: f64::INFINITY,
        },
    ];
    let n = upsert_onchain_values(&pool, "nupl", "bgeometrics", &vals)
        .await
        .expect("upsert");
    assert_eq!(n, 0, "NaN/inf tidak boleh masuk database");
}

#[tokio::test]
async fn intermarket_upsert_is_idempotent() {
    let (pool, _tmp) = fresh_pool().await;
    let d = NaiveDate::from_ymd_opt(2026, 11, 24).expect("valid");

    let gold = vec![IntermarketValue { date: d, close: 3550.5 }];
    let ins = upsert_intermarket(&pool, "XAU", "yahoo_finance", &gold)
        .await
        .expect("ins");
    assert_eq!(ins, 1);

    // Upsert ulang dengan nilai revisi — tidak boleh menambah baris
    let gold_rev = vec![IntermarketValue { date: d, close: 3552.0 }];
    let ins2 = upsert_intermarket(&pool, "XAU", "yahoo_finance", &gold_rev)
        .await
        .expect("ins2");
    assert_eq!(ins2, 1);

    let series = intermarket_series(&pool, "XAU").await.expect("series");
    assert_eq!(series.len(), 1);
    assert!((series[0].close - 3552.0).abs() < 1e-6);
}

#[test]
fn find_closest_before_handles_weekends_and_holidays() {
    let d1 = NaiveDate::from_ymd_opt(2026, 11, 20).expect("valid"); // Jumat
    let d2 = NaiveDate::from_ymd_opt(2026, 11, 24).expect("valid"); // Selasa

    let series = vec![
        IntermarketValue { date: d1, close: 3500.0 },
        IntermarketValue { date: d2, close: 3550.0 },
    ];

    // Sabtu 2026-11-21 -> jatuh ke harga Jumat
    let sabtu = NaiveDate::from_ymd_opt(2026, 11, 21).expect("valid");
    assert_eq!(find_closest_before(&series, sabtu), Some(3500.0));
    assert_eq!(find_closest_before(&series, d2), Some(3550.0));
    // Sebelum data tersedia -> None
    let pre = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid");
    assert_eq!(find_closest_before(&series, pre), None);
}

#[test]
fn parse_bgeometrics_payload_supports_array_and_single_object() {
    // Format array of objects
    let json_arr = r#"[
        {"d": "2026-11-20", "v": 53400.5},
        {"d": "2026-11-21", "v": 53450.0}
    ]"#;
    let res = parse_bgeometrics_payload(json_arr, "realized-price");
    assert_eq!(res.len(), 2);
    assert_eq!(
        res[0].date,
        NaiveDate::from_ymd_opt(2026, 11, 20).expect("valid")
    );
    assert!((res[0].value - 53400.5).abs() < 1e-6);
    assert!(res[0].date < res[1].date, "hasil harus terurut menaik");

    // Format objek tunggal (endpoint /last)
    let json_single = r#"{"d": "2026-11-24", "v": 53500.0}"#;
    let res2 = parse_bgeometrics_payload(json_single, "realized-price");
    assert_eq!(res2.len(), 1);
    assert!((res2[0].value - 53500.0).abs() < 1e-6);
}

#[test]
fn parse_bgeometrics_payload_ignores_malformed_rows() {
    let json = r#"[
        {"d": "2026-11-20", "v": 100.0},
        {"d": "bukan-tanggal", "v": 200.0},
        {"d": "2026-11-22", "v": null}
    ]"#;
    let res = parse_bgeometrics_payload(json, "realized-price");
    assert_eq!(
        res.len(),
        1,
        "baris rusak harus dibuang, bukan menghasilkan angka palsu"
    );
    assert!((res[0].value - 100.0).abs() < 1e-6);
}

#[test]
fn parse_yahoo_chart_handles_nulls_and_extracts_closes() {
    let yahoo_json = r#"{
        "chart": {
            "result": [
                {
                    "meta": { "symbol": "GC=F" },
                    "timestamp": [1763510400, 1763596800, 1763683200],
                    "indicators": {
                        "quote": [ { "close": [3500.25, null, 3515.5] } ]
                    }
                }
            ],
            "error": null
        }
    }"#;

    let res = parse_yahoo_chart(yahoo_json).expect("parse");
    assert_eq!(res.len(), 2, "close null harus difilter");
    assert!((res[0].close - 3500.25).abs() < 1e-6);
    assert!((res[1].close - 3515.5).abs() < 1e-6);
    assert!(res[0].date < res[1].date, "hasil harus terurut menaik");
}

#[test]
fn parse_yahoo_chart_rejects_error_payload() {
    let err_json =
        r#"{"chart": {"result": null, "error": {"code": "Not Found", "description": "No data found"}}}"#;
    assert!(
        parse_yahoo_chart(err_json).is_err(),
        "payload error harus jadi Err, bukan data kosong"
    );
}

// ============================================================
// TESTS: transform payload riil BGeometrics (diverifikasi 2026-11-26)
// Payload di bawah adalah respons HTTP 200 asli dari
// https://bitcoin-data.com/v1/<slug>/last
// ============================================================

/// `GET /v1/profit-loss/last` (respons riil):
/// {"d":"2026-11-24","unixTs":1795478400,"profitLoss":0.8035371464617553}
/// Nilai API berupa FRAKSI 0-1; engine memakai skala PERSEN 0-100
/// (`evaluate_supply_in_profit`: <=53 BUYZONE, >=95 TOP_ZONE).
/// Tanpa ×100, 80.35% akan terbaca 0.80% → salah klasifikasi jadi Deep Capitulation.
#[test]
fn parse_profit_loss_scales_fraction_to_percent() {
    let live = r#"{"d":"2026-11-24","unixTs":1795478400,"profitLoss":0.8035371464617553}"#;
    let res = parse_bgeometrics_payload(live, "profit-loss");
    assert_eq!(res.len(), 1);
    assert_eq!(
        res[0].date,
        NaiveDate::from_ymd_opt(2026, 11, 24).expect("valid")
    );
    assert!(
        (res[0].value - 80.35371464617553).abs() < 1e-9,
        "fraksi 0.8035 harus jadi 80.35 persen, dapat {}",
        res[0].value
    );
    // Pastikan masuk rentang persen yang wajar (bukan 0-1)
    assert!(res[0].value > 1.0 && res[0].value <= 100.0);
}

/// Provider BGeometrics mengirim `profitLoss` = 1.0 secara PINNED pada
/// rentang historis panjang (2022–2026). Itu artefak provider: supply in
/// profit tidak pernah tepat 100% karena selalu ada UTXO di bawah harga
/// akuisisi. Kebijakan: buang baris semacam itu — JANGAN dicap 100%, karena
/// cap berarti mengarang nilai. Hanya baris plausibel (0 < x < 1) yang lolos.
#[test]
fn parse_profit_loss_discards_impossible_full_supply_rows() {
    let payload = r#"[
        {"d":"2022-11-26","unixTs":1669420800,"profitLoss":1.0},
        {"d":"2024-01-01","unixTs":1704067200,"profitLoss":1.0000001},
        {"d":"2025-06-01","unixTs":1748736000,"profitLoss":0.0},
        {"d":"2026-11-24","unixTs":1795478400,"profitLoss":0.8035371464617553}
    ]"#;
    let res = parse_bgeometrics_payload(payload, "profit-loss");
    assert_eq!(
        res.len(),
        1,
        "hanya baris plausibel yang boleh lolos, dapat {res:?}"
    );
    assert_eq!(
        res[0].date,
        NaiveDate::from_ymd_opt(2026, 11, 24).expect("valid")
    );
    assert!((res[0].value - 80.35371464617553).abs() < 1e-9);
}

/// `GET /v1/realized_profit_lth/last` (respons riil):
/// {"d":"2026-11-24","unixTs":"1795478400","realizedProfitLth":"137922027.16"}
/// Nilai dikirim sebagai STRING dan field-nya camelCase.
#[test]
fn parse_realized_profit_lth_reads_camelcase_string_numeric() {
    let live = r#"{"d":"2026-11-24","unixTs":"1795478400","realizedProfitLth":"137922027.16"}"#;
    let res = parse_bgeometrics_payload(live, "realized_profit_lth");
    assert_eq!(res.len(), 1);
    assert!(
        (res[0].value - 137_922_027.16).abs() < 1e-6,
        "string numerik harus terbaca, dapat {}",
        res[0].value
    );
}

/// `GET /v1/realized_loss_lth/last` (respons riil):
/// {"d":"2026-11-24","unixTs":1795478400,"realizedLossLth":-6.8446138704E8}
/// Kerugian dikirim NEGATIF dalam notasi ilmiah. Untuk rasio kita butuh
/// magnitudo, jadi parser menyimpan nilai absolut.
#[test]
fn parse_realized_loss_lth_stores_absolute_magnitude() {
    let live = r#"{"d":"2026-11-24","unixTs":1795478400,"realizedLossLth":-6.8446138704E8}"#;
    let res = parse_bgeometrics_payload(live, "realized_loss_lth");
    assert_eq!(res.len(), 1);
    assert!(
        (res[0].value - 684_461_387.04).abs() < 0.01,
        "kerugian negatif harus jadi magnitudo positif, dapat {}",
        res[0].value
    );
    assert!(res[0].value > 0.0, "nilai tersimpan harus positif");
}

/// End-to-end dari payload riil: ratio = profit / |loss|.
/// 137,922,027.16 / 684,461,387.04 = 0.2015…
/// (< 1.0 → engine BUYZONE: LTH belum profit, tanda akumulasi).
#[test]
fn derive_ratio_from_live_bgeometrics_payloads() {
    use btc_cycle_radar::sync::derive_lth_realized_pl_ratio;

    let profit_live = r#"{"d":"2026-11-24","unixTs":"1795478400","realizedProfitLth":"137922027.16"}"#;
    let loss_live = r#"{"d":"2026-11-24","unixTs":1795478400,"realizedLossLth":-6.8446138704E8}"#;

    let profit_rows = parse_bgeometrics_payload(profit_live, "realized_profit_lth");
    let loss_rows = parse_bgeometrics_payload(loss_live, "realized_loss_lth");
    assert_eq!(profit_rows.len(), 1);
    assert_eq!(loss_rows.len(), 1);

    let profit: Vec<_> = profit_rows.iter().map(|r| (r.date, r.value)).collect();
    let loss: Vec<_> = loss_rows.iter().map(|r| (r.date, r.value)).collect();

    let ratio = derive_lth_realized_pl_ratio(&profit, &loss);
    assert_eq!(ratio.len(), 1);
    let expected = 137_922_027.16_f64 / 684_461_387.04_f64;
    assert!(
        (ratio[0].1 - expected).abs() < 1e-9,
        "ratio harus {expected}, dapat {}",
        ratio[0].1
    );
    // Nilai riil < 1.0 → pasar berada di zona LTH belum profit (buyzone).
    assert!(ratio[0].1 < 1.0);
}

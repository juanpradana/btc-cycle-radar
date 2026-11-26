//! Pipeline sinkronisasi on-chain & intermarket (Phase 2).
//!
//! Prinsip:
//! - Pure functions untuk logika yang tidak butuh jaringan (testable offline).
//! - Async orchestration functions yang memanggil connector & DB.
//! - Tidak ada `unwrap()` pada input eksternal; error → `Result` / `anyhow`.
//! - Semua fetch dicatat ke `fetch_logs` (rate-limit guard).

use super::config::AppConfig;
use super::db;
use super::onchain::bgeometrics::{BGeometricsClient, DEFAULT_BGEOMETRICS_BASE};
use super::onchain::coinmetrics::{
    build_client_from_config as build_cm_client,
};
use super::onchain::{FetchStatus, MetricFetchOutcome};
use anyhow::Result;
use chrono::{NaiveDate, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Pure helpers (offline testable)
// ---------------------------------------------------------------------------

/// Keputusan apakah metrik harus di-fetch atau di-skip.
/// Disediakan untuk resumabilitas full sync lintas jendela rate-limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricSyncDecision {
    Fetch,
    SkipFresh,
}

/// Tentukan apakah metrik perlu di-fetch atau bisa di-skip demi hemat kuota.
/// - Belum ada data (rows == 0) → Fetch
/// - Data pendek (< MIN_HISTORY_ROWS) → Fetch (backfill histori)
/// - Data lengkap & segar (latest ≤ 2 hari lalu) → SkipFresh
/// - Data lengkap tapi basi (> 2 hari) → Fetch
pub fn decide_metric_sync(
    existing_rows: i64,
    latest_date: Option<NaiveDate>,
    today: NaiveDate,
) -> MetricSyncDecision {
    if existing_rows == 0 {
        return MetricSyncDecision::Fetch;
    }
    if existing_rows < MIN_HISTORY_ROWS {
        return MetricSyncDecision::Fetch; // backfill histori belum lengkap
    }
    let stale_days = latest_date
        .map(|d| (today - d).num_days())
        .unwrap_or(i64::MAX);
    if stale_days <= 2 {
        MetricSyncDecision::SkipFresh
    } else {
        MetricSyncDecision::Fetch
    }
}

/// Mode fetch untuk BGeometrics: riwayat penuh vs hanya `/last`.
/// Digunakan agar full sync hemat kuota: data sudah lengkap → ambil /last saja.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricFetchMode {
    FullHistory,
    LatestOnly,
}

/// Ambang minimal baris agar riwayat dianggap "lengkap" untuk BGeometrics.
/// 300 baris ≈ 1 tahun daily; endpoint /v1/{slug} mengembalikan ~1.4k baris.
pub const MIN_HISTORY_ROWS: i64 = 300;

/// Tentukan mode fetch: data pendek → butuh riwayat penuh; data lengkap → /last.
pub fn decide_fetch_mode(existing_rows: i64) -> MetricFetchMode {
    if existing_rows < MIN_HISTORY_ROWS {
        MetricFetchMode::FullHistory
    } else {
        MetricFetchMode::LatestOnly
    }
}

/// BGeometrics tier gratis menunda data **7 hari** (terverifikasi dari respons
/// `/v1/nupl/last`: `"delayed": true, "message": "Real-time data (last 7 days) requires an active subscription"`).
/// Konstanta ini harus eksplisit agar UI/engine bisa mendisklousurkan bahwa input
/// on-chain psychology **bukan kondisi hari ini**.
pub const BGEOMETRICS_FREE_TIER_DELAY_DAYS: i64 = 7;

/// Derivasi **LTH Realized P/L Ratio** = LTH Realized Profit / LTH Realized Loss.
///
/// # Status: TIDAK DIPAKAI di produksi (2026-11-26)
///
/// Fungsi ini **matematis benar** dan tetap diuji terhadap payload riil
/// (`derive_ratio_from_live_bgeometrics_payloads`), tetapi **tidak dipanggil**
/// oleh `run_full_sync` karena input-nya tidak layak dipercaya:
///
/// BGeometrics tier gratis mengembalikan `realized_loss_lth` yang **degenerate**
/// selama bull market — median harian hanya **$2.090** di 2025 (241 dari 361 hari
/// < $100k), versus $276 juta di 2022. Akibatnya `profit/loss` meledak ke
/// 1e8–1e11 dan **selalu** memicu `> 20 = TopZone` di engine: sinyal palsu permanen.
///
/// Kebijakan proyek: indikator yang tidak dapat dihitung dari data layak
/// dilaporkan **MISSING**, bukan diisi angka turunan yang menyesatkan.
/// Karena itu `sync.rs` tidak memanggil fungsi ini dan engine menerima `None`
/// → `missing_indicators` memuat `lth_realized_pl_ratio`, bobotnya di-redistribusi.
///
/// Aktifkan kembali hanya bila sumber data berganti ke metrik **entity-adjusted**
/// (mis. Glassnode Professional `realized_profit_lth_account_based`), yang
/// penyebutnya tidak kolaps saat holder menahan koin.
///
/// Definisi resmi (Glassnode): `ema(LTH Realized Profit / LTH Realized Loss, 7)`.
/// Di sini kami menyimpan **rasio raw per hari** (tanpa EMA7) agar deterministik
/// dan dapat direproduksi; provenance `bgeometrics_derived` menandai bedanya di UI.
///
/// Aturan:
/// - Hanya tanggal yang ADA di kedua seri (join tepat tanggal) yang dihitung.
/// - Penyebut diambil nilai absolutnya (`loss.abs()`) karena BGeometrics mengembalikan
///   kerugian sebagai angka negatif; kita butuh magnitudo kerugian.
/// - `loss == 0` atau non-finite → rasio tak bermakna, dibuang.
/// - Hasil non-finite dibuang.
/// - Output terurut menaik berdasarkan tanggal.
///
/// Pure function — tanpa I/O; diuji terhadap nilai referensi nyata.
#[allow(dead_code)] // sengaja tidak dipanggil: lihat alasan kualitas data di atas
pub fn derive_lth_realized_pl_ratio(
    profit_series: &[(NaiveDate, f64)],
    loss_series: &[(NaiveDate, f64)],
) -> Vec<(NaiveDate, f64)> {
    // Filter profit > 0, loss >= MIN_LOSS (avoid near-zero loss → huge ratios)
    const MIN_LOSS: f64 = 1_000.0; // USD minimum — near-zero realized loss implies unreliable ratio
    let loss_map: HashMap<NaiveDate, f64> = loss_series
        .iter()
        .filter(|(_, v)| v.is_finite())
        .map(|(d, v)| (*d, v.abs()))
        .filter(|(_, v)| *v >= MIN_LOSS)
        .collect();

    let mut out: Vec<(NaiveDate, f64)> = profit_series
        .iter()
        .filter(|(_, v)| v.is_finite() && *v > 0.0)
        .filter_map(|(d, profit)| {
            loss_map.get(d).and_then(|loss| {
                let ratio = profit / loss;
                if ratio.is_finite() {
                    Some((*d, ratio))
                } else {
                    None
                }
            })
        })
        .collect();

    out.sort_by_key(|(d, _)| *d);
    out.dedup_by_key(|(d, _)| *d);
    out
}

/// Kebijakan indikator (2026-11-26):
/// `lth_realized_pl_ratio` dinonaktifkan karena penyebut `realized_loss_lth`
/// dari tier gratis BGeometrics kolaps saat bull market (median ~$2.090 di 2025;
/// 241/361 hari < $100k) → rasio meledak ke 1e8–1e11, SELALU memicu
/// `> 20 = TopZone`. Indikator dilaporkan MISSING (bukan perkiraan).
///
/// Flip ke `true` HANYA bila sumber entity-adjusted tersedia (mis. Glassnode
/// Professional `realized_profit_lth_account_based`,
/// `realized_loss_lth_account_based`) dan divalidasi ulang.
pub const LTH_REALIZED_PL_RATIO_ENABLED: bool = false;

/// Gate runtime — dipakai `run_full_sync` untuk memutuskan apakah
/// mempersist metrik turunan `lth_realized_pl_ratio`.
pub fn should_derive_lth_realized_pl_ratio() -> bool {
    LTH_REALIZED_PL_RATIO_ENABLED
}

// ---------------------------------------------------------------------------
// Endpoint & sync plan (offline logic, tested by sync_pipeline_tests.rs)
// ---------------------------------------------------------------------------

/// Daftar simbol intermarket yang di-fetch oleh sync.
/// (query_symbol, internal_symbol) tetap untuk kompatibilitas dengan plan/test;
/// worker intermarket memakai FRED SP500 + Binance PAXG (tidak Yahoo untuk XAU/GSPC).
pub fn yahoo_standard_symbols() -> Vec<(&'static str, &'static str)> {
    vec![("DX-Y.NYB", "DXY")]
}

/// Simbol internal yang di-fetch oleh sumber non-Yahoo.
pub fn primary_intermarket_symbols() -> Vec<&'static str> {
    vec!["XAU", "GSPC"]
}

/// Daftar endpoint BGeometrics yang terverifikasi (sama dengan `VERIFIED_METRICS`).
/// Dipetakan ke nama metric internal di tabel `onchain_daily`.
///
/// Urutan = prioritas kuota: metrik yang TIDAK tersedia dari CoinMetrics diletakkan
/// DEPAN agar mendapat budget BGeometrics 8/jam sebelum kehabisan.
/// - nupl, true_market_mean: HANYA dari BGeometrics (Tier S)
/// - lth_realized_price, aviv, sth_mvrv: HANYA dari BGeometrics (Tier A/B)
/// - profit-loss → supply_in_profit_pct: satu-satunya sumber gratis (% supply in profit)
/// - realized_profit_lth + realized_loss_lth → bahan turunan lth_realized_pl_ratio
/// - sth_realized_price & realized_price: DIBUANG — tidak dipakai engine, membuang budget.
pub fn bgeometrics_endpoints() -> Vec<(&'static str, &'static str)> {
    vec![
        // Prioritas: 3 metrik baru (butuh FullHistory, riwayat belum ada)
        ("profit-loss", "supply_in_profit_pct"),
        ("realized_profit_lth", "realized_profit_lth"),
        ("realized_loss_lth", "realized_loss_lth"),
        // Metrik lama: riwayat sudah lengkap → LatestOnly (hemat kuota)
        ("nupl", "nupl"),
        ("true-market-mean", "true_market_mean"),
        ("lth-realized-price", "lth_realized_price"),
        ("aviv", "aviv"),
        ("sth-mvrv", "sth_mvrv"),
    ]
}

/// Ringkasan hasil fetch per metric (offline testable).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricOutcomeSummary {
    pub total: usize,
    pub ok: usize,
    pub failed: usize,
    pub rate_limited: usize,
    pub skipped: usize,
    pub total_rows: i64,
}

pub fn summarize_metric_outcomes(outcomes: &[MetricFetchOutcome]) -> MetricOutcomeSummary {
    let mut s = MetricOutcomeSummary {
        total: outcomes.len(),
        ..Default::default()
    };
    for o in outcomes {
        match o.status {
            FetchStatus::Ok => s.ok += 1,
            FetchStatus::Error => s.failed += 1,
            FetchStatus::RateLimited => s.rate_limited += 1,
            FetchStatus::Skipped => s.skipped += 1,
        }
        s.total_rows += o.rows as i64;
    }
    s
}

/// Status staleness per metrik (offline testable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricStaleness {
    pub metric: String,
    pub latest_date: Option<NaiveDate>,
    pub days_since_latest: i64,
    pub is_stale: bool, // default > 30 hari dianggap stale
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StalenessReport {
    pub as_of: NaiveDate,
    pub per_metric: HashMap<String, MetricStaleness>,
}

impl StalenessReport {
    /// Bangun dari `HashMap<metric, latest_date>`.
    /// `stale_threshold_days` default 30.
    pub fn from_latest(latest_by_metric: HashMap<String, NaiveDate>, as_of: NaiveDate) -> Self {
        let stale_threshold = 30;
        let mut per_metric = HashMap::new();
        for (metric, latest_date) in latest_by_metric {
            let days = (as_of - latest_date).num_days();
            per_metric.insert(
                metric.clone(),
                MetricStaleness {
                    metric,
                    latest_date: Some(latest_date),
                    days_since_latest: days,
                    is_stale: days > stale_threshold,
                },
            );
        }
        Self { as_of, per_metric }
    }
}

// ---------------------------------------------------------------------------
// Sync Report (hasil orkestrasi)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub sync_date: NaiveDate,
    pub coinmetrics_outcomes: Vec<MetricFetchOutcome>,
    pub bgeometrics_outcomes: Vec<MetricFetchOutcome>,
    pub intermarket_symbols: Vec<String>,
    pub intermarket_rows: Vec<(String, usize)>,
    pub btc_candles_total: i64,
}

// ---------------------------------------------------------------------------
// Sync Plan (offline testable) — menentukan APA yang di-fetch tanpa jaringan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPlan {
    /// Apakah CoinMetrics akan di-fetch (selalu true, tidak butuh key).
    pub coinmetrics: bool,
    /// Apakah BGeometrics akan di-fetch (true walau tanpa API key — tier gratis bekerja).
    pub bgeometrics: bool,
    /// Simbol intermarket yang akan di-fetch (query_symbol, internal_symbol).
    pub intermarket: Vec<(String, String)>,
    /// Slug endpoint BGeometrics yang akan di-fetch (sesuai VERIFIED_METRICS).
    pub bgeometrics_slugs: Vec<String>,
    /// Start date untuk CoinMetrics full history.
    pub coinmetrics_start: NaiveDate,
    /// Start date untuk BGeometrics full history.
    pub bgeometrics_start: NaiveDate,
}

/// Bangun rencana sinkronisasi dari config — pure function, testable offline.
/// Rule: BGeometrics tier gratis TIDAK butuh API key (endpoint `/v1/{slug}` & `/v1/{slug}/last`
/// keduanya publik). Key hanya menaikkan tier (real-time, kuota lebih besar).
pub fn build_sync_plan(config: &AppConfig) -> SyncPlan {
    let mut intermarket = vec![
        ("binance:PAXGUSDT".to_string(), "XAU".to_string()),
        ("fred:SP500".to_string(), "GSPC".to_string()),
    ];
    intermarket.extend(
        yahoo_standard_symbols()
            .into_iter()
            .map(|(q, s)| (q.to_string(), s.to_string())),
    );
    let bgeometrics_slugs: Vec<String> = bgeometrics_endpoints()
        .iter()
        .map(|(s, _m)| s.to_string())
        .collect();

    SyncPlan {
        coinmetrics: true,
        bgeometrics: true, // <-- TIER GRATIS BEKERJA TANPA KEY
        intermarket,
        bgeometrics_slugs,
        coinmetrics_start: config.bootstrap_start,
        bgeometrics_start: config.bootstrap_start,
    }
}

impl SyncReport {
    pub fn summary(&self) -> SyncSummary {
        let cm = summarize_metric_outcomes(&self.coinmetrics_outcomes);
        let bg = summarize_metric_outcomes(&self.bgeometrics_outcomes);
        let intermarket_total_rows: usize = self.intermarket_rows.iter().map(|(_, n)| *n).sum();
        let has_critical_errors = cm.failed > 0 || bg.failed > 0;
        SyncSummary {
            sync_date: self.sync_date,
            coinmetrics_ok: cm.ok,
            coinmetrics_failed: cm.failed,
            coinmetrics_rate_limited: cm.rate_limited,
            bgeometrics_ok: bg.ok,
            bgeometrics_failed: bg.failed,
            bgeometrics_rate_limited: bg.rate_limited,
            bgeometrics_skipped: bg.skipped,
            intermarket_symbols: self.intermarket_symbols.len(),
            intermarket_total_rows,
            btc_candles_total: self.btc_candles_total,
            has_critical_errors,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncSummary {
    pub sync_date: NaiveDate,
    pub coinmetrics_ok: usize,
    pub coinmetrics_failed: usize,
    /// Metrik yang ditolak karena kuota (429). Bukan error kritis — data lama tetap dipakai.
    pub coinmetrics_rate_limited: usize,
    pub bgeometrics_ok: usize,
    pub bgeometrics_failed: usize,
    /// Metrik yang ditolak karena kuota 10/jam (429). Bukan error kritis.
    pub bgeometrics_rate_limited: usize,
    pub bgeometrics_skipped: usize,
    pub intermarket_symbols: usize,
    pub intermarket_total_rows: usize,
    pub btc_candles_total: i64,
    pub has_critical_errors: bool,
}

// ---------------------------------------------------------------------------
// Async orchestration (butuh DB & jaringan)
// ---------------------------------------------------------------------------

/// Jalankan sinkronisasi penuh on-chain + intermarket.
/// - CoinMetrics: `fetch_full_history` dari `config.bootstrap_start` (tanpa API key).
/// - BGeometrics: 7 endpoint terverifikasi via `/v1/{slug}` (tanpa API key; key hanya
///   menaikkan tier). Metrik yang sudah lengkap & segar dilewati agar kuota 10/jam hemat.
/// - Yahoo: fetch standar 3 simbol (XAU, GSPC, DXY) range 5y.
/// Return `SyncReport` lengkap untuk logging / response API.
pub async fn run_full_sync(
    pool: &SqlitePool,
    config: &AppConfig,
    http: &Client,
) -> Result<SyncReport> {
    let sync_date = Utc::now().date_naive();
    let mut report = SyncReport {
        sync_date,
        coinmetrics_outcomes: Vec::new(),
        bgeometrics_outcomes: Vec::new(),
        intermarket_symbols: Vec::new(),
        intermarket_rows: Vec::new(),
        btc_candles_total: 0,
    };

    // ---- CoinMetrics (always available, no API key needed) ----
    let cm_client = build_cm_client(config, http.clone());
    match cm_client.fetch_full_history(pool, config.bootstrap_start).await {
        Ok(outcomes) => {
            report.coinmetrics_outcomes = outcomes;
        }
        Err(e) => {
            report.coinmetrics_outcomes.push(MetricFetchOutcome::failed(
                "coinmetrics_full",
                None,
                e.to_string(),
            ));
        }
    }

    // ---- BGeometrics (selalu jalan, tier gratis bekerja tanpa API key) ----
    let bg_client = BGeometricsClient::new(
        http.clone(),
        DEFAULT_BGEOMETRICS_BASE.to_string(),
        config.bgeometrics_api_key.clone(),
    )
    .with_safe_hourly_cap(config.bgeometrics_max_endpoints_per_day);

    // Status data yang sudah ada (untuk resumabilitas lintas jendela rate-limit)
    let counts = db::onchain_counts_by_source(pool, "bgeometrics").await?;
    let latest = db::latest_onchain_by_source(pool, "bgeometrics").await?;

    for (slug, metric) in bgeometrics_endpoints() {
        // Resumabilitas: jangan buang kuota 10/jam untuk metrik yang sudah lengkap & segar.
        let existing_rows = counts.get(metric).copied().unwrap_or(0);
        if decide_metric_sync(existing_rows, latest.get(metric).copied(), sync_date)
            == MetricSyncDecision::SkipFresh
        {
            report.bgeometrics_outcomes.push(MetricFetchOutcome::skipped(
                metric,
                format!("sudah lengkap ({existing_rows} baris) & segar"),
            ));
            continue;
        }

        // Hemat kuota: riwayat sudah lengkap → cukup ambil /last.
        // Sebelum perbaikan ini, SEMUA metrik memakai fetch_full_history setiap run
        // sehingga kuota 8/jam habis sebelum endpoint terakhir (lth_realized_price,
        // aviv, sth_mvrv) tersentuh — ketiganya tidak pernah tersimpan sama sekali.
        let mode = decide_fetch_mode(existing_rows);
        let fetch_result = match mode {
            MetricFetchMode::LatestOnly => bg_client.fetch_metric_last(pool, slug, metric).await,
            MetricFetchMode::FullHistory => bg_client.fetch_full_history(pool, slug, metric).await,
        };
        match fetch_result {
            Ok(out) => {
                let stop = out.status == FetchStatus::RateLimited || out.status == FetchStatus::Skipped;
                report.bgeometrics_outcomes.push(out);
                if stop {
                    break;
                }
            }
            Err(e) => {
                report.bgeometrics_outcomes.push(MetricFetchOutcome::failed(
                    metric,
                    None,
                    e.to_string(),
                ));
            }
        }
        // Jeda sopan antar request agar tidak memicu deteksi burst
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    // ---- Derivasi LTH Realized P/L Ratio (GATED, default OFF) ----
    // BGeometrics free-tier `realized_loss_lth` is unreliable during bull markets
    // (median $2,090 in 2025, 241/361 days < $100k), causing profit/loss ratios
    // of 100k-1M which break engine thresholds (>20 = TopZone).
    // Indicator is marked MISSING in engine. Re-enable only with reliable data source.
    if should_derive_lth_realized_pl_ratio() {
        if let (Ok(profit_series), Ok(loss_series)) = (
            db::fetch_onchain_series(pool, "realized_profit_lth").await,
            db::fetch_onchain_series(pool, "realized_loss_lth").await,
        ) {
            if !profit_series.is_empty() && !loss_series.is_empty() {
                let derived = derive_lth_realized_pl_ratio(&profit_series, &loss_series);
                if !derived.is_empty() {
                    crate::onchain::upsert_onchain_values(
                        pool,
                        "lth_realized_pl_ratio",
                        "bgeometrics_derived",
                        &derived
                            .into_iter()
                            .map(|(d, v)| crate::onchain::OnchainValue { date: d, value: v })
                            .collect::<Vec<_>>(),
                    )
                    .await?;
                }
            }
        }
    }

    // ---- Intermarket: Binance PAXG (gold) + FRED SP500 (S&P) + Yahoo DXY fallback ----
    // Yahoo Finance memblokir IP cloud (HTTP 429 dari VPS) → primary source diganti
    // ke endpoint publik yang tidak punya blocklist cloud:
    //   XAU  → Binance PAXGUSDT daily klines (1 PAXG = 1 troy oz LBMA gold, 24/7)
    //   GSPC → FRED series SP500 (CSV publik, tanpa API key)
    // Yahoo tetap dipakai sebagai fallback terakhir per simbol (best-effort).
    let intermarket = sync_intermarket(pool, http, true).await;
    for (symbol, rows, err) in &intermarket {
        report.intermarket_symbols.push(symbol.clone());
        report.intermarket_rows.push((symbol.clone(), *rows));
        if let Some(e) = err {
            tracing::warn!("intermarket {symbol} gagal: {e}");
        }
    }
    db::set_meta(
        pool,
        "last_intermarket_sync",
        &Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
    .await?;

    // ---- Total candle BTC untuk referensi ----
    let bounds = db::candle_bounds(pool).await?;
    report.btc_candles_total = bounds.count;

    Ok(report)
}

/// Sinkronisasi seluruh simbol intermarket dengan rantai primary → fallback.
///
/// Return: `(internal_symbol, rows_written, error_message)` per simbol.
/// Error tidak pernah ditelan diam-diam: selalu dikembalikan ke pemanggil agar
/// bisa dicatat ke `fetch_logs` dan dilaporkan transparan.
pub async fn sync_intermarket(
    pool: &SqlitePool,
    http: &Client,
    full_history: bool,
) -> Vec<(String, usize, Option<String>)> {
    let mut out: Vec<(String, usize, Option<String>)> = Vec::new();

    // --- XAU: Binance PAXGUSDT primary ---
    let paxg = crate::intermarket::binance_paxg::BinancePaxgClient::default_client(http.clone());
    let xau_result = if full_history {
        paxg.fetch_and_save_full_history(pool, None).await
    } else {
        paxg.fetch_and_save_incremental(pool, 30).await
    };
    match xau_result {
        Ok(rows) if rows > 0 => out.push(("XAU".into(), rows, None)),
        Ok(rows) => {
            // Nol baris dianggap gagal → coba fallback Yahoo
            tracing::warn!("Binance PAXG mengembalikan {rows} baris; fallback ke Yahoo GC=F");
            out.push(("XAU".into(), rows, Some("binance_paxg: 0 rows".into())));
        }
        Err(e) => {
            tracing::warn!("Binance PAXG gagal: {e}; fallback ke Yahoo GC=F");
            out.push(("XAU".into(), 0, Some(format!("binance_paxg: {e}"))));
        }
    }

    // --- GSPC: FRED SP500 primary ---
    let fred = crate::intermarket::fred::FredClient::default_client(http.clone());
    let spx_result = if full_history {
        fred.fetch_and_save_full_history(pool).await
    } else {
        fred.fetch_and_save_incremental(pool, Utc::now().date_naive() - chrono::Duration::days(45))
            .await
    };
    match spx_result {
        Ok(rows) if rows > 0 => out.push(("GSPC".into(), rows, None)),
        Ok(rows) => {
            tracing::warn!("FRED SP500 mengembalikan {rows} baris; fallback ke Yahoo ^GSPC");
            out.push(("GSPC".into(), rows, Some("fred: 0 rows".into())));
        }
        Err(e) => {
            tracing::warn!("FRED SP500 gagal: {e}; fallback ke Yahoo ^GSPC");
            out.push(("GSPC".into(), 0, Some(format!("fred: {e}"))));
        }
    }

    // --- DXY + fallback Yahoo (best-effort) ---
    let yahoo_client = crate::intermarket::yahoo::YahooFinanceClient::default_client(http.clone());
    let yahoo_range = if full_history { "5y" } else { "1mo" };

    // Fallback XAU/GSPC: hanya kalau primary gagal / nol baris.
    let xau_needs_fallback = out
        .iter()
        .find(|(s, _, _)| s == "XAU")
        .map(|(_, rows, err)| *rows == 0 || err.is_some())
        .unwrap_or(true);
    let gspc_needs_fallback = out
        .iter()
        .find(|(s, _, _)| s == "GSPC")
        .map(|(_, rows, err)| *rows == 0 || err.is_some())
        .unwrap_or(true);

    for (query, symbol, needs) in [
        ("GC=F", "XAU", xau_needs_fallback),
        ("^GSPC", "GSPC", gspc_needs_fallback),
        ("DX-Y.NYB", "DXY", true),
    ] {
        if !needs {
            continue;
        }
        match yahoo_client
            .fetch_and_save(pool, query, symbol, yahoo_range)
            .await
        {
            Ok(rows) => {
                if symbol == "DXY" {
                    out.push((symbol.into(), rows, None));
                } else if rows > 0 {
                    // fallback berhasil: perbarui entri
                    if let Some(entry) = out.iter_mut().find(|(s, _, _)| s == symbol) {
                        entry.1 = rows;
                    } else {
                        out.push((symbol.into(), rows, None));
                    }
                } else if let Some(entry) = out.iter_mut().find(|(s, _, _)| s == symbol) {
                    entry.2 = Some(format!("yahoo {query}: 0 rows"));
                }
            }
            Err(e) => {
                if symbol == "DXY" {
                    out.push((symbol.into(), 0, Some(format!("yahoo {query}: {e}"))));
                } else if let Some(entry) = out.iter_mut().find(|(s, _, _)| s == symbol) {
                    let prev = entry.2.clone().unwrap_or_default();
                    entry.2 = Some(format!("{prev}; yahoo {query}: {e}"));
                }
            }
        }
    }

    out
}

/// Jalankan sinkronisasi inkremental (harian, untuk cron).
/// - CoinMetrics: 30 hari terakhir.
/// - BGeometrics: endpoint yang belum fresh (cek staleness > 1 hari).
/// - Yahoo: 30 hari terakhir.
pub async fn run_incremental_sync(
    pool: &SqlitePool,
    config: &AppConfig,
    http: &Client,
) -> Result<SyncReport> {
    let sync_date = Utc::now().date_naive();
    let mut report = SyncReport {
        sync_date,
        coinmetrics_outcomes: Vec::new(),
        bgeometrics_outcomes: Vec::new(),
        intermarket_symbols: Vec::new(),
        intermarket_rows: Vec::new(),
        btc_candles_total: 0,
    };

    // CoinMetrics incremental (30 hari)
    let cm_client = build_cm_client(config, http.clone());
    let start = sync_date - chrono::Duration::days(30);
    match cm_client.fetch_full_history(pool, start).await {
        Ok(outcomes) => report.coinmetrics_outcomes = outcomes,
        Err(e) => report
            .coinmetrics_outcomes
            .push(MetricFetchOutcome::failed("coinmetrics_inc", None, &e.to_string())),
    }

    // BGeometrics incremental (cek staleness > 1 hari)
    let bg_client = BGeometricsClient::new(
        http.clone(),
        DEFAULT_BGEOMETRICS_BASE.to_string(),
        config.bgeometrics_api_key.clone(),
    )
    .with_safe_hourly_cap(config.bgeometrics_max_endpoints_per_day);

    // Cek staleness per metric on-chain dari BGeometrics
    let latest = db::latest_onchain_by_source(pool, "bgeometrics").await?;
    for (slug, metric) in bgeometrics_endpoints() {
        let stale = latest
            .get(metric)
            .map(|d| (sync_date - *d).num_days() > 1)
            .unwrap_or(true); // jika tidak ada data sama sekali, fetch

        if stale {
            let out = bg_client.fetch_metric_last(pool, slug, metric).await?;
            let stop = out.status == FetchStatus::RateLimited || out.status == FetchStatus::Skipped;
            report.bgeometrics_outcomes.push(out);
            if stop {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        } else {
            report.bgeometrics_outcomes
                .push(MetricFetchOutcome::skipped(metric, "fresh (< 1 day)"));
        }
    }

    // Intermarket incremental (Binance PAXG + FRED SP500 + Yahoo fallback)
    let intermarket = sync_intermarket(pool, http, false).await;
    for (symbol, rows, err) in &intermarket {
        report.intermarket_symbols.push(symbol.clone());
        report.intermarket_rows.push((symbol.clone(), *rows));
        if let Some(e) = err {
            tracing::warn!("intermarket inkremental {symbol} gagal: {e}");
        }
    }
    db::set_meta(
        pool,
        "last_intermarket_sync",
        &Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
    .await?;

    let bounds = db::candle_bounds(pool).await?;
    report.btc_candles_total = bounds.count;

    Ok(report)
}

// ---------------------------------------------------------------------------
// DB helpers yang dibutuhkan sync (akan diimplementasikan di db.rs)
// ---------------------------------------------------------------------------

// Catatan: fungsi `latest_onchain_by_source` ditambahkan ke `src/db.rs` di bawah.
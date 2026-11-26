//! Connector BGeometrics (`https://bitcoin-data.com/v1/*`).
//!
//! # Fakta Sumber (diverifikasi live 2026-11-25)
//!
//! 1. **Tanpa API key** (tier gratis) bekerja untuk endpoint `/v1/<metric>/last`
//!    maupun `/v1/<metric>` (historis).
//! 2. **Delay 7 hari** pada tier gratis — respons sengaja terlambat 1 minggu dari
//!    waktu nyata. Ini bukan bug; backend mencatat tanggal aktual data apa adanya.
//! 3. **Kuota keras per jam**: `X-RateLimit-Limit-Hour: 10`. Melebihi ini langsung
//!    menghasilkan HTTP 429 dengan header `X-RateLimit-Reset-Hour: <epoch>` dan
//!    body: `{"error":{"status":429,"code":"RATE_LIMIT_HOUR_EXCEEDED",...}}`.
//! 4. **Format respons**:
//!    - `/last`: objek tunggal `{"date":"2026-11-18", ...}`
//!    - historis: array objek `[{"date":"...", ...}, ...]`
//!    Sebagian nilai dikirim sebagai string numerik (misal `"48462.36"`).
//!
//! # Strategi Anti Rate-Limit
//!
//! - `safe_hourly_cap`: default **8 request/jam** (menyisakan margin 2 dari kuota 10).
//! - Sebelum menembak, periksa jumlah request di `fetch_logs` dalam 3600 detik terakhir.
//!   Jika sudah `>= safe_hourly_cap`, fetch **dibatalkan secara terstruktur**
//!   (status `skipped`), TIDAK menembak jaringan sama sekali, dan data SQLite lama
//!   dipertahankan.
//! - Jika server mengembalikan 429, catat `status='rate_limited'` dan simpan
//!   epoch reset pada kolom `message` agar antarmuka bisa menampilkan hitung mundur.

use super::{parse_date, pick_numeric_field, requests_in_window, FetchStatus, MetricFetchOutcome, OnchainValue};
use crate::market_data::log_fetch;
use crate::config::FetchLogEntry;
use anyhow::{Context, Result};
use reqwest::header::HeaderMap;
use reqwest::Client;
use sqlx::SqlitePool;

pub const DEFAULT_BGEOMETRICS_BASE: &str = "https://bitcoin-data.com/v1";

/// Daftar endpoint BGeometrics yang terverifikasi menghasilkan HTTP 200 pada 2026-11-25.
/// Dipetakan ke nama metric internal di tabel `onchain_daily`.
///
/// Urutan sengaja = prioritas kuota: metrik yang hanya tersedia dari BGeometrics
/// didahulukan agar tidak kehabisan budget 8/jam sebelum sampai ke sana.
/// `sth-realized-price` TIDAK diambil karena tidak dipakai engine (membuang kuota).
// - `realized-price` TIDAK diambil karena engine tidak membacanya (hanya lth_realized_price).
/// - MVRV polos juga tidak diambil — CoinMetrics Community menyediakannya dengan
///   delay 1 hari (bukan 7) dan riwayat penuh; lihat `src/onchain/coinmetrics.rs`.
///
/// **Baru (2026-11-26)**: ditambahkan 1 endpoint untuk mengisi indikator missing.
/// DIPERDAHULUKAN (urutan pertama) karena butuh FullHistory (riwayat belum ada),
/// sedangkan 5 metrik lama sudah punya riwayat → cukup LatestOnly.
/// Kuota per jam = 10 (provider) / 8 (safe cap); prioritas = FullHistory dulu.
/// - profit-loss → supply_in_profit_pct (langsung)
/// - realized_profit_lth + realized_loss_lth DIFETCH untuk audit,
///   TAPI lth_realized_pl_ratio DINONAKTIFKAN: penyebut free-tier kolaps
///   saat bull market. Lihat `LTH_REALIZED_PL_RATIO_ENABLED` di sync.rs.
pub const VERIFIED_METRICS: &[(&str, &str)] = &[
    ("profit-loss", "supply_in_profit_pct"),
    ("realized_profit_lth", "realized_profit_lth"),
    ("realized_loss_lth", "realized_loss_lth"),
    ("nupl", "nupl"),
    ("true-market-mean", "true_market_mean"),
    ("lth-realized-price", "lth_realized_price"),
    ("aviv", "aviv"),
    ("sth-mvrv", "sth_mvrv"),
];

pub struct BGeometricsClient {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    safe_hourly_cap: i64,
}

impl BGeometricsClient {
    pub fn new(client: Client, base_url: String, api_key: Option<String>) -> Self {
        Self {
            client,
            base_url,
            api_key,
            safe_hourly_cap: 8, // Margin 2 di bawah limit keras 10
        }
    }

    pub fn with_safe_hourly_cap(mut self, cap: i64) -> Self {
        self.safe_hourly_cap = cap;
        self
    }

    /// Ambil nilai terakhir satu metric BGeometrics.
    ///
    /// Menghormati rate-limit: jika kuota habis, mengembalikan
    /// `MetricFetchOutcome::skipped` tanpa membuka koneksi HTTP.
    pub async fn fetch_metric_last(
        &self,
        pool: &SqlitePool,
        endpoint_slug: &str,
        metric_name: &str,
    ) -> Result<MetricFetchOutcome> {
        // 1. Guard rate-limit per jam
        let used = requests_in_window(pool, "bgeometrics", 3600).await?;
        if used >= self.safe_hourly_cap {
            let msg = format!(
                "BGeometrics hourly budget reached ({used}/{}), call skipped to avoid 429 lockout",
                self.safe_hourly_cap
            );
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "bgeometrics".to_string(),
                    endpoint: format!("{endpoint_slug}/last"),
                    status: FetchStatus::Skipped.as_str().to_string(),
                    http_code: None,
                    rows: 0,
                    message: Some(msg.clone()),
                },
            )
            .await?;
            return Ok(MetricFetchOutcome::skipped(metric_name, msg));
        }

        // 2. Siapkan request
        let url = format!("{}/{}/last", self.base_url.trim_end_matches('/'), endpoint_slug);
        let mut req = self.client.get(&url).header("User-Agent", "btc-cycle-radar/0.1");
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        // 3. Eksekusi
        let res = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("network error: {e}");
                log_fetch(
                    pool,
                    &FetchLogEntry {
                        source: "bgeometrics".to_string(),
                        endpoint: format!("{endpoint_slug}/last"),
                        status: FetchStatus::Error.as_str().to_string(),
                        http_code: None,
                        rows: 0,
                        message: Some(msg.clone()),
                    },
                )
                .await?;
                return Ok(MetricFetchOutcome::failed(metric_name, None, msg));
            }
        };

        let status = res.status();
        let headers = res.headers().clone();

        // 4. Deteksi 429
        if status.as_u16() == 429 {
            let reset_epoch = extract_reset_epoch(&headers);
            let msg = format!(
                "429 RATE_LIMIT_HOUR_EXCEEDED; reset_epoch={}",
                reset_epoch.unwrap_or(0)
            );
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "bgeometrics".to_string(),
                    endpoint: format!("{endpoint_slug}/last"),
                    status: FetchStatus::RateLimited.as_str().to_string(),
                    http_code: Some(429),
                    rows: 0,
                    message: Some(msg.clone()),
                },
            )
            .await?;
            return Ok(MetricFetchOutcome {
                metric: metric_name.to_string(),
                status: FetchStatus::RateLimited,
                rows: 0,
                http_code: Some(429),
                message: Some(msg),
            });
        }

        // 5. Non-200 error
        if !status.is_success() {
            let body_preview = res.text().await.unwrap_or_default();
            let msg = format!("HTTP {status}: {}", &body_preview[..body_preview.len().min(120)]);
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "bgeometrics".to_string(),
                    endpoint: format!("{endpoint_slug}/last"),
                    status: FetchStatus::Error.as_str().to_string(),
                    http_code: Some(status.as_u16() as i32),
                    rows: 0,
                    message: Some(msg.clone()),
                },
            )
            .await?;
            return Ok(MetricFetchOutcome::failed(metric_name, Some(status.as_u16()), msg));
        }

        // 6. Parse JSON
        let text = res.text().await.context("read bgeometrics response")?;
        let parsed = parse_bgeometrics_payload(&text, endpoint_slug);
        let rows = parsed.len();

        if rows == 0 {
            let msg = "empty or unparseable JSON from bgeometrics".to_string();
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "bgeometrics".to_string(),
                    endpoint: format!("{endpoint_slug}/last"),
                    status: FetchStatus::Error.as_str().to_string(),
                    http_code: Some(200),
                    rows: 0,
                    message: Some(msg.clone()),
                },
            )
            .await?;
            return Ok(MetricFetchOutcome::failed(metric_name, Some(200), msg));
        }

        // 7. Simpan ke database
        super::upsert_onchain_values(pool, metric_name, "bgeometrics", &parsed).await?;

        log_fetch(
            pool,
            &FetchLogEntry {
                source: "bgeometrics".to_string(),
                endpoint: format!("{endpoint_slug}/last"),
                status: FetchStatus::Ok.as_str().to_string(),
                http_code: Some(200),
                rows: rows as i64,
                message: None,
            },
        )
        .await?;

        Ok(MetricFetchOutcome::ok(metric_name, rows))
    }

    /// Ambil seluruh kumpulan metric terverifikasi yang belum lewat batas kuota jam ini.
    /// Berhenti segera setelah kuota sisa habis agar tidak memicu 429.
    pub async fn fetch_all_verified(
        &self,
        pool: &SqlitePool,
    ) -> Result<Vec<MetricFetchOutcome>> {
        let mut outcomes = Vec::new();
        for (slug, metric) in VERIFIED_METRICS {
            let out = self.fetch_metric_last(pool, slug, metric).await?;
            let should_stop = out.status == FetchStatus::RateLimited || out.status == FetchStatus::Skipped;
            outcomes.push(out);
            if should_stop {
                // Jangan paksa endpoint berikutnya kalau sudah mentok
                break;
            }
            // Beri jeda kecil antar-request
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        Ok(outcomes)
    }

    /// Ambil riwayat penuh (historis) satu metric dari endpoint `/v1/{slug}` (array).
    /// Mengembalikan `MetricFetchOutcome` dengan `rows` = jumlah baris disimpan.
    pub async fn fetch_full_history(
        &self,
        pool: &SqlitePool,
        endpoint_slug: &str,
        metric_name: &str,
    ) -> Result<MetricFetchOutcome> {
        // 1. Guard rate-limit per jam
        let used = requests_in_window(pool, "bgeometrics", 3600).await?;
        if used >= self.safe_hourly_cap {
            let msg = format!(
                "BGeometrics hourly budget reached ({used}/{}), call skipped to avoid 429 lockout",
                self.safe_hourly_cap
            );
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "bgeometrics".to_string(),
                    endpoint: format!("{endpoint_slug} (full history)"),
                    status: FetchStatus::Skipped.as_str().to_string(),
                    http_code: None,
                    rows: 0,
                    message: Some(msg.clone()),
                },
            )
            .await?;
            return Ok(MetricFetchOutcome::skipped(metric_name, msg));
        }

        // 2. Siapkan request ke endpoint historis (bukan /last)
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), endpoint_slug);
        let mut req = self.client.get(&url).header("User-Agent", "btc-cycle-radar/0.1");
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        // 3. Eksekusi
        let res = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("network error: {e}");
                log_fetch(
                    pool,
                    &FetchLogEntry {
                        source: "bgeometrics".to_string(),
                        endpoint: format!("{endpoint_slug} (full history)"),
                        status: FetchStatus::Error.as_str().to_string(),
                        http_code: None,
                        rows: 0,
                        message: Some(msg.clone()),
                    },
                )
                .await?;
                return Ok(MetricFetchOutcome::failed(metric_name, None, msg));
            }
        };

        let status = res.status();
        let headers = res.headers().clone();

        // 4. Deteksi 429
        if status.as_u16() == 429 {
            let reset_epoch = extract_reset_epoch(&headers);
            let msg = format!(
                "429 RATE_LIMIT_HOUR_EXCEEDED; reset_epoch={}",
                reset_epoch.unwrap_or(0)
            );
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "bgeometrics".to_string(),
                    endpoint: format!("{endpoint_slug} (full history)"),
                    status: FetchStatus::RateLimited.as_str().to_string(),
                    http_code: Some(429),
                    rows: 0,
                    message: Some(msg.clone()),
                },
            )
            .await?;
            return Ok(MetricFetchOutcome {
                metric: metric_name.to_string(),
                status: FetchStatus::RateLimited,
                rows: 0,
                http_code: Some(429),
                message: Some(msg),
            });
        }

        // 5. Non-200 error
        if !status.is_success() {
            let body_preview = res.text().await.unwrap_or_default();
            let msg = format!("HTTP {status}: {}", &body_preview[..body_preview.len().min(120)]);
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "bgeometrics".to_string(),
                    endpoint: format!("{endpoint_slug} (full history)"),
                    status: FetchStatus::Error.as_str().to_string(),
                    http_code: Some(status.as_u16() as i32),
                    rows: 0,
                    message: Some(msg.clone()),
                },
            )
            .await?;
            return Ok(MetricFetchOutcome::failed(metric_name, Some(status.as_u16()), msg));
        }

        // 6. Parse JSON array
        let text = res.text().await.context("read bgeometrics full history response")?;
        let parsed = parse_bgeometrics_payload(&text, endpoint_slug);
        let rows = parsed.len();

        if rows == 0 {
            let msg = "empty or unparseable JSON from bgeometrics full history".to_string();
            log_fetch(
                pool,
                &FetchLogEntry {
                    source: "bgeometrics".to_string(),
                    endpoint: format!("{endpoint_slug} (full history)"),
                    status: FetchStatus::Error.as_str().to_string(),
                    http_code: Some(200),
                    rows: 0,
                    message: Some(msg.clone()),
                },
            )
            .await?;
            return Ok(MetricFetchOutcome::failed(metric_name, Some(200), msg));
        }

        // 7. Simpan ke database
        super::upsert_onchain_values(pool, metric_name, "bgeometrics", &parsed).await?;

        log_fetch(
            pool,
            &FetchLogEntry {
                source: "bgeometrics".to_string(),
                endpoint: format!("{endpoint_slug} (full history)"),
                status: FetchStatus::Ok.as_str().to_string(),
                http_code: Some(200),
                rows: rows as i64,
                message: None,
            },
        )
        .await?;

        Ok(MetricFetchOutcome::ok(metric_name, rows))
    }
}

/// Ekstrak epoch reset dari header `X-RateLimit-Reset-Hour`.
fn extract_reset_epoch(headers: &HeaderMap) -> Option<i64> {
    headers
        .get("x-ratelimit-reset-hour")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
}

/// Parser serbaguna untuk respons BGeometrics — menangani baik array maupun objek tunggal.
/// Menerapkan transform khusus per endpoint:
/// - `profit-loss`: value adalah fraksi 0-1 → dikali 100 untuk persen.
///   KEBIJAKAN DATA: nilai 1.0 (100%) dikirim BGeometrics secara PINNED untuk
///   rentang panjang (2022–2026) → artefak provider. Supply in profit tidak
///   pernah tepat 100% (selalu ada UTXO di bawah harga akuisisi). Baris dengan
///   `val <= 0.0`, `val >= 1.0`, atau non-finite DIBUANG (bukan dicap), agar
///   tidak mengarang data. Hanya baris plausibel (0 < x < 1) yang lolos.
/// - `realized_loss_lth`: value negatif (kerugian) → disimpan sebagai absolut.
pub fn parse_bgeometrics_payload(body: &str, endpoint_slug: &str) -> Vec<OnchainValue> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    match v {
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Some(ov) = parse_single_item(&item, endpoint_slug) {
                    out.push(ov);
                }
            }
        }
        serde_json::Value::Object(_) => {
            if let Some(ov) = parse_single_item(&v, endpoint_slug) {
                out.push(ov);
            }
        }
        _ => {}
    }
    // Jaminan urut menaik: pipeline hilir (find_closest_before, MA, quantile) mengasumsikan
    // deret kronologis, sementara respons API tidak menjamin urutannya.
    out.sort_by_key(|v| v.date);
    out
}

fn parse_single_item(item: &serde_json::Value, endpoint_slug: &str) -> Option<OnchainValue> {
    let obj = item.as_object()?;

    // Tanggal bisa bernama "date", "timestamp", "time", atau kunci pendek "d"
    // (format padat yang dipakai sebagian mirror BGeometrics).
    let date_str = obj
        .get("date")
        .or_else(|| obj.get("timestamp"))
        .or_else(|| obj.get("time"))
        .or_else(|| obj.get("d"))
        .and_then(|v| v.as_str())?;
    let date = parse_date(date_str)?;

    // Kandidat nama field numerik (slug + fallback generik)
    let mut candidates = vec![
        endpoint_slug,
        "value",
        "val",
        "v",
        "price",
        "close",
    ];
    // Tambahkan camelCase variant dari slug (profit-loss → profitLoss, realized_profit_lth → realizedProfitLth)
    if let Some(camel) = slug_to_camel_case(endpoint_slug) {
        candidates.push(camel);
    }

    let val = pick_numeric_field(obj, &candidates)?;

    // Transform khusus endpoint
    let val = match endpoint_slug {
        "profit-loss" => {
            // Plausibilitas: supply in profit adalah FRAKSI 0..1 dan tidak pernah
            // tepat 1.0 (selalu ada supply yang berada di bawah harga akuisisi).
            // BGeometrics mengirim 1.0 secara pinned pada rentang historis panjang
            // → artefak provider. Buang baris semacam itu (jangan dicap 100%,
            // karena cap = mengarang nilai).
            if !val.is_finite() || val <= 0.0 || val >= 1.0 {
                return None;
            }
            val * 100.0
        }
        "realized_loss_lth" => val.abs(),                // kerugian negatif → absolut
        _ => val,
    };

    Some(OnchainValue { date, value: val })
}

/// Konversi slug kebab-case/snake_case ke camelCase (profit-loss → profitLoss).
/// Tidak dijamin benar 100% untuk semua slug, tapi cukup untuk endpoint BGeometrics kami.
fn slug_to_camel_case(slug: &str) -> Option<&'static str> {
    // Hardcoded untuk endpoint yang kita pakai agar benar pasti.
    // Jika slug tidak dikenal, kembalikan None → fallback di pick_numeric_field.
    match slug {
        "profit-loss" => Some("profitLoss"),
        "realized_profit_lth" => Some("realizedProfitLth"),
        "realized_loss_lth" => Some("realizedLossLth"),
        _ => None,
    }
}

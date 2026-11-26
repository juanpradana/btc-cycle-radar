//! HTTP API Phase 1: healthz, meta, bootstrap & update trigger.
//!
//! Prinsip: tidak ada `unwrap()` pada path runtime. Kegagalan DB/network
//! dikembalikan sebagai respons JSON terstruktur dengan status code yang tepat.
use crate::config::AppConfig;
use crate::db;
use crate::engine::loader::LoaderError;
use crate::engine::service::{compute_valuation, list_indicators};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub cfg: AppConfig,
    pub http: reqwest::Client,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/meta", get(meta))
        // Phase 2: Valuation & Indicators API (GET-only, read-only)
        .route("/api/v1/valuation", get(get_valuation))
        .route("/api/v1/indicators", get(get_indicators))
        // POST-only: operasi ini memicu I/O jaringan ke sumber eksternal.
        .route("/api/v1/bootstrap", post(trigger_bootstrap))
        .route("/api/v1/update", post(trigger_update))
        // Sinkronisasi on-chain + intermarket. Body opsional:
        //   { "mode": "full" | "incremental" }  — default "incremental"
        .route("/api/v1/sync", post(trigger_sync))
        .with_state(state)
}

/// Hanya nama file DB yang di-expose (jangan bocorkan path absolut host).
fn database_label(database_url: &str) -> String {
    database_url
        .trim_start_matches("sqlite:")
        .trim_start_matches("//")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(database_url)
        .to_string()
}

async fn healthz() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "service": "btc-cycle-radar",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Ringkasan metadata DB untuk UI / monitoring.
/// Kalau query DB gagal → 503 dengan pesan transparan (bukan data palsu).
async fn meta(State(state): State<Arc<AppState>>) -> Response {
    let bounds = match db::candle_bounds(&state.pool).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "ok": false,
                    "error": format!("database unavailable: {e}"),
                })),
            )
                .into_response();
        }
    };

    Json(json!({
        "ok": true,
        "candle_count": bounds.count,
        "first_date": bounds.first_date,
        "last_date": bounds.last_date,
        "min_required": state.cfg.min_required_candles,
        "bootstrap_ok": bounds.count >= state.cfg.min_required_candles,
        "database": database_label(&state.cfg.database_url),
        "coinmetrics_base_url": state.cfg.coinmetrics_base_url,
        "binance_base_urls": state.cfg.binance_base_urls,
        "has_bgeometrics_key": state.cfg.bgeometrics_api_key.is_some(),
        "has_fred_key": state.cfg.fred_api_key.is_some(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// Trigger bootstrap manual via HTTP (mis. dari UI/admin).
async fn trigger_bootstrap(State(state): State<Arc<AppState>>) -> Response {
    match crate::bootstrap::bootstrap_history(&state.pool, &state.cfg, &state.http, true).await {
        Ok(report) => (StatusCode::OK, Json(json!({ "ok": true, "report": report }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Trigger update harian manual (klines + spot price).
async fn trigger_update(State(state): State<Arc<AppState>>) -> Response {
    match crate::bootstrap::update_recent(&state.pool, &state.cfg, &state.http, 5).await {
        Ok(report) => (StatusCode::OK, Json(json!({ "ok": true, "report": report }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Body request untuk POST /api/v1/sync.
#[derive(serde::Deserialize, Default)]
struct SyncRequest {
    #[serde(default)]
    mode: Option<String>,
}

/// Trigger sinkronisasi on-chain (BGeometrics + CoinMetrics) & intermarket (Yahoo).
/// Tidak ada `unwrap()` di path ini: error jaringan/DB → JSON terstruktur.
async fn trigger_sync(
    State(state): State<Arc<AppState>>,
    body: Option<Json<SyncRequest>>,
) -> Response {
    let mode = body
        .and_then(|Json(b)| b.mode)
        .unwrap_or_else(|| "incremental".to_string())
        .to_ascii_lowercase();

    let result = match mode.as_str() {
        "full" => crate::sync::run_full_sync(&state.pool, &state.cfg, &state.http).await,
        "incremental" | "inc" => {
            crate::sync::run_incremental_sync(&state.pool, &state.cfg, &state.http).await
        }
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": format!("mode tidak dikenal: {other} (gunakan \"full\" atau \"incremental\")"),
                })),
            )
                .into_response();
        }
    };

    match result {
        Ok(report) => {
            let summary = report.summary();
            let status = if summary.has_critical_errors {
                StatusCode::PARTIAL_CONTENT
            } else {
                StatusCode::OK
            };
            (status, Json(json!({ "ok": !summary.has_critical_errors, "summary": summary, "report": report })))
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Query params untuk endpoint GET /api/v1/valuation & /api/v1/indicators.
#[derive(Deserialize, Default)]
struct ValuationQuery {
    /// Tanggal evaluasi (YYYY-MM-DD). Opsional — default hari terakhir di DB.
    as_of: Option<String>,
}

/// GET /api/v1/valuation — evaluasi valuasi lengkap (19 indikator + komposit + zone + provenance).
///
/// Tidak ada `unwrap()` di path ini: seluruh kegagalan loader dipetakan ke
/// status code + `error_code` yang eksplisit.
async fn get_valuation(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ValuationQuery>,
) -> Response {
    let as_of = match parse_as_of(q.as_of) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    match compute_valuation(&state.pool, as_of, state.cfg.min_required_candles).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => loader_error_response(e),
    }
}

/// GET /api/v1/indicators — registry 19 indikator + bobot efektif + hasil evaluasi terakhir.
async fn get_indicators(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ValuationQuery>,
) -> Response {
    let as_of = match parse_as_of(q.as_of) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    match list_indicators(&state.pool, as_of, state.cfg.min_required_candles).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => loader_error_response(e),
    }
}

/// Parse query `as_of` (YYYY-MM-DD). `Err` berisi respons 400 siap-kirim.
fn parse_as_of(raw: Option<String>) -> Result<Option<NaiveDate>, Response> {
    match raw {
        None => Ok(None),
        Some(s) => match NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
            Ok(d) => Ok(Some(d)),
            Err(_) => Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error_code": "invalid_date",
                    "error": format!("format tanggal tidak valid: {s} (gunakan YYYY-MM-DD)"),
                })),
            )
                .into_response()),
        },
    }
}

/// Petakan `LoaderError` ke respons HTTP yang transparan (tanpa data palsu).
fn loader_error_response(err: LoaderError) -> Response {
    let (status, code, message) = match &err {
        LoaderError::NoCandles => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no_candles",
            "tidak ada candle OHLCV di database".to_string(),
        ),
        LoaderError::InsufficientCoverage { available, required } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "insufficient_coverage",
            format!("candle tidak mencukupi: {available} < {required}"),
        ),
        LoaderError::DateOutOfRange { requested, first, last } => (
            StatusCode::BAD_REQUEST,
            "date_out_of_range",
            format!("tanggal {requested} di luar rentang data: {first} s.d. {last}"),
        ),
        LoaderError::NoCandleForDate(d) => (
            StatusCode::NOT_FOUND,
            "no_candle_for_date",
            format!("tidak ada candle OHLCV untuk tanggal {d}"),
        ),
        LoaderError::InvalidPrice { date, value } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_price",
            format!("harga close tidak valid pada {date} (nilai: {value})"),
        ),
        LoaderError::Db(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "database_error",
            format!("gagal membaca database: {e}"),
        ),
    };

    let mut body = json!({
        "ok": false,
        "error_code": code,
        "error": message,
    });

    match &err {
        LoaderError::InsufficientCoverage { available, required } => {
            body["available"] = json!(available);
            body["required"] = json!(required);
        }
        LoaderError::DateOutOfRange { requested, first, last } => {
            body["requested"] = json!(requested.to_string());
            body["first"] = json!(first.to_string());
            body["last"] = json!(last.to_string());
        }
        _ => {}
    }

    (status, Json(body)).into_response()
}

use btc_cycle_radar::{api, bootstrap, config::AppConfig, db};
use axum::serve;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ----- CLI subcommand handling -----
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 {
        return match args[1].as_str() {
            "bootstrap" => {
                let force = args.get(2).map(|s| s == "--force" || s == "-f").unwrap_or(false);
                let verify = args.get(2).map(|s| s == "--verify" || s == "-v").unwrap_or(false);
                run_bootstrap_cli(force, verify).await
            }
            "update" => {
                let days = args.get(2).and_then(|s| s.parse::<i64>().ok()).unwrap_or(5);
                run_update_cli(days).await
            }
            "sync" => {
                let incremental = args
                    .get(2)
                    .map(|s| s == "--incremental" || s == "--inc")
                    .unwrap_or(false);
                run_sync_cli(incremental).await
            }
            "migrate" => {
                run_migrate_cli().await
            }
            _ => {
                eprintln!(
                    "Usage: {} [bootstrap [--force|--verify] | update [days] | sync [--incremental] | migrate]",
                    args[0]
                );
                std::process::exit(1);
            }
        };
    }

    // ----- Server mode -----
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("btc_cycle_radar=debug".parse()?))
        .init();

    let cfg = AppConfig::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;
    let http = reqwest::Client::new();

    // Auto-bootstrap on startup (bisa dimatikan via AUTO_BOOTSTRAP=false)
    let auto_bootstrap = std::env::var("AUTO_BOOTSTRAP")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);
    if auto_bootstrap {
        tracing::info!("auto-bootstrap enabled");
        match crate::bootstrap::bootstrap_history(&pool, &cfg, &http, false).await {
            Ok(r) => tracing::info!("{}", r),
            Err(e) => tracing::error!("auto-bootstrap failed: {}", e),
        }
    }

    let state = Arc::new(btc_cycle_radar::api::AppState { pool, cfg: cfg.clone(), http });

    let mut app = api::router(state.clone());

    // Serve static frontend jika dikonfigurasi
    if let Some(dir) = &cfg.static_dir {
        if dir.exists() {
            app = app.fallback_service(ServeDir::new(dir).append_index_html_on_directories(true));
            tracing::info!("serving static files from {}", dir.display());
        }
    }

    let addr: SocketAddr = cfg.bind_addr.parse()?;
    tracing::info!("listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("signal handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

async fn run_bootstrap_cli(force: bool, verify: bool) -> anyhow::Result<()> {
    let cfg = AppConfig::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;
    let http = reqwest::Client::new();

    if verify {
        // Hanya verifikasi count >= min_required_candles
        let bounds = db::candle_bounds(&pool).await?;
        println!("Candles: {} (required: {})", bounds.count, cfg.min_required_candles);
        if bounds.count >= cfg.min_required_candles {
            println!("✅ PASS");
            std::process::exit(0);
        } else {
            println!("❌ FAIL");
            std::process::exit(1);
        }
    }

    let report = crate::bootstrap::bootstrap_history(&pool, &cfg, &http, force).await?;
    println!("{}", report);
    if report.candles_after < cfg.min_required_candles {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_update_cli(days: i64) -> anyhow::Result<()> {
    let cfg = AppConfig::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;
    let http = reqwest::Client::new();
    let report = crate::bootstrap::update_recent(&pool, &cfg, &http, days).await?;
    println!("Update report: {:?}", report);
    Ok(())
}

async fn run_migrate_cli() -> anyhow::Result<()> {
    let cfg = AppConfig::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;
    db::run_migrations(&pool).await?;
    println!("Migrations applied successfully");
    Ok(())
}

async fn run_sync_cli(incremental: bool) -> anyhow::Result<()> {
    let cfg = AppConfig::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;
    let http = reqwest::Client::new();

    let report = if incremental {
        btc_cycle_radar::sync::run_incremental_sync(&pool, &cfg, &http).await?
    } else {
        btc_cycle_radar::sync::run_full_sync(&pool, &cfg, &http).await?
    };

    println!("{}", serde_json::to_string_pretty(&report.summary())?);
    if report.summary().has_critical_errors {
        std::process::exit(1);
    }
    Ok(())
}
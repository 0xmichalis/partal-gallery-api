mod api;
mod db;
mod logging;
mod router;

use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{error, info};

use crate::db::Db;
use crate::logging::{should_enable_color, LogLevel};
use crate::router::{build_router, AppState};

#[derive(Parser, Debug)]
#[command(author, version, about = "Gallery storage API for Partal (a self-hosted replacement for Supabase).", long_about = None)]
struct Args {
    /// The address to listen on
    #[arg(long, default_value = "127.0.0.1:8091")]
    listen_address: String,

    /// Set the log level
    #[arg(short, long, value_enum, default_value = "info")]
    log_level: LogLevel,

    /// Maximum number of Postgres connections in the pool
    #[arg(long, default_value_t = 5)]
    max_db_connections: u32,

    /// Disable colored log output. NO_COLOR and FORCE_COLOR take precedence.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    no_color: bool,
}

#[tokio::main]
async fn main() {
    // Config comes from both the environment and the command line.
    dotenvy::dotenv().ok();
    let args = Args::parse();
    logging::init(args.log_level, should_enable_color(args.no_color));
    info!(
        "Version: {} {} (commit {})",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("GIT_COMMIT")
    );
    info!("Initializing server with options: {:?}", args);

    let database_url = require_env("DATABASE_URL");
    let auth_token = require_env("GALLERY_AUTH_TOKEN");

    let db = Db::new(&database_url, args.max_db_connections).await;
    let state = AppState {
        db: Arc::new(db),
        auth_token: Arc::new(auth_token),
    };

    let app = build_router(state);

    let listener = match TcpListener::bind(&args.listen_address).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind {}: {}", args.listen_address, e);
            std::process::exit(1);
        }
    };
    info!("Listening on {}", args.listen_address);

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        error!("Server error: {}", e);
        std::process::exit(1);
    }
    info!("Server shut down gracefully");
}

/// Read a required, non-empty environment variable or exit with a clear error.
fn require_env(key: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => {
            error!("{key} must be set and non-empty");
            std::process::exit(1);
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl+C, shutting down"),
        _ = terminate => info!("Received SIGTERM, shutting down"),
    }
}

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::Router;
use clap::Parser;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod api;
mod blacklist;
mod canonical;
mod catalog;
mod codec;
mod config;
mod default_order;
mod epg;
mod hosts;
mod measured;
mod play_log;
mod play_sessions;
mod probe;
mod proxy;
mod radio;
mod sps;
mod state;
mod xtream;

use crate::catalog::spawn_catalog_loop;
use crate::config::Config;
use crate::hosts::spawn_probe_loop;
use crate::state::AppState;

#[derive(Parser, Debug)]
#[command(name = "iptv-proxy", about = "Self-hosted IPTV proxy and aggregator")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,iptv_proxy=debug"));
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(filter)
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config).context("loading config")?;
    let listen: SocketAddr = config.listen_addr.parse().context("parsing listen_addr")?;
    info!("starting iptv-proxy on {listen} ({} hosts)", config.xtream.hosts.len());

    let state = AppState::new(config.clone())?;

    spawn_probe_loop(
        Arc::clone(&state.hosts),
        state.xtream.clone(),
        config.probe.clone(),
        config.xtream.hosts.clone(),
        Arc::clone(&state.max_connections),
    );

    spawn_catalog_loop(
        Arc::clone(&state.catalog),
        Arc::clone(&state.hosts),
        state.xtream.clone(),
        config.catalog.clone(),
        Arc::clone(&state.curation),
        Arc::clone(&state.radio_curation),
        config.radio.clone(),
    );

    // Background tasks for the measured-quality system. All three run for
    // the lifetime of the server and use cancel-safe primitives.
    tokio::spawn(measured::run_flush_task(Arc::clone(&state.measured)));
    tokio::spawn(
        Arc::clone(&state.per_play).run_committer(Arc::clone(&state.measured)),
    );
    probe::spawn_bootstrap_sweep(state.clone());

    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!("listening");
    axum::serve(listener, app.into_make_service())
        .await
        .context("axum serve")?;
    Ok(())
}

fn router(state: Arc<AppState>) -> Router {
    let ui_dir = state.config.ui_dir.clone();
    info!("serving UI from {}", ui_dir.display());
    Router::new()
        .route("/", get(api::serve_index))
        .route("/index.html", get(api::serve_index))
        .route("/api/channels", get(api::list_channels))
        .route("/api/epg/:key", get(api::get_epg))
        .route("/api/status", get(api::status))
        .route("/api/feedback/:key", post(api::feedback))
        .route("/admin/reprobe", post(api::admin_reprobe))
        .route("/admin/clear-blacklist", post(api::admin_clear_blacklist))
        .route("/admin/clear-demoted", post(api::admin_clear_demoted))
        .route("/admin/clear-classifier", post(api::admin_clear_classifier))
        .route("/admin/clear-all", post(api::admin_clear_all))
        .route("/admin/recent-plays", get(api::admin_recent_plays))
        .route("/admin/measured-quality", get(api::admin_measured_quality))
        .route("/api/probe/audio.m3u8", get(api::probe_audio))
        .route("/api/probe/video.m3u8", get(api::probe_video))
        .route("/play/:name", get(proxy::play_playlist))
        .route("/seg/:token", get(proxy::proxy_segment))
        .fallback_service(ServeDir::new(ui_dir))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any))
        // Gzip / Brotli for text payloads. Skips /play/, /seg/ (HLS/TS segments are
        // already compressed), and respects client Accept-Encoding. Hits hls.min.js
        // (~400 KB → ~120 KB), main.js (~70 KB), app.css (~32 KB), and
        // /api/channels JSON (~100 KB for 400 channels) — wire-size reductions on
        // every cold boot, no client changes required.
        .layer(CompressionLayer::new().gzip(true).br(true))
}

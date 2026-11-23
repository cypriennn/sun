mod auth;
mod config;
mod domain;
mod error;
mod files;
mod llm;
mod markdown;
mod mcp;
mod schedule;
mod state;
mod storage;
mod tools;
mod web;

use crate::{
    config::{Config, Secrets},
    state::AppState,
};
use std::path::PathBuf;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = dotenvy::dotenv();

    let config_path = std::env::var("SUN_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));
    let config = Config::load_sync(&config_path)?;
    let bind_addr = std::env::var("SUN_BIND").unwrap_or_else(|_| config.server.bind.clone());
    let secrets = Secrets::from_env();

    let state = AppState::new(config_path, config, secrets)?;
    state.refresh_discovered_models().await;
    state.spawn_config_watcher();

    let listener = TcpListener::bind(&bind_addr).await?;
    tokio::spawn(schedule::Scheduler::run(state.clone()));
    axum::serve(listener, web::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

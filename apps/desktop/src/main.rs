mod app;
mod home;
mod session_basic;
mod status_bar;
mod theme;

use std::path::PathBuf;
use std::sync::Arc;

use aw_claude::ClaudeProvider;
use aw_core::AgentProvider;
use aw_runtime::SessionManager;
use aw_store::{StoreReader, StoreWriter};

use crate::app::App;

/// `AW_DATA_DIR` overrides the platform data dir (tests, throwaway runs).
fn data_dir() -> PathBuf {
    std::env::var_os("AW_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("agent-workspace")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let db = dir.join("workspace.db");
    tracing::info!(path = %db.display(), "opening store");
    let writer = StoreWriter::spawn(&db)?;
    let reader = StoreReader::open(&db)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let provider: Arc<dyn AgentProvider> = Arc::new(ClaudeProvider::new());
    let preflight = provider.preflight().err().map(|e| e.to_string());
    if let Some(e) = &preflight {
        tracing::warn!(error = %e, "provider preflight failed");
    }
    let providers = vec![provider];
    let (cmd_tx, event_rx) = {
        let _guard = rt.enter();
        SessionManager::start(providers.clone(), writer.clone(), reader.clone())
    };

    let handle = rt.handle().clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Agent Workspace")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Agent Workspace",
        options,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                cc, handle, cmd_tx, event_rx, providers, writer, reader, preflight,
            )))
        }),
    )?;
    Ok(())
}

mod app;
mod chrome;
mod composer;
mod config;
mod conversation;
mod diff;
mod editor;
mod home;
mod icons;
mod keymap;
mod running;
mod session_view;
mod status_bar;
mod theme;
mod widgets;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use xade_claude::ClaudeProvider;
use xade_core::AgentProvider;
use xade_runtime::SessionManager;
use xade_store::{StoreReader, StoreWriter};

use crate::app::App;

/// Smallest main window that still fits the top bar without overlap.
pub const MIN_WINDOW_SIZE: [f32; 2] = [1280.0, 760.0];

/// `XADE_DATA_DIR` overrides the platform data dir (tests, throwaway runs).
fn data_dir() -> PathBuf {
    std::env::var_os("XADE_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("xade")))
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

    let config_path = config::path(&dir);
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
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Xade")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size(MIN_WINDOW_SIZE);
    match eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/xade-icon-app.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => tracing::warn!("app icon not loaded: {e}"),
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Xade",
        options,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                cc,
                handle,
                cmd_tx,
                event_rx,
                providers,
                writer,
                reader,
                preflight,
                config_path,
            )))
        }),
    )?;
    Ok(())
}

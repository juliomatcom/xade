mod app;
mod attach;
mod chrome;
mod composer;
mod config;
mod conversation;
mod diff;
mod editor;
mod errors;
mod file_tree;
mod find;
mod home;
mod icons;
mod keymap;
mod logging;
mod mention;
mod running;
mod send_anim;
mod session_view;
mod status_bar;
mod theme;
mod turn_files;
mod widgets;
mod windows_state;
mod workspace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xade_antigravity::AntigravityProvider;
use xade_claude::ClaudeProvider;
use xade_core::{AgentProvider, ProviderError, ProviderId};
use xade_runtime::SessionManager;
use xade_store::{StoreReader, StoreWriter};

pub use crate::app::App;

/// Smallest main window that still fits the top bar without overlap.
pub const MIN_WINDOW_SIZE: [f32; 2] = [1280.0, 760.0];

/// `XADE_DATA_DIR` overrides the platform data dir (tests, throwaway runs).
fn data_dir() -> PathBuf {
    std::env::var_os("XADE_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("xade")))
        .unwrap_or_else(|| PathBuf::from("."))
}

const USAGE: &str = "Usage: xade [PATH]

Opens PATH (a folder, or a file's folder) as a workspace in a new Xade window.
With no PATH, opens Xade as usual.

Options:
  -h, --help       Print this help
  -V, --version    Print the version";

#[derive(Debug, PartialEq)]
enum Cli {
    Help,
    Version,
    /// `detached` is the hidden marker of the re-spawned copy: it must not re-spawn again.
    Run {
        path: Option<PathBuf>,
        detached: bool,
    },
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let (mut path, mut detached) = (None, false);
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Cli::Help),
            "-V" | "--version" => return Ok(Cli::Version),
            "--detached" => detached = true,
            a if a.starts_with('-') => return Err(format!("unknown option '{a}'")),
            _ if path.is_some() => return Err("only one PATH is accepted".into()),
            _ => path = Some(PathBuf::from(arg)),
        }
    }
    Ok(Cli::Run { path, detached })
}

/// The absolute folder to open: a file resolves to its parent; it must exist and be listable.
fn resolve_folder(path: &Path) -> Result<PathBuf, String> {
    let fail = |e: std::io::Error| format!("cannot open '{}': {e}", path.display());
    let mut dir = std::fs::canonicalize(path).map_err(fail)?;
    if dir.is_file() {
        dir.pop();
    }
    std::fs::read_dir(&dir).map_err(fail)?;
    Ok(dir)
}

/// Starts a detached copy of this binary on `dir` in its own process group with no stdio, so
/// the shell gets its prompt back.
fn respawn(dir: &Path) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("--detached")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().map(drop)
}

/// Claude's preflight, run before the window shows. The other providers report in the
/// background (see `App::new`) so a slow one never delays startup.
fn run_preflights(
    providers: &[Arc<dyn AgentProvider>],
) -> HashMap<ProviderId, Result<(), ProviderError>> {
    providers
        .iter()
        .filter(|p| p.id() == ProviderId::Claude)
        .map(|p| {
            let result = p.preflight();
            if let Err(e) = &result {
                tracing::warn!(provider = ?p.id(), error = %e, "provider preflight failed");
            }
            (p.id(), result)
        })
        .collect()
}

/// Starts Xade: the command line, the data folder, logging, the backend and the window.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (path, detached) = match parse_args(std::env::args().skip(1)) {
        Ok(Cli::Help) => {
            println!("{USAGE}");
            return Ok(());
        }
        Ok(Cli::Version) => {
            println!("xade {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Ok(Cli::Run { path, detached }) => (path, detached),
        Err(e) => {
            eprintln!("xade: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let initial_workspace = match path.as_deref().map(resolve_folder).transpose() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("xade: {e}");
            std::process::exit(1);
        }
    };
    if let (Some(dir), false) = (&initial_workspace, detached) {
        if let Err(e) = respawn(dir) {
            eprintln!("xade: cannot start a new window: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let logs = logging::log_dir(&dir);
    logging::init(&logs);
    logging::install_panic_hook(logs.clone());
    logging::log_startup(&dir);
    let (rt, launch) = backend(&dir)?;
    // The runtime must outlive the window: the session manager and its processes run on it.
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
        Box::new(move |cc| Ok(Box::new(launch.into_app(cc, initial_workspace)))),
    )?;
    drop(rt);
    Ok(())
}

/// What the window needs from the backend: the store, the providers and the session manager's
/// channels.
pub struct Launch {
    handle: tokio::runtime::Handle,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<xade_runtime::ManagerCmd>,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<xade_runtime::AppEvent>,
    providers: Vec<Arc<dyn AgentProvider>>,
    writer: xade_store::WriterHandle,
    reader: StoreReader,
    preflight: HashMap<ProviderId, Result<(), ProviderError>>,
    config_path: PathBuf,
    logs: PathBuf,
}

impl Launch {
    /// The app, over the window's creation context, opening `initial_workspace` if given.
    pub fn into_app(
        self,
        cc: &eframe::CreationContext<'_>,
        initial_workspace: Option<PathBuf>,
    ) -> App {
        App::new(
            cc,
            self.handle,
            self.cmd_tx,
            self.event_rx,
            self.providers,
            self.writer,
            self.reader,
            self.preflight,
            self.config_path,
            self.logs,
            initial_workspace,
        )
    }
}

/// Opens the store in `dir`, starts the runtime, the providers and the session manager. `run`
/// and the e2e tests (over a temporary `dir`) both start Xade through this.
pub fn backend(
    dir: &Path,
) -> Result<(tokio::runtime::Runtime, Launch), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let db = dir.join("workspace.db");
    tracing::info!(path = %db.display(), "opening store");
    let writer = StoreWriter::spawn(&db)?;
    let reader = StoreReader::open(&db)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let providers: Vec<Arc<dyn AgentProvider>> = vec![
        Arc::new(ClaudeProvider::new()),
        Arc::new(AntigravityProvider::new()),
    ];
    let preflight = run_preflights(&providers);
    let (cmd_tx, event_rx) = {
        let _guard = rt.enter();
        SessionManager::start(providers.clone(), writer.clone(), reader.clone())
    };
    let launch = Launch {
        handle: rt.handle().clone(),
        cmd_tx,
        event_rx,
        providers,
        writer,
        reader,
        preflight,
        config_path: config::path(dir),
        logs: logging::log_dir(dir),
    };
    Ok((rt, launch))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    fn run(path: Option<&str>, detached: bool) -> Result<Cli, String> {
        Ok(Cli::Run {
            path: path.map(PathBuf::from),
            detached,
        })
    }

    #[test]
    fn no_args_runs_as_usual() {
        assert_eq!(parse(&[]), run(None, false));
    }

    #[test]
    fn help_and_version_flags() {
        for f in ["-h", "--help"] {
            assert_eq!(parse(&[f]), Ok(Cli::Help));
        }
        for f in ["-V", "--version"] {
            assert_eq!(parse(&[f]), Ok(Cli::Version));
        }
    }

    #[test]
    fn a_path_runs_with_it() {
        assert_eq!(parse(&["."]), run(Some("."), false));
        assert_eq!(parse(&["--detached", "/tmp"]), run(Some("/tmp"), true));
    }

    #[test]
    fn unknown_flags_and_extra_paths_are_errors() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["a", "b"]).is_err());
    }

    #[test]
    fn startup_waits_for_claude_only() {
        use crate::app::StubProvider;
        use std::time::{Duration, Instant};
        let slow = Duration::from_secs(30);
        let providers: Vec<Arc<dyn AgentProvider>> = vec![
            Arc::new(StubProvider::claude().preflighting(Duration::ZERO, Ok(()))),
            Arc::new(StubProvider::antigravity().preflighting(slow, Ok(()))),
        ];
        let started = Instant::now();
        let results = run_preflights(&providers);
        assert!(started.elapsed() < slow, "startup waited for Antigravity");
        assert_eq!(results.len(), 1);
        assert_eq!(results[&ProviderId::Claude], Ok(()));
    }

    #[test]
    fn resolves_folders_files_and_missing_paths() {
        let dir = std::env::temp_dir().join(format!("xade-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "").unwrap();
        let canonical = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(resolve_folder(&dir), Ok(canonical.clone()));
        assert_eq!(resolve_folder(&file), Ok(canonical));
        assert!(resolve_folder(&dir.join("missing")).is_err());
    }
}

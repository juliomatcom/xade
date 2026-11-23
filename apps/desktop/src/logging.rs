//! Log files, the panic hook, and the log tail that error reports quote.
//!
//! Logs never hold prompt or message text above `debug`, so the tail is safe to paste.

use std::backtrace::Backtrace;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::ThreadId;

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};
use xade_core::panic_message;

/// Daily files are `xade.log.<date>`.
const LOG_PREFIX: &str = "xade.log";
const KEEP_FILES: usize = 7;
/// The tail is read from the end of the newest file; this much is plenty for 50 lines.
const TAIL_BYTES: u64 = 64 * 1024;

pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

/// Logs to stderr and to a daily rolling file in `dir` (7 files kept). A file that cannot be
/// opened leaves stderr only, and says so.
pub fn init(dir: &Path) {
    let appender = std::fs::create_dir_all(dir)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .filename_prefix(LOG_PREFIX)
                .max_log_files(KEEP_FILES)
                .build(dir)
                .map_err(|e| e.to_string())
        });
    let (file_layer, file_error) = match appender {
        Ok(a) => (Some(fmt::layer().with_writer(a).with_ansi(false)), None),
        Err(e) => (None, Some(e)),
    };
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        .init();
    if let Some(e) = file_error {
        tracing::warn!(error = %e, "log file unavailable; logging to stderr only");
    }
}

/// The startup line: version, OS and data directory.
pub fn log_startup(data_dir: &Path) {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        data_dir = %data_dir.display(),
        "xade starting"
    );
}

/// The last `n` lines of the newest log file, or an empty string.
pub fn tail(dir: &Path, n: usize) -> String {
    let Some(newest) = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(LOG_PREFIX))
        .max_by_key(|e| e.file_name())
    else {
        return String::new();
    };
    let read = || -> std::io::Result<String> {
        let mut file = std::fs::File::open(newest.path())?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    match read() {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(n)..].join("\n")
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not read the log tail");
            String::new()
        }
    }
}

/// Opens `dir` in the system file manager.
pub fn open_dir(dir: &Path) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let mut cmd = std::process::Command::new(program);
    cmd.arg(dir);
    crate::widgets::diff_modal::spawn_detached(cmd);
}

/// The last panic and the thread it happened on, so the frame guard can attach its backtrace.
static LAST_PANIC: Mutex<Option<(ThreadId, String)>> = Mutex::new(None);

/// The backtrace text of the last panic on this thread, once.
pub fn take_panic_details() -> Option<String> {
    let mut last = LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner());
    match last.take() {
        Some((thread, text)) if thread == std::thread::current().id() => Some(text),
        other => {
            *last = other;
            None
        }
    }
}

/// Logs every panic (any thread) with a backtrace and writes `crash-<unix time>.txt` into
/// `dir`; the default hook still prints to stderr.
pub fn install_panic_hook(dir: PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed").to_string();
        let msg = panic_message(info.payload());
        let location = info.location().map_or_else(
            || "unknown".into(),
            |l| format!("{}:{}", l.file(), l.line()),
        );
        let backtrace = Backtrace::force_capture();
        tracing::error!(thread = %name, %location, %msg, "panic\n{backtrace}");
        let text = format!("{msg}\nat {location} on thread {name}\n\n{backtrace}");
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let file = format!(
            "Xade {} on {}\n\n{text}\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        );
        if let Err(e) = std::fs::write(dir.join(format!("crash-{secs}.txt")), file) {
            tracing::error!(error = %e, "could not write the crash file");
        }
        *LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner()) = Some((thread.id(), text));
        default(info);
    }));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn tail_reads_the_newest_file_and_keeps_the_last_lines() {
        let dir = std::env::temp_dir().join(format!("xade-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(tail(&dir, 5), "");
        std::fs::write(dir.join("xade.log.2026-01-01"), "old\n").unwrap();
        let lines: String = (0..100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("xade.log.2026-01-02"), lines).unwrap();
        std::fs::write(dir.join("crash-1.txt"), "not a log").unwrap();
        let t = tail(&dir, 3);
        assert_eq!(t, "line 97\nline 98\nline 99");
        assert_eq!(tail(&dir.join("missing"), 3), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn panic_details_are_taken_once_and_only_on_the_same_thread() {
        *LAST_PANIC.lock().unwrap() = Some((std::thread::current().id(), "x".into()));
        let other = std::thread::spawn(take_panic_details).join().unwrap();
        assert!(other.is_none(), "another thread must not take it");
        assert_eq!(take_panic_details().as_deref(), Some("x"));
        assert!(take_panic_details().is_none());
    }

    #[test]
    fn the_panic_hook_writes_a_crash_file_and_keeps_the_details() {
        let dir = std::env::temp_dir().join(format!("xade-crash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        install_panic_hook(dir.clone());
        let caught = std::panic::catch_unwind(|| panic!("hook test panic"));
        drop(std::panic::take_hook());
        assert!(caught.is_err());
        let details = take_panic_details().expect("details kept for this thread");
        assert!(details.contains("hook test panic") && details.contains("on thread"));
        let crash = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .find(|t| t.contains("hook test panic"))
            .expect("a crash file with the panic");
        assert!(crash.starts_with("Xade ") && crash.contains(std::env::consts::OS));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

use std::process::ExitStatus;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ProviderError {
    #[error("provider CLI is not installed")]
    NotInstalled,
    #[error("provider CLI is not logged in")]
    NotLoggedIn,
    #[error("failed to spawn provider: {0}")]
    Spawn(String),
    #[error("provider protocol error: {0}")]
    Protocol(String),
}

/// Readable text of a caught panic payload (`catch_unwind`, `JoinError::into_panic`).
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// User-facing error text when a provider child exits without being asked to shut down.
/// Includes the exit code or Unix signal name/number and any non-empty stderr tail.
pub fn unexpected_exit_message(
    program: &str,
    status: Option<ExitStatus>,
    stderr: &[String],
) -> String {
    let reason = status.and_then(exit_reason);
    let detail = stderr.join("\n");
    let detail = detail.trim();
    match (reason, detail.is_empty(), detail.contains('\n')) {
        (Some(r), true, _) => format!("{program} exited unexpectedly ({r})"),
        (Some(r), false, false) => format!("{program} exited unexpectedly ({r}): {detail}"),
        (Some(r), false, true) => format!("{program} exited unexpectedly ({r})\n{detail}"),
        (None, true, _) => format!("{program} exited unexpectedly"),
        (None, false, false) => format!("{program} exited unexpectedly: {detail}"),
        (None, false, true) => format!("{program} exited unexpectedly\n{detail}"),
    }
}

fn exit_reason(status: ExitStatus) -> Option<String> {
    if let Some(code) = status.code() {
        return Some(format!("exit code {code}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return Some(match signal_name(sig) {
                Some(name) => format!("{name}, signal {sig}"),
                None => format!("signal {sig}"),
            });
        }
    }
    None
}

#[cfg(unix)]
fn signal_name(sig: i32) -> Option<&'static str> {
    match sig {
        libc::SIGHUP => Some("SIGHUP"),
        libc::SIGINT => Some("SIGINT"),
        libc::SIGQUIT => Some("SIGQUIT"),
        libc::SIGILL => Some("SIGILL"),
        libc::SIGTRAP => Some("SIGTRAP"),
        libc::SIGABRT => Some("SIGABRT"),
        libc::SIGBUS => Some("SIGBUS"),
        libc::SIGFPE => Some("SIGFPE"),
        libc::SIGKILL => Some("SIGKILL"),
        libc::SIGSEGV => Some("SIGSEGV"),
        libc::SIGPIPE => Some("SIGPIPE"),
        libc::SIGALRM => Some("SIGALRM"),
        libc::SIGTERM => Some("SIGTERM"),
        libc::SIGXCPU => Some("SIGXCPU"),
        libc::SIGXFSZ => Some("SIGXFSZ"),
        libc::SIGSYS => Some("SIGSYS"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_status_formats_with_and_without_stderr() {
        assert_eq!(
            unexpected_exit_message("agy", None, &[]),
            "agy exited unexpectedly"
        );
        assert_eq!(
            unexpected_exit_message("agy", None, &["  ".into(), String::new()]),
            "agy exited unexpectedly"
        );
        assert_eq!(
            unexpected_exit_message("agy", None, &["boom".into()]),
            "agy exited unexpectedly: boom"
        );
        assert_eq!(
            unexpected_exit_message("agy", None, &["line 1".into(), "line 2".into()]),
            "agy exited unexpectedly\nline 1\nline 2"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_exit_code_and_signals_are_included() {
        use std::os::unix::process::ExitStatusExt;

        let exited = ExitStatus::from_raw(3 << 8);
        assert_eq!(
            unexpected_exit_message("agy", Some(exited), &["boom".into()]),
            "agy exited unexpectedly (exit code 3): boom"
        );
        assert_eq!(
            unexpected_exit_message("agy", Some(exited), &["line 1".into(), "line 2".into()]),
            "agy exited unexpectedly (exit code 3)\nline 1\nline 2"
        );
        assert_eq!(
            unexpected_exit_message("claude", Some(exited), &[]),
            "claude exited unexpectedly (exit code 3)"
        );

        let sigbus = ExitStatus::from_raw(libc::SIGBUS);
        assert_eq!(
            unexpected_exit_message("agy", Some(sigbus), &[]),
            format!("agy exited unexpectedly (SIGBUS, signal {})", libc::SIGBUS)
        );

        let sigkill = ExitStatus::from_raw(libc::SIGKILL);
        assert_eq!(
            unexpected_exit_message("codex", Some(sigkill), &["oom".into()]),
            format!(
                "codex exited unexpectedly (SIGKILL, signal {}): oom",
                libc::SIGKILL
            )
        );

        let unknown_sig = ExitStatus::from_raw(63);
        assert_eq!(
            unexpected_exit_message("agy", Some(unknown_sig), &[]),
            "agy exited unexpectedly (signal 63)"
        );
    }
}

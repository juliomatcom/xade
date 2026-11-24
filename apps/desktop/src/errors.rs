//! The error queue and its modal: what the user is told when something went wrong, with a
//! ready-to-paste report and a pre-filled GitHub issue.

use std::collections::VecDeque;
use std::path::Path;

use egui::RichText;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{icons, theme};

const ISSUES_URL: &str = "https://github.com/juliomatcom/xade/issues/new";
/// The whole issue URL stays under this many characters.
const MAX_URL_LEN: usize = 6000;
const TRUNCATED: &str = "\n...[truncated]";
/// Distinct errors kept waiting; a flood beyond this is only logged.
const MAX_QUEUED: usize = 50;

#[derive(Clone, Debug)]
pub struct ErrorReport {
    pub title: String,
    pub message: String,
    /// Technical detail (backtrace, causes); may be empty.
    pub details: String,
    pub at: OffsetDateTime,
}

impl ErrorReport {
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            details: String::new(),
            at: OffsetDateTime::now_utc(),
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = details.into();
        self
    }
}

struct Entry {
    report: ErrorReport,
    count: u32,
}

/// Errors waiting to be shown, one at a time. An error equal to a queued one (same title and
/// message) only raises that one's count.
#[derive(Default)]
pub struct ErrorQueue {
    items: VecDeque<Entry>,
}

impl ErrorQueue {
    pub fn push(&mut self, report: ErrorReport) {
        if let Some(e) = self
            .items
            .iter_mut()
            .find(|e| e.report.title == report.title && e.report.message == report.message)
        {
            e.count += 1;
        } else if self.items.len() < MAX_QUEUED {
            self.items.push_back(Entry { report, count: 1 });
        }
    }

    /// The error on screen and how many times it happened.
    pub fn current(&self) -> Option<(&ErrorReport, u32)> {
        self.items.front().map(|e| (&e.report, e.count))
    }

    pub fn dismiss(&mut self) {
        self.items.pop_front();
    }

    /// How many distinct errors wait, the one on screen included.
    pub fn pending(&self) -> usize {
        self.items.len()
    }
}

/// What the user chose in the modal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModalAction {
    None,
    Copy,
    OpenIssue,
    OpenLogs,
    Dismiss,
}

/// The error and its details as one block, for the text view and the clipboard.
fn error_block(report: &ErrorReport, count: u32) -> String {
    let mut out = report.message.clone();
    if count > 1 {
        out.push_str(&format!("\n(happened {count} times)"));
    }
    if !report.details.is_empty() {
        out.push_str("\n\n");
        out.push_str(&report.details);
    }
    out
}

/// Version, OS, error, details and the log tail: what Copy puts on the clipboard.
pub fn report_text(report: &ErrorReport, count: u32, log_tail: &str) -> String {
    let at = report.at.format(&Rfc3339).unwrap_or_default();
    let mut out = format!(
        "Xade {} on {}\n{at}\n\n{}\n\n{}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        report.title,
        error_block(report, count),
    );
    if !log_tail.trim().is_empty() {
        out.push_str(&format!("\n\nLast log lines:\n{}", log_tail.trim_end()));
    }
    out
}

/// The `new issue` URL for a report. The home directory becomes `~` and email-like text is
/// masked; the encoded URL is cut to `MAX_URL_LEN` characters.
pub fn issue_url(report: &ErrorReport, count: u32, log_tail: &str, home: Option<&Path>) -> String {
    let title = redact(&report.title, home);
    let body = redact(&report_text(report, count, log_tail), home);
    build_url(&title, &body, MAX_URL_LEN)
}

fn build_url(title: &str, body: &str, max_len: usize) -> String {
    let url = |body: &str| {
        format!(
            "{ISSUES_URL}?title={}&body={}",
            percent_encode(title),
            percent_encode(body)
        )
    };
    let full = url(body);
    if full.len() <= max_len {
        return full;
    }
    // Longest prefix (in chars) that still fits with the marker.
    let chars: Vec<char> = body.chars().collect();
    let cut = |n: usize| chars[..n].iter().collect::<String>() + TRUNCATED;
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if url(&cut(mid)).len() <= max_len {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    url(&cut(lo))
}

/// Percent-encodes everything but the RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Replaces the home directory with `~` and masks email-like strings.
pub fn redact(text: &str, home: Option<&Path>) -> String {
    let mut out = text.to_string();
    if let Some(home) = home.map(|h| h.to_string_lossy().into_owned()) {
        if home.len() > 1 {
            out = out.replace(home.trim_end_matches('/'), "~");
        }
    }
    mask_emails(&out)
}

fn mask_emails(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    let domain = |c: char| c.is_ascii_alphanumeric() || ".-".contains(c);
    let mut out = String::with_capacity(s.len());
    let mut done = 0;
    for at in 0..chars.len() {
        if chars[at] != '@' || at < done {
            continue;
        }
        let mut start = at;
        while start > done && local(chars[start - 1]) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < chars.len() && domain(chars[end]) {
            end += 1;
        }
        // A trailing dot is sentence punctuation.
        while end > at + 1 && matches!(chars[end - 1], '.' | '-') {
            end -= 1;
        }
        let host: String = chars[at + 1..end].iter().collect();
        let valid = start < at && host.contains('.') && host.split('.').all(|l| !l.is_empty());
        if valid {
            out.extend(&chars[done..start]);
            out.push_str("<email>");
            done = end;
        }
    }
    out.extend(&chars[done..]);
    out
}

/// Draws the modal for the error at the front of the queue. Esc dismisses it.
pub fn modal(ctx: &egui::Context, queue: &ErrorQueue) -> ModalAction {
    let Some((report, count)) = queue.current() else {
        return ModalAction::None;
    };
    let mut action = ModalAction::None;
    let more = queue.pending() - 1;
    let resp = crate::widgets::modal(ctx, egui::Id::unique("error_modal")).show(ctx, |ui| {
        ui.set_width(560.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} {}", icons::WARNING, report.title))
                    .strong()
                    .size(18.0)
                    .color(theme::danger()),
            );
            if count > 1 {
                ui.label(RichText::new(format!("\u{d7}{count}")).color(theme::text_muted()));
            }
        });
        ui.add_space(6.0);
        let text = error_block(report, count);
        egui::ScrollArea::vertical()
            .max_height(260.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                // A read-only buffer: selectable and copyable, not editable.
                let mut view: &str = &text;
                ui.add(
                    egui::TextEdit::multiline(&mut view)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY),
                );
            });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(icons::with(icons::COPY, "Copy")).clicked() {
                action = ModalAction::Copy;
            }
            if ui
                .button(icons::with(icons::BUG, "Open GitHub issue"))
                .clicked()
            {
                action = ModalAction::OpenIssue;
            }
            if ui
                .button(icons::with(icons::FOLDER_OPEN, "Open logs folder"))
                .clicked()
            {
                action = ModalAction::OpenLogs;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let dismiss = egui::Button::new(
                    RichText::new(icons::with(icons::CLOSE, "Dismiss")).color(theme::text()),
                )
                .fill(theme::accent());
                if ui.add(dismiss).clicked() {
                    action = ModalAction::Dismiss;
                }
                if more > 0 {
                    ui.label(
                        RichText::new(format!("{more} more"))
                            .small()
                            .color(theme::text_muted()),
                    );
                }
            });
        });
    });
    if action == ModalAction::None && resp.should_close() {
        action = ModalAction::Dismiss;
    }
    action
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn report(msg: &str) -> ErrorReport {
        ErrorReport::new("Saving is disabled", msg).with_details("details line")
    }

    #[test]
    fn identical_errors_collapse_with_a_count() {
        let mut q = ErrorQueue::default();
        q.push(report("a"));
        q.push(report("a"));
        q.push(report("b"));
        q.push(report("a"));
        assert_eq!(q.pending(), 2);
        let (r, n) = q.current().unwrap();
        assert_eq!((r.message.as_str(), n), ("a", 3));
        q.dismiss();
        assert_eq!(
            q.current().map(|(r, n)| (r.message.as_str(), n)),
            Some(("b", 1))
        );
        q.dismiss();
        assert!(q.pending() == 0 && q.current().is_none());
    }

    #[test]
    fn the_queue_is_bounded() {
        let mut q = ErrorQueue::default();
        for i in 0..MAX_QUEUED + 10 {
            q.push(report(&i.to_string()));
        }
        assert_eq!(q.pending(), MAX_QUEUED);
    }

    #[test]
    fn redaction_hides_home_and_emails() {
        let home = PathBuf::from("/Users/jane");
        let text = "open /Users/jane/code/x.rs; mail jane.doe+x@example.co.uk, or a@b.io. \
                    Not an email: user@localhost and @mention and a@";
        let out = redact(text, Some(&home));
        assert!(out.contains("~/code/x.rs"), "{out}");
        assert!(!out.contains("jane"), "{out}");
        assert_eq!(out.matches("<email>").count(), 2, "{out}");
        assert!(out.contains("<email>, or <email>."), "{out}");
        assert!(out.contains("user@localhost and @mention and a@"), "{out}");
    }

    #[test]
    fn issue_url_is_encoded_and_carries_the_report() {
        let r = report("disk & \"quote\" \u{e9}");
        let url = issue_url(&r, 2, "INFO line one\nWARN line two", None);
        assert!(url.starts_with("https://github.com/juliomatcom/xade/issues/new?title="));
        assert!(url.contains("title=Saving%20is%20disabled&body="), "{url}");
        assert!(url.contains("%26") && url.contains("%22") && url.contains("%C3%A9"));
        assert!(!url.contains(' ') && !url.contains('"') && !url.contains('\n'));
        assert!(url.contains("happened%202%20times") && url.contains("WARN%20line%20two"));
    }

    #[test]
    fn issue_url_is_redacted() {
        let home = PathBuf::from("/Users/jane");
        let r = ErrorReport::new("t", "failed at /Users/jane/p, ask jane@example.com");
        let url = issue_url(&r, 1, "", Some(&home));
        assert!(!url.contains("jane"), "{url}");
        assert!(url.contains("~%2Fp"), "{url}");
        assert!(url.contains("%3Cemail%3E"));
    }

    #[test]
    fn issue_url_is_truncated_to_the_limit() {
        let tail: String = (0..2000)
            .map(|i| format!("line {i} \u{e9}\u{e9}\n"))
            .collect();
        let r = report("boom");
        let url = issue_url(&r, 1, &tail, None);
        assert!(url.len() <= MAX_URL_LEN, "{}", url.len());
        assert!(
            url.len() > MAX_URL_LEN - 20,
            "cut as late as possible: {}",
            url.len()
        );
        assert!(url.ends_with("%5Btruncated%5D"), "{url}");
        // The head (the error itself) survives; a short report is left whole.
        assert!(url.contains("boom"));
        assert!(!issue_url(&r, 1, "short", None).contains("truncated"));
    }

    #[test]
    fn report_text_has_version_os_error_details_and_log_tail() {
        let text = report_text(&report("msg"), 1, "a log line\n");
        for needle in [
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            "Saving is disabled",
            "msg",
            "details line",
            "Last log lines:\na log line",
        ] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
        assert!(!report_text(&report("msg"), 1, "").contains("Last log lines"));
    }

    /// The status-bar account label is display-only: no report, copy text or issue URL has it.
    #[test]
    fn reports_never_carry_the_account_label() {
        use xade_core::AgentProvider;
        let provider = crate::status_bar::LabelledProvider("zq-org-label");
        let label = provider.account_label().unwrap();
        let r = report("msg");
        let tail = "INFO line";
        assert!(!report_text(&r, 1, tail).contains(&label));
        assert!(!issue_url(&r, 1, tail, None).contains(&label));
    }

    fn texts(out: &egui::FullOutput) -> Vec<String> {
        fn walk(s: &egui::Shape, v: &mut Vec<String>) {
            match s {
                egui::Shape::Text(t) => v.push(t.galley.text().to_string()),
                egui::Shape::Vec(l) => l.iter().for_each(|s| walk(s, v)),
                _ => {}
            }
        }
        let mut v = Vec::new();
        out.shapes.iter().for_each(|c| walk(&c.shape, &mut v));
        v
    }

    #[test]
    fn modal_renders_headless_and_dismisses_on_escape() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut q = ErrorQueue::default();
        q.push(report("the message"));
        q.push(report("the message"));
        q.push(report("another"));
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let run = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut action = ModalAction::None;
            let mut out = ctx.run_pass(input, |ctx| action = modal(ctx, &q));
            out.textures_delta.clear();
            (action, texts(&out))
        };
        run(vec![]);
        let (action, shown) = run(vec![]);
        assert_eq!(action, ModalAction::None);
        let all = shown.join("|");
        for needle in [
            "Saving is disabled",
            "the message",
            "details line",
            "Copy",
            "Open GitHub issue",
            "Open logs folder",
            "Dismiss",
            "1 more",
            "\u{d7}2",
        ] {
            assert!(all.contains(needle), "{needle} missing in {all}");
        }
        let esc = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        assert_eq!(run(vec![esc]).0, ModalAction::Dismiss);
        assert_eq!(modal(&ctx, &ErrorQueue::default()), ModalAction::None);
    }
}

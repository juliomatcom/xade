//! Tool-call cards and the pure helpers behind them.

use aw_core::Message;
use egui::collapsing_header::CollapsingState;
use egui::{Id, RichText, Stroke};
use egui_commonmark::CommonMarkCache;
use serde_json::Value;

use super::markdown::markdown;
use crate::diff::is_code_change;
use crate::theme;

const OUTPUT_LIMIT: usize = 4096;
const SUMMARY_LIMIT: usize = 80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CardStatus {
    Running,
    Ok,
    Error,
    /// No result and nothing running (interrupted or loaded from history).
    Unknown,
}

/// One-line description of a tool call.
pub fn summary(name: &str, input_json: &str) -> String {
    let input: Option<Value> = serde_json::from_str(input_json).ok();
    let field = |k: &str| {
        input
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let found = match name {
        "Bash" => field("command").map(|c| clip(c.lines().next().unwrap_or(""), SUMMARY_LIMIT)),
        "Read" | "Edit" | "MultiEdit" | "Write" => field("file_path"),
        "NotebookEdit" => field("notebook_path"),
        _ => None,
    };
    found.unwrap_or_else(|| clip(input_json, SUMMARY_LIMIT))
}

fn clip(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Fence language (a token syntect knows) for a file path; empty when unknown.
pub fn language_from_path(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map_or("", |(_, e)| e);
    match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => "js",
        "json" => "json",
        "md" | "markdown" => "md",
        "sh" | "bash" | "zsh" => "sh",
        "html" | "htm" => "html",
        "css" => "css",
        "xml" => "xml",
        "yml" | "yaml" => "yaml",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" => "cpp",
        "rb" => "rb",
        "sql" => "sql",
        "toml" => "toml",
        _ => "",
    }
}

/// A fenced block that cannot be closed early by backticks inside `text`.
pub fn fenced(lang: &str, text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{lang}\n{text}\n{fence}")
}

/// `text` cut at a char boundary below `max` bytes, and whether it was cut.
pub fn truncate(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn input_block(name: &str, input_json: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(input_json).ok();
    if name == "Bash" {
        if let Some(cmd) = parsed
            .as_ref()
            .and_then(|v| v.get("command"))
            .and_then(Value::as_str)
        {
            return fenced("sh", cmd);
        }
    }
    let pretty = parsed
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| input_json.to_string());
    fenced("json", &pretty)
}

fn output_lang(name: &str, input_json: &str) -> &'static str {
    if name != "Read" {
        return "";
    }
    serde_json::from_str::<Value>(input_json)
        .ok()
        .and_then(|v| v.get("file_path")?.as_str().map(language_from_path))
        .unwrap_or("")
}

/// Paints a spinner, check or cross; returns nothing to click.
fn status_icon(ui: &mut egui::Ui, status: CardStatus) {
    match status {
        CardStatus::Running => {
            ui.add(egui::Spinner::new().size(12.0));
        }
        CardStatus::Unknown => {
            ui.allocate_space(egui::vec2(12.0, 12.0));
        }
        CardStatus::Ok | CardStatus::Error => {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
            let p = ui.painter();
            let (a, b, c, d) = (
                rect.left_top(),
                rect.right_top(),
                rect.left_bottom(),
                rect.right_bottom(),
            );
            if status == CardStatus::Ok {
                let s = Stroke::new(1.8, theme::OK);
                let knee = egui::pos2(rect.left() + 4.0, rect.bottom() - 2.0);
                p.line_segment([egui::pos2(rect.left() + 1.0, rect.center().y), knee], s);
                p.line_segment([knee, egui::pos2(rect.right() - 1.0, rect.top() + 2.0)], s);
            } else {
                let s = Stroke::new(1.8, theme::DANGER);
                p.line_segment([a, d], s);
                p.line_segment([b, c], s);
            }
        }
    }
}

/// Draws a tool card. Returns true when "Review changes" was clicked.
pub fn show(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    call: &Message,
    result: Option<&Message>,
    status: CardStatus,
    awaiting: bool,
) -> bool {
    let name = call.tool_name.as_deref().unwrap_or("tool");
    let input = call.tool_payload_json.as_deref().unwrap_or("");
    let id = Id::unique(("tool_card", call.id));
    let mut review = false;
    egui::Frame::new()
        .fill(theme::BG_RAISED)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let head = |ui: &mut egui::Ui| {
                status_icon(ui, status);
                ui.label(RichText::new(name).strong());
                let text = summary(name, input);
                ui.add(egui::Label::new(RichText::new(text).color(theme::TEXT_MUTED)).truncate());
            };
            if is_code_change(name) {
                ui.horizontal(head);
                let label = if awaiting {
                    "Review changes (awaiting approval)"
                } else {
                    "Review changes"
                };
                review = ui.small_button(label).clicked();
                return;
            }
            let state = CollapsingState::load_with_default_open(ui.ctx(), id, false);
            state.show_header(ui, head).body(|ui| {
                markdown(ui, cache, (id, "in"), &input_block(name, input));
                let Some(res) = result else { return };
                let all_id = id.with("all");
                let all = ui.data(|d| d.get_temp::<bool>(all_id)).unwrap_or(false);
                let (shown, cut) = if all {
                    (res.content_text.as_str(), false)
                } else {
                    truncate(&res.content_text, OUTPUT_LIMIT)
                };
                let lang = output_lang(name, input);
                markdown(ui, cache, (id, "out"), &fenced(lang, shown));
                if cut && ui.small_button("Show all").clicked() {
                    ui.data_mut(|d| d.insert_temp(all_id, true));
                }
            });
        });
    review
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_picks_the_main_field() {
        assert_eq!(summary("Bash", r#"{"command":"ls -la\nfoo"}"#), "ls -la");
        assert_eq!(summary("Read", r#"{"file_path":"/a/b.rs"}"#), "/a/b.rs");
        assert_eq!(
            summary("Edit", r#"{"file_path":"/a/c.rs","old_string":"x"}"#),
            "/a/c.rs"
        );
        assert_eq!(
            summary("Write", r#"{"file_path":"/a/d.md","content":"x"}"#),
            "/a/d.md"
        );
        let long = format!(r#"{{"pattern":"{}"}}"#, "x".repeat(200));
        let s = summary("Grep", &long);
        assert_eq!(s.chars().count(), SUMMARY_LIMIT + 1);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn language_follows_the_extension() {
        assert_eq!(language_from_path("/x/main.rs"), "rust");
        assert_eq!(language_from_path("a.TSX"), "js");
        assert_eq!(language_from_path("Makefile"), "");
        assert_eq!(language_from_path("noext."), "");
    }

    #[test]
    fn fence_outgrows_backticks_in_the_text() {
        assert_eq!(fenced("sh", "ls"), "```sh\nls\n```");
        assert!(fenced("", "a ```` b").starts_with("`````\n"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 10), ("abc", false));
        assert_eq!(truncate("aéb", 2), ("a", true));
    }
}

//! Tool-call cards and the pure helpers behind them.

use egui::collapsing_header::CollapsingState;
use egui::{Id, RichText, Stroke};
use egui_commonmark::CommonMarkCache;
use serde_json::Value;
use xade_core::Message;

use super::markdown::markdown;
use crate::diff::is_code_change;
use crate::{icons, theme};

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
                let s = Stroke::new(1.8, theme::ok());
                let knee = egui::pos2(rect.left() + 4.0, rect.bottom() - 2.0);
                p.line_segment([egui::pos2(rect.left() + 1.0, rect.center().y), knee], s);
                p.line_segment([knee, egui::pos2(rect.right() - 1.0, rect.top() + 2.0)], s);
            } else {
                let s = Stroke::new(1.8, theme::danger());
                p.line_segment([a, d], s);
                p.line_segment([b, c], s);
            }
        }
    }
}

/// Draws a tool card. Returns true when "Review changes" was clicked, shown only while a code change awaits approval.
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
        .fill(theme::bg_terminal())
        .stroke(Stroke::new(1.0, theme::border()))
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let head = |ui: &mut egui::Ui| {
                status_icon(ui, status);
                ui.label(RichText::new(name).strong());
                let text = summary(name, input);
                ui.add(egui::Label::new(RichText::new(text).color(theme::text_muted())).truncate());
            };
            if is_code_change(name) {
                ui.horizontal(head);
                if awaiting {
                    review = ui
                        .add(
                            egui::Button::new(icons::with(
                                icons::REVIEW,
                                "Review changes (awaiting approval)",
                            ))
                            .small()
                            .wrap(),
                        )
                        .clicked();
                }
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

/// One call of a group, with what it produced.
pub struct GroupCall<'a> {
    pub call: &'a Message,
    pub result: Option<&'a Message>,
    pub status: CardStatus,
}

/// What the collapsed group line says.
#[derive(Debug, PartialEq)]
struct GroupInfo {
    total: usize,
    /// Tool names in first-use order with their counts, "Bash x3 · Read".
    names: String,
    failed: usize,
    /// Name and summary of the call still running (the latest), if any.
    current: Option<String>,
    status: CardStatus,
}

fn describe(c: &GroupCall) -> String {
    let name = c.call.tool_name.as_deref().unwrap_or("tool");
    let input = c.call.tool_payload_json.as_deref().unwrap_or("");
    format!("{name} {}", summary(name, input))
}

fn group_info(calls: &[GroupCall]) -> GroupInfo {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for c in calls {
        let name = c.call.tool_name.as_deref().unwrap_or("tool");
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, k)) => *k += 1,
            None => counts.push((name, 1)),
        }
    }
    let names = counts
        .iter()
        .map(|(n, k)| {
            if *k > 1 {
                format!("{n} \u{d7}{k}")
            } else {
                n.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
    let is = |s: CardStatus| calls.iter().filter(move |c| c.status == s);
    let failed = is(CardStatus::Error).count();
    // Running while any call runs; otherwise the outcome of the last call executed.
    let status = if is(CardStatus::Running).next().is_some() {
        CardStatus::Running
    } else {
        calls.last().map_or(CardStatus::Unknown, |c| c.status)
    };
    GroupInfo {
        total: calls.len(),
        names,
        failed,
        current: is(CardStatus::Running).next_back().map(describe),
        status,
    }
}

/// The single line a collapsed group shows: count, tools, failure count, and the running call.
fn group_line(ui: &egui::Ui, info: &GroupInfo) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('\u{2026}');
    let mut add = |text: &str, color: egui::Color32| {
        job.append(
            text,
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color,
                ..Default::default()
            },
        );
    };
    add(&format!("{} tool calls", info.total), theme::text());
    add(&format!("  \u{b7}  {}", info.names), theme::text_muted());
    if info.failed > 0 {
        add(
            &format!("  \u{b7}  {} failed", info.failed),
            theme::danger(),
        );
    }
    if let Some(now) = &info.current {
        add(&format!("  \u{b7}  {now}"), theme::text_muted());
    }
    job
}

/// Several consecutive tool calls as one line that expands into their cards. The failure count shows
/// on that line even while it is collapsed.
pub fn group(ui: &mut egui::Ui, cache: &mut CommonMarkCache, calls: &[GroupCall]) {
    let Some(first) = calls.first() else { return };
    let id = Id::unique(("tool_group", first.call.id));
    let open_id = id.with("open");
    let open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
    let info = group_info(calls);
    let header = egui::Frame::new()
        .fill(theme::bg_terminal())
        .stroke(Stroke::new(1.0, theme::border()))
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let chevron = if open {
                    icons::EXPANDED
                } else {
                    icons::COLLAPSED
                };
                ui.label(RichText::new(chevron).color(theme::text_muted()));
                status_icon(ui, info.status);
                ui.add(egui::Label::new(group_line(ui, &info)).truncate());
            });
        })
        .response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if header.clicked() {
        ui.data_mut(|d| d.insert_temp(open_id, !open));
    }
    if open {
        ui.indent(id, |ui| {
            for c in calls {
                ui.add_space(2.0);
                show(ui, cache, c.call, c.result, c.status, false);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, input: &str) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            session_id: xade_core::SessionId::new(),
            seq: 0,
            role: xade_core::Role::ToolCall,
            content_text: String::new(),
            thinking_text: None,
            tool_name: Some(name.into()),
            tool_use_id: Some(name.into()),
            tool_payload_json: Some(input.into()),
            is_error: false,
            created_at: time::OffsetDateTime::now_utc(),
            provider_id: None,
            sender_handle: None,
        }
    }

    #[test]
    fn a_group_names_its_tools_and_its_failures() {
        let msgs = [
            call("Bash", r#"{"command":"ls"}"#),
            call("Read", r#"{"file_path":"/a.rs"}"#),
            call("Bash", r#"{"command":"cargo test"}"#),
        ];
        let calls = |statuses: [CardStatus; 3]| -> Vec<GroupCall> {
            msgs.iter()
                .zip(statuses)
                .map(|(call, status)| GroupCall {
                    call,
                    result: None,
                    status,
                })
                .collect()
        };
        let ok = group_info(&calls([CardStatus::Ok; 3]));
        assert_eq!(
            (ok.total, ok.names.as_str()),
            (3, "Bash \u{d7}2 \u{b7} Read")
        );
        assert_eq!((ok.status, ok.failed), (CardStatus::Ok, 0));
        let bad = group_info(&calls([CardStatus::Ok, CardStatus::Ok, CardStatus::Error]));
        assert_eq!((bad.status, bad.failed), (CardStatus::Error, 1));
        let recovered = group_info(&calls([CardStatus::Error, CardStatus::Ok, CardStatus::Ok]));
        assert_eq!((recovered.status, recovered.failed), (CardStatus::Ok, 1));
        let live = group_info(&calls([
            CardStatus::Error,
            CardStatus::Ok,
            CardStatus::Running,
        ]));
        assert_eq!(live.status, CardStatus::Running);
        assert_eq!(live.current.as_deref(), Some("Bash cargo test"));
        assert_eq!(live.failed, 1);
        let idle = group_info(&calls([
            CardStatus::Ok,
            CardStatus::Ok,
            CardStatus::Unknown,
        ]));
        assert_eq!(idle.status, CardStatus::Unknown);
    }

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

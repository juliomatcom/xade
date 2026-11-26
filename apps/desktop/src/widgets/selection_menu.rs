//! Copy / Quote popup that follows a text selection in a chat message.

use egui::{Id, Pos2, Rect};

use crate::app::{App, View};
use crate::icons;
use crate::workspace::{win_id, WindowKind};

const SETTLED: &str = "xade_selection_settled";
const TEXT_BOX: &str = "xade_text_box_focused";

/// A selection waiting in the popup: its text and where the pointer let go.
pub struct Pending {
    text: String,
    pos: Pos2,
    /// The popup's rect from the last frame it was drawn.
    rect: Option<Rect>,
}

/// Catches egui's "selection settled" output (labels only) so the app can react to it.
pub struct SelectionWatcher;

impl egui::Plugin for SelectionWatcher {
    fn debug_name(&self) -> &'static str {
        "XadeSelectionWatcher"
    }

    fn output_hook(&mut self, ctx: &egui::Context, output: &mut egui::FullOutput) {
        for cmd in &output.platform_output.commands {
            if let egui::OutputCommand::TextSelectionSettled(text) = cmd {
                ctx.data_mut(|d| d.insert_temp(Id::unique(SETTLED), text.clone()));
                // The popup reads it next frame; egui repaints only on input, so without this
                // it would wait for the next mouse move.
                ctx.request_repaint();
            }
        }
    }
}

/// Called while an editable text box (prompt, file) has focus, so its selections are not offered
/// the popup. Code blocks in messages are focusable text edits too and must still get it.
pub fn mark_text_box(ctx: &egui::Context) {
    let frame = ctx.cumulative_frame_nr();
    ctx.data_mut(|d| d.insert_temp(Id::unique(TEXT_BOX), frame));
}

pub(crate) fn in_text_box(ctx: &egui::Context) -> bool {
    let frame = ctx.cumulative_frame_nr();
    ctx.data(|d| d.get_temp::<u64>(Id::unique(TEXT_BOX))) == Some(frame)
}

/// `> ` before every line, ready to type under.
fn quoted(text: &str) -> String {
    let mut out: String = text
        .trim_end()
        .lines()
        .map(|l| {
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str("\n\n");
    out
}

/// Appends a quote to a prompt box's text, on its own paragraph.
pub fn append_quote(input: &mut String, text: &str) {
    if !input.is_empty() && !input.ends_with("\n\n") {
        input.push_str(if input.ends_with('\n') { "\n" } else { "\n\n" });
    }
    input.push_str(&quoted(text));
}

/// True when `pos` lands directly on an open, uncollapsed workspace window (not the top bar,
/// sidebar, status bar, or a floating modal).
fn in_window(app: &App, ctx: &egui::Context, pos: Pos2) -> bool {
    let Some(layer) = ctx.layer_id_at(pos) else {
        return false;
    };
    app.open_windows.iter().any(|&kind| {
        let collapsed = match kind {
            WindowKind::Session(id) => app.sessions.get(&id).is_none_or(|vm| vm.collapsed),
            WindowKind::MultiSession(id) => {
                app.multi_sessions.get(&id).is_none_or(|vm| vm.collapsed)
            }
            WindowKind::Editor(id) => app.editors.map.get(&id).is_none_or(|vm| vm.collapsed),
        };
        !collapsed && layer.id == win_id(kind)
    })
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    if app.view != View::Workspace {
        ctx.data_mut(|d| {
            d.remove_temp::<String>(Id::unique(SETTLED));
        });
        app.selection_menu = None;
        return;
    }
    if let Some(text) = ctx.data_mut(|d| d.remove_temp::<String>(Id::unique(SETTLED))) {
        // Selections inside a text box (prompt, file) or outside an open window are not chat text.
        let in_text_box = in_text_box(ctx);
        let pos = ctx.input(|i| i.pointer.latest_pos());
        let can_quote = app.focused_session().is_some() || app.focused_multi_session().is_some();
        app.selection_menu = match (in_text_box, pos, can_quote) {
            (false, Some(pos), true) if !text.trim().is_empty() && in_window(app, ctx, pos) => {
                Some(Pending {
                    text,
                    pos,
                    rect: None,
                })
            }
            _ => None,
        };
    }
    let Some(pending) = app.selection_menu.as_mut() else {
        return;
    };
    let pressed_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && pending
                .rect
                .zip(i.pointer.interact_pos())
                .is_some_and(|(r, p)| !r.contains(p))
    });
    if pressed_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.selection_menu = None;
        return;
    }
    let (mut copy, mut quote) = (false, false);
    let screen = ctx.content_rect();
    let at = (pending.pos + egui::vec2(6.0, 12.0)).min(screen.max - egui::vec2(170.0, 44.0));
    let area = egui::Area::new(Id::unique("selection_menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(at)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    copy = ui.button(icons::with(icons::COPY, "Copy")).clicked();
                    quote = ui.button("\u{275D} Quote").clicked();
                });
            });
        });
    pending.rect = Some(area.response.rect);
    if copy || quote {
        let text = std::mem::take(&mut pending.text);
        app.selection_menu = None;
        if copy {
            ctx.copy_text(text);
        } else if let Some(vm) = app
            .focused_session()
            .and_then(|id| app.sessions.get_mut(&id))
        {
            append_quote(&mut vm.input, &text);
            vm.focus_input = true;
        } else if let Some(vm) = app
            .focused_multi_session()
            .and_then(|id| app.multi_sessions.get_mut(&id))
        {
            append_quote(&mut vm.input, &text);
            vm.focus_input = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_every_line_and_leaves_room_to_type() {
        assert_eq!(quoted("a\n\nb"), "> a\n>\n> b\n\n");
    }

    #[test]
    fn a_quote_starts_its_own_paragraph() {
        let mut s = String::new();
        append_quote(&mut s, "x");
        assert_eq!(s, "> x\n\n");
        let mut s = "hi".to_string();
        append_quote(&mut s, "x");
        assert_eq!(s, "hi\n\n> x\n\n");
        let mut s = "hi\n".to_string();
        append_quote(&mut s, "x");
        assert_eq!(s, "hi\n\n> x\n\n");
    }

    /// Drags from the first paragraph into the second and returns what egui reported.
    fn drag_report(text: &str) -> Option<String> {
        use egui::{Event, PointerButton, Pos2, RawInput};
        let ctx = egui::Context::default();
        ctx.add_plugin(SelectionWatcher);
        let mut cache = crate::widgets::markdown::new_cache();
        let mut release_repaint = false;
        let mut run = |events: Vec<Event>| -> bool {
            let mut input = RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    Pos2::ZERO,
                    egui::vec2(600.0, 400.0),
                )),
                ..Default::default()
            };
            input.events = events;
            let out = ctx.run_ui(input, |ui| {
                crate::widgets::markdown::markdown(ui, &mut cache, 1, text);
            });
            let repaint = out
                .viewport_output
                .values()
                .any(|v| v.repaint_delay.is_zero());
            out.drop_without_applying_deltas();
            repaint
        };
        let btn = |pos: Pos2, pressed: bool| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        let (a, b) = (Pos2::new(12.0, 8.0), Pos2::new(60.0, 70.0));
        run(vec![]);
        run(vec![Event::PointerMoved(a)]);
        run(vec![btn(a, true)]);
        run(vec![Event::PointerMoved(b)]);
        run(vec![Event::PointerMoved(Pos2::new(61.0, 71.0))]);
        release_repaint |= run(vec![btn(Pos2::new(61.0, 71.0), false)]);
        assert!(
            release_repaint,
            "the release frame must ask for another frame"
        );
        run(vec![]);
        run(vec![]);
        ctx.data_mut(|d| d.remove_temp::<String>(Id::unique(SETTLED)))
    }

    #[test]
    fn a_drag_over_one_paragraph_is_reported() {
        assert!(drag_report("alpha beta gamma delta epsilon").is_some());
    }

    #[test]
    fn a_drag_across_paragraphs_is_reported() {
        let got = drag_report("alpha beta gamma\n\ndelta epsilon zeta\n\neta theta iota");
        assert!(got.is_some(), "{got:?}");
    }

    #[test]
    fn a_drag_across_a_list_and_code_is_reported() {
        let got = drag_report("- one\n- two\n- three\n\n```\ncode here\nmore\n```\n\nend");
        assert!(got.is_some(), "{got:?}");
    }

    #[test]
    fn selection_menu_only_opens_inside_an_open_window() {
        use egui::{Event, RawInput};
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-sel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        ctx.add_plugin(SelectionWatcher);
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let now = time::OffsetDateTime::now_utc();
        let sid = xade_core::SessionId::new();
        let session = xade_core::Session {
            origin: xade_core::SessionOrigin::default(),
            id: sid,
            workspace_id: xade_core::WorkspaceId::new(),
            provider_id: xade_core::ProviderId::Claude,
            provider_session_id: None,
            title: "Test".into(),
            preview_snippet: String::new(),
            model_id: "opus".into(),
            effort: xade_core::Effort::Default,
            sent_model_id: String::new(),
            sent_effort: xade_core::Effort::Default,
            auto_mode: false,
            usage: xade_core::UsageTotals::default(),
            created_at: now,
            updated_at: now,
            multi_session_id: None,
            agent_handle: None,
        };
        let vm = crate::app::SessionVm {
            cwd: dir.clone(),
            session,
            status: xade_core::SessionStatus::Idle,
            queued: Default::default(),
            messages: Vec::new(),
            live_text: String::new(),
            thinking_tokens: None,
            pending_permission: None,
            input: String::new(),
            error: None,
            md_cache: crate::widgets::markdown::new_cache(),
            path_links: Default::default(),
            diff: None,
            changes: Default::default(),
            settings_pending: false,
            turn_base: None,
            turn_snapshotting: false,
            turn_files: Default::default(),
            sent_model: String::new(),
            sent_effort: xade_core::Effort::Default,
            collapsed: false,
            window_rect: None,
            apply_rect: None,
            sync: Default::default(),
            mention: Default::default(),
            attach_note: None,
            focus_input: false,
            subagent_msgs: Vec::new(),
            view_subagents: false,
            fork_preparing: false,
            renaming: None,
            scroll_to_end: false,
        };
        app.sessions.insert(sid, vm);
        app.open_windows.push(WindowKind::Session(sid));
        app.focused = Some(WindowKind::Session(sid));
        app.view = View::Workspace;

        let step = |app: &mut App, events: Vec<Event>| {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1400.0, 800.0))),
                events,
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                crate::home::top_bar(app, ui);
                egui::CentralPanel::default().show(ui, |ui| {
                    crate::workspace::show(app, ui);
                });
                show(app, ui.ctx());
            })
            .drop_without_applying_deltas();
        };
        // Settle layout so the top bar and window layers exist.
        step(&mut app, vec![]);
        step(&mut app, vec![]);
        let win_rect = app.sessions[&sid].window_rect.expect("window rect");

        // Selection settling over the top bar (e.g. search input) must NOT open the menu.
        let top_bar_pos = Pos2::new(300.0, 16.0);
        ctx.data_mut(|d| d.insert_temp(Id::unique(SETTLED), "query".to_string()));
        step(&mut app, vec![Event::PointerMoved(top_bar_pos)]);
        assert!(
            app.selection_menu.is_none(),
            "top bar selection must not open menu"
        );

        // Selection settling inside the open session window DOES open the menu.
        let inside_pos = win_rect.center();
        ctx.data_mut(|d| d.insert_temp(Id::unique(SETTLED), "hello".to_string()));
        step(&mut app, vec![Event::PointerMoved(inside_pos)]);
        assert!(
            app.selection_menu.is_some(),
            "in-window selection must open menu"
        );

        // Switching to Home clears the menu.
        app.view = View::Home;
        step(&mut app, vec![]);
        assert!(app.selection_menu.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

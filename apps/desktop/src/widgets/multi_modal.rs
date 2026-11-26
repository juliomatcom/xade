//! "New Multi-Session" dialog: compose an immutable team of N models across any available
//! providers before launching.

use std::path::PathBuf;

use egui::{Align, Layout, RichText};
use xade_core::{assign_handles, Effort, MultiMemberSpec, ProviderId, WorkspaceId};
use xade_runtime::ManagerCmd;

use crate::app::App;
use crate::{icons, theme};

const MIN_WIDTH: f32 = 760.0;
const COL_HANDLE: f32 = 116.0;
const COL_PROVIDER: f32 = 132.0;
const COL_MODEL: f32 = 228.0;
const COL_EFFORT: f32 = 96.0;
const COL_AUTO: f32 = 96.0;
const COL_REMOVE: f32 = 28.0;
const ROW_HEIGHT: f32 = 24.0;

pub struct MultiCreateState {
    pub workspace_id: WorkspaceId,
    pub cwd: PathBuf,
    pub title: String,
    pub rows: Vec<MultiMemberSpec>,
}

impl MultiCreateState {
    pub fn default_for(app: &App, workspace_id: WorkspaceId, cwd: PathBuf) -> Option<Self> {
        let providers = app.selectable_providers();
        if providers.is_empty() {
            return None;
        }
        let mut rows = Vec::new();
        if providers.len() >= 2 {
            for p in providers.iter().take(2) {
                rows.push(default_spec(app, *p));
            }
        } else {
            let p = providers[0];
            rows.push(default_spec(app, p));
            rows.push(default_spec(app, p));
        }
        Some(Self {
            workspace_id,
            cwd,
            title: String::new(),
            rows,
        })
    }
}

fn default_spec(app: &App, provider_id: ProviderId) -> MultiMemberSpec {
    let models = app.provider_models(provider_id);
    let default_id = app.provider_default_model(provider_id);
    let model_id = if !default_id.is_empty()
        && (models.is_empty() || models.iter().any(|m| m.id == default_id))
    {
        default_id
    } else {
        models.first().map(|m| m.id.clone()).unwrap_or(default_id)
    };
    MultiMemberSpec {
        provider_id,
        model_id,
        effort: Effort::Default,
        auto_mode: true,
    }
}

fn effort_label(e: Effort) -> &'static str {
    match e {
        Effort::Default => "Default",
        Effort::Low => "Low",
        Effort::Medium => "Medium",
        Effort::High => "High",
        Effort::XHigh => "X-High",
        Effort::Max => "Max",
    }
}

fn cell(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ROW_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            add(ui);
        },
    );
}

fn auto_toggle_button(ui: &mut egui::Ui, on: &mut bool) {
    let (text, fill, fg, stroke) = if *on {
        (
            icons::with(icons::AUTO, "Auto: ON"),
            theme::ok_bg(),
            theme::ok(),
            theme::ok(),
        )
    } else {
        (
            icons::with(icons::AUTO, "Auto: OFF"),
            theme::bg_hover(),
            theme::text(),
            theme::border(),
        )
    };
    let btn = egui::Button::new(RichText::new(text).small().color(fg))
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(3)
        .min_size(egui::vec2(COL_AUTO, ROW_HEIGHT));
    if ui
        .add(btn)
        .on_hover_text("Auto-approve tool calls for this model")
        .clicked()
    {
        *on = !*on;
    }
}

/// Draws the creation modal when open; Start sends `ManagerCmd::CreateMulti`.
pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(mut st) = app.multi_create.take() else {
        return;
    };
    let providers = app.selectable_providers();
    let ws_open = app
        .home
        .workspaces
        .iter()
        .any(|w| w.id == st.workspace_id && w.is_open);
    if providers.is_empty() || !ws_open {
        return;
    }
    let ws_name = app.workspace_name(st.workspace_id);
    let model_ids: Vec<&str> = st.rows.iter().map(|r| r.model_id.as_str()).collect();
    let handles = assign_handles(&model_ids);
    let show_provider = providers.len() > 1;
    let model_col = if show_provider {
        COL_MODEL
    } else {
        COL_MODEL + COL_PROVIDER + 8.0
    };
    let mut start = false;
    let mut cancel = false;
    let mut add_row = false;
    let mut remove_idx = None;

    let resp =
        crate::widgets::modal(ctx, egui::Id::unique("create_multi_session")).show(ctx, |ui| {
            ui.set_min_width(MIN_WIDTH);
            ui.label(RichText::new("New Multi-Session").strong().size(18.0));
            ui.label(
                RichText::new(format!(
                    "Workspace: {ws_name} \u{2022} Models cannot be added or removed after start."
                ))
                .small()
                .color(theme::text_muted()),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label("Title:");
                ui.add(
                    egui::TextEdit::singleline(&mut st.title)
                        .hint_text("Optional (derived from first message if empty)")
                        .desired_width(f32::INFINITY),
                );
            });
            ui.add_space(12.0);
            ui.label(RichText::new("Team members").strong());
            ui.add_space(4.0);

            // Column headers aligned with the member rows below.
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(10, 0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let hdr = |ui: &mut egui::Ui, w: f32, text: &str| {
                            cell(ui, w, |ui| {
                                ui.label(RichText::new(text).small().color(theme::text_muted()));
                            });
                        };
                        hdr(ui, COL_HANDLE, "Handle");
                        if show_provider {
                            hdr(ui, COL_PROVIDER, "Provider");
                        }
                        hdr(ui, model_col, "Model");
                        hdr(ui, COL_EFFORT, "Thinking");
                        hdr(ui, COL_AUTO, "Auto");
                    });
                });
            ui.add_space(2.0);

            let can_remove = st.rows.len() > 1;
            egui::ScrollArea::vertical()
                .max_height(380.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (i, row) in st.rows.iter_mut().enumerate() {
                        let handle = handles.get(i).map_or("agent", String::as_str);
                        egui::Frame::new()
                            .fill(theme::bg_raised())
                            .stroke(egui::Stroke::new(1.0, theme::border()))
                            .corner_radius(4)
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    cell(ui, COL_HANDLE, |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(format!("#{handle}"))
                                                    .strong()
                                                    .color(theme::accent()),
                                            )
                                            .truncate(),
                                        );
                                    });
                                    if show_provider {
                                        cell(ui, COL_PROVIDER, |ui| {
                                            let prev = row.provider_id;
                                            egui::ComboBox::from_id_salt(("multi_prov", i))
                                                .selected_text(app.provider_name(row.provider_id))
                                                .width(COL_PROVIDER)
                                                .truncate()
                                                .show_ui(ui, |ui| {
                                                    for &pid in &providers {
                                                        ui.selectable_value(
                                                            &mut row.provider_id,
                                                            pid,
                                                            app.provider_name(pid),
                                                        );
                                                    }
                                                });
                                            if row.provider_id != prev {
                                                let next = default_spec(app, row.provider_id);
                                                row.model_id = next.model_id;
                                                row.effort = Effort::Default;
                                            }
                                        });
                                    }
                                    let models = app.provider_models(row.provider_id);
                                    let model_label = models
                                        .iter()
                                        .find(|m| m.id == row.model_id)
                                        .map_or_else(|| row.model_id.clone(), |m| m.label.clone());
                                    cell(ui, model_col, |ui| {
                                        egui::ComboBox::from_id_salt(("multi_model", i))
                                            .selected_text(model_label)
                                            .width(model_col)
                                            .truncate()
                                            .show_ui(ui, |ui| {
                                                for m in &models {
                                                    ui.selectable_value(
                                                        &mut row.model_id,
                                                        m.id.clone(),
                                                        &m.label,
                                                    );
                                                }
                                            });
                                    });
                                    let efforts =
                                        app.model_effort_levels(row.provider_id, &row.model_id);
                                    if !efforts.contains(&row.effort) {
                                        row.effort = Effort::Default;
                                    }
                                    cell(ui, COL_EFFORT, |ui| {
                                        egui::ComboBox::from_id_salt(("multi_effort", i))
                                            .selected_text(effort_label(row.effort))
                                            .width(COL_EFFORT)
                                            .truncate()
                                            .show_ui(ui, |ui| {
                                                for e in efforts {
                                                    ui.selectable_value(
                                                        &mut row.effort,
                                                        e,
                                                        effort_label(e),
                                                    );
                                                }
                                            });
                                    });
                                    cell(ui, COL_AUTO, |ui| {
                                        auto_toggle_button(ui, &mut row.auto_mode);
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui
                                            .add_enabled(
                                                can_remove,
                                                egui::Button::new(icons::CLOSE)
                                                    .min_size(egui::vec2(COL_REMOVE, ROW_HEIGHT)),
                                            )
                                            .on_hover_text("Remove model")
                                            .clicked()
                                        {
                                            remove_idx = Some(i);
                                        }
                                    });
                                });
                            });
                        ui.add_space(6.0);
                    }
                });

            ui.add_space(4.0);
            if ui
                .button(icons::with(icons::NEW_SESSION, "Add model"))
                .clicked()
            {
                add_row = true;
            }

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let can_start =
                    !st.rows.is_empty() && st.rows.iter().all(|r| !r.model_id.is_empty());
                start = ui
                    .add_enabled(
                        can_start,
                        egui::Button::new(
                            RichText::new("Start Multi-Session").color(theme::text()),
                        )
                        .fill(theme::accent()),
                    )
                    .clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });

    if let Some(idx) = remove_idx {
        if st.rows.len() > 1 && idx < st.rows.len() {
            st.rows.remove(idx);
        }
    }
    if add_row {
        let pid = st.rows.last().map_or(providers[0], |r| r.provider_id);
        st.rows.push(default_spec(app, pid));
    }

    let valid = !st.rows.is_empty() && st.rows.iter().all(|r| !r.model_id.is_empty());
    if start && valid {
        let _ = app.cmd_tx.send(ManagerCmd::CreateMulti {
            workspace_id: st.workspace_id,
            cwd: st.cwd,
            title: st.title,
            members: st.rows,
        });
        return;
    }
    if cancel || resp.should_close() {
        return;
    }
    app.multi_create = Some(st);
}

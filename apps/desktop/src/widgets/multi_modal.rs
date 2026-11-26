//! "New Multi-Session" dialog: compose an immutable team of N models across any available
//! providers before launching.

use std::path::PathBuf;

use egui::RichText;
use xade_core::{assign_handles, Effort, MultiMemberSpec, ProviderId, WorkspaceId};
use xade_runtime::ManagerCmd;

use crate::app::App;
use crate::{icons, theme};

const MIN_WIDTH: f32 = 580.0;

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
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Title:");
                ui.add(
                    egui::TextEdit::singleline(&mut st.title)
                        .hint_text("Optional (derived from first message if empty)")
                        .desired_width(f32::INFINITY),
                );
            });
            ui.add_space(8.0);
            ui.label(RichText::new("Team members").strong());
            ui.add_space(4.0);

            let can_remove = st.rows.len() > 1;
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (i, row) in st.rows.iter_mut().enumerate() {
                        let handle = handles.get(i).map_or("agent", String::as_str);
                        egui::Frame::new()
                            .fill(theme::bg_raised())
                            .stroke(egui::Stroke::new(1.0, theme::border()))
                            .corner_radius(6)
                            .inner_margin(egui::Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!("#{handle}"))
                                            .strong()
                                            .color(theme::accent()),
                                    );
                                    if providers.len() > 1 {
                                        let prev = row.provider_id;
                                        egui::ComboBox::from_id_salt(("multi_prov", i))
                                            .selected_text(app.provider_name(row.provider_id))
                                            .width(105.0)
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
                                    }
                                    let models = app.provider_models(row.provider_id);
                                    let model_label = models
                                        .iter()
                                        .find(|m| m.id == row.model_id)
                                        .map_or_else(|| row.model_id.clone(), |m| m.label.clone());
                                    egui::ComboBox::from_id_salt(("multi_model", i))
                                        .selected_text(model_label)
                                        .width(180.0)
                                        .show_ui(ui, |ui| {
                                            for m in &models {
                                                ui.selectable_value(
                                                    &mut row.model_id,
                                                    m.id.clone(),
                                                    &m.label,
                                                );
                                            }
                                        });
                                    let efforts =
                                        app.model_effort_levels(row.provider_id, &row.model_id);
                                    if !efforts.contains(&row.effort) {
                                        row.effort = Effort::Default;
                                    }
                                    egui::ComboBox::from_id_salt(("multi_effort", i))
                                        .selected_text(effort_label(row.effort))
                                        .width(78.0)
                                        .show_ui(ui, |ui| {
                                            for e in efforts {
                                                ui.selectable_value(
                                                    &mut row.effort,
                                                    e,
                                                    effort_label(e),
                                                );
                                            }
                                        });
                                    ui.checkbox(&mut row.auto_mode, "Auto")
                                        .on_hover_text("Auto-approve tool calls for this model");
                                    if ui
                                        .add_enabled(can_remove, egui::Button::new(icons::CLOSE))
                                        .on_hover_text("Remove model")
                                        .clicked()
                                    {
                                        remove_idx = Some(i);
                                    }
                                });
                            });
                        ui.add_space(4.0);
                    }
                });

            ui.add_space(4.0);
            if ui
                .button(icons::with(icons::NEW_SESSION, "Add model"))
                .clicked()
            {
                add_row = true;
            }

            ui.add_space(12.0);
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

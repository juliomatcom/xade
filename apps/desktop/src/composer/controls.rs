//! The row under the input: attach button, model and effort pickers, Auto pill and stats.

use egui::RichText;
use xade_core::{Effort, ModelOption};
use xade_runtime::ManagerCmd;

use super::meters::{context_donut, usage_text};
use crate::app::SessionVm;
use crate::workspace::Env;
use crate::{icons, theme};

/// Returns whether the attach button was clicked.
pub(super) fn controls(ui: &mut egui::Ui, vm: &mut SessionVm, env: &Env) -> bool {
    let id = vm.session.id;
    let provider = env
        .providers
        .iter()
        .find(|p| p.id() == vm.session.provider_id);
    let models = provider.map(|p| p.models()).unwrap_or_default();
    let fallback = provider.map(|p| p.effort_levels()).unwrap_or_default();
    let efforts = effort_choices(&models, &vm.session.model_id, &fallback);
    ui.horizontal(|ui| {
        // As tall as the other controls; a big "+" in a normal button would stretch the row, so
        // the glyph is painted inside a control-height box.
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(30.0, ui.spacing().interact_size.y),
            egui::Sense::click(),
        );
        let visuals = ui.style().interact(&resp);
        ui.painter().rect(
            rect,
            visuals.corner_radius,
            visuals.weak_bg_fill,
            visuals.bg_stroke,
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "+",
            egui::FontId::proportional(20.0),
            visuals.fg_stroke.color,
        );
        let attach = resp
            .on_hover_text("Attach files (or type @ or drop files here): adds @path mentions")
            .clicked();
        ui.label(RichText::new("Model").small().color(theme::text_muted()));
        let current = models
            .iter()
            .find(|m| m.id == vm.session.model_id)
            .map_or(vm.session.model_id.clone(), |m| m.label.clone());
        egui::ComboBox::from_id_salt(("model", id))
            .selected_text(current)
            .width(110.0)
            .show_ui(ui, |ui| {
                for m in &models {
                    let picked = ui
                        .selectable_label(vm.session.model_id == m.id, &m.label)
                        .on_hover_text(&m.description)
                        .clicked();
                    if picked && vm.session.model_id != m.id {
                        vm.session.model_id = m.id.clone();
                        vm.settings_pending = true;
                        let _ = env.cmd_tx.send(ManagerCmd::SetModel {
                            session_id: id,
                            model_id: m.id.clone(),
                        });
                        let supported = effort_choices(&models, &m.id, &fallback);
                        let effort = reconcile_effort(vm.session.effort, &supported);
                        if effort != vm.session.effort {
                            vm.session.effort = effort;
                            let _ = env.cmd_tx.send(ManagerCmd::SetEffort {
                                session_id: id,
                                effort,
                            });
                        }
                    }
                }
            });
        ui.label(RichText::new("Thinking").small().color(theme::text_muted()));
        egui::ComboBox::from_id_salt(("effort", id))
            .selected_text(effort_label(vm.session.effort))
            .width(80.0)
            .show_ui(ui, |ui| {
                for e in &efforts {
                    let picked = ui
                        .selectable_label(vm.session.effort == *e, effort_label(*e))
                        .clicked();
                    if picked && vm.session.effort != *e {
                        vm.session.effort = *e;
                        vm.settings_pending = true;
                        let _ = env.cmd_tx.send(ManagerCmd::SetEffort {
                            session_id: id,
                            effort: *e,
                        });
                    }
                }
            });
        let on = vm.session.auto_mode;
        let (text, fill, fg, stroke) = if on {
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
        let pill = egui::Button::new(RichText::new(text).small().color(fg))
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, stroke))
            .corner_radius(3);
        if ui.add(pill).clicked() {
            vm.session.auto_mode = !on;
            let _ = env.cmd_tx.send(ManagerCmd::SetAutoMode {
                session_id: id,
                on: !on,
            });
        }
        if vm.settings_pending {
            ui.label(
                RichText::new("applies to next turn")
                    .small()
                    .color(theme::warn()),
            );
        }
        // Session stats take the rest of the row, right-aligned; they are cut with "…" when
        // narrow (never a second row) and show in full on hover.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let u = &vm.session.usage;
            context_donut(ui, u);
            let stats = usage_text(u);
            ui.add(
                egui::Label::new(RichText::new(&stats).small().color(theme::text_muted()))
                    .truncate(),
            )
            .on_hover_text(stats);
        });
        attach
    })
    .inner
}

/// `Default` plus the levels of the model with this exact id; `fallback` when the id is unknown.
pub(crate) fn effort_choices(
    models: &[ModelOption],
    model_id: &str,
    fallback: &[Effort],
) -> Vec<Effort> {
    match models.iter().find(|m| m.id == model_id) {
        Some(m) => std::iter::once(Effort::Default)
            .chain(
                m.effort_levels
                    .iter()
                    .copied()
                    .filter(|e| *e != Effort::Default),
            )
            .collect(),
        None => fallback.to_vec(),
    }
}

/// Keeps `current` when supported, else falls back to `Default`.
fn reconcile_effort(current: Effort, supported: &[Effort]) -> Effort {
    if supported.contains(&current) {
        current
    } else {
        Effort::Default
    }
}

pub(super) fn effort_label(e: Effort) -> &'static str {
    match e {
        Effort::Default => "Default",
        Effort::Low => "Low",
        Effort::Medium => "Medium",
        Effort::High => "High",
        Effort::XHigh => "X-High",
        Effort::Max => "Max",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn effort_follows_the_model() {
        let models = [ModelOption {
            id: "m-1".into(),
            label: "M 1".into(),
            description: String::new(),
            effort_levels: vec![Effort::Low, Effort::High],
        }];
        let all = [Effort::Default, Effort::Low, Effort::Medium];
        assert_eq!(
            effort_choices(&models, "m-1", &all),
            [Effort::Default, Effort::Low, Effort::High]
        );
        assert_eq!(effort_choices(&models, "old-alias", &all), all);
        let supported = effort_choices(&models, "m-1", &all);
        assert_eq!(reconcile_effort(Effort::High, &supported), Effort::High);
        assert_eq!(reconcile_effort(Effort::Max, &supported), Effort::Default);
    }
}

//! The scrolling transcript of one session.

use std::collections::HashMap;

use aw_core::{Message, Role, SessionStatus};
use egui::RichText;
use egui_commonmark::CommonMarkCache;

use crate::app::SessionVm;
use crate::theme;
use crate::widgets::markdown::markdown;
use crate::widgets::tool_card::{self, CardStatus};

pub fn show(ui: &mut egui::Ui, vm: &mut SessionVm) {
    let mut review = None;
    let SessionVm {
        messages,
        md_cache,
        live_text,
        thinking_tokens,
        status,
        pending_permission,
        error,
        ..
    } = &mut *vm;
    let results: HashMap<&str, &Message> = messages
        .iter()
        .filter(|m| m.role == Role::ToolResult)
        .filter_map(|m| Some((m.tool_use_id.as_deref()?, m)))
        .collect();
    let busy = matches!(
        status,
        SessionStatus::Streaming | SessionStatus::NeedsApproval
    );
    egui::ScrollArea::vertical()
        .id_salt("conversation")
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for m in messages.iter() {
                        match m.role {
                            Role::User => user_bubble(ui, md_cache, m),
                            Role::Assistant => assistant(ui, md_cache, m.id, &m.content_text),
                            Role::ToolCall => {
                                let result = m.tool_use_id.as_deref().and_then(|t| results.get(t));
                                let card_status = match result {
                                    Some(r) if r.is_error => CardStatus::Error,
                                    Some(_) => CardStatus::Ok,
                                    None if busy => CardStatus::Running,
                                    None => CardStatus::Unknown,
                                };
                                let awaiting = pending_permission.as_ref().is_some_and(|p| {
                                    m.tool_use_id.as_deref() == Some(&*p.tool_use_id)
                                });
                                if tool_card::show(
                                    ui,
                                    md_cache,
                                    m,
                                    result.copied(),
                                    card_status,
                                    awaiting,
                                ) {
                                    review = m.tool_use_id.clone();
                                }
                            }
                            Role::System => {
                                ui.label(RichText::new(&m.content_text).color(theme::TEXT_MUTED));
                            }
                            Role::ToolResult => {}
                        }
                        ui.add_space(6.0);
                    }
                    if !live_text.is_empty() {
                        assistant(ui, md_cache, "live", live_text);
                    }
                    if let (Some(n), true) = (*thinking_tokens, live_text.is_empty()) {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(12.0));
                            ui.label(
                                RichText::new(format!("Thinking… ~{n} tokens"))
                                    .color(theme::TEXT_MUTED),
                            );
                        });
                    }
                    if let Some(e) = error {
                        ui.label(RichText::new(&*e).color(theme::DANGER));
                    }
                });
        });
    if let Some(id) = review {
        vm.open_diff(&id);
    }
}

fn user_bubble(ui: &mut egui::Ui, cache: &mut CommonMarkCache, m: &Message) {
    let indent = ui.available_width() * 0.15;
    ui.horizontal_top(|ui| {
        ui.add_space(indent);
        egui::Frame::new()
            .fill(theme::BG_RAISED)
            .stroke(egui::Stroke::new(1.0, theme::ACCENT))
            .corner_radius(8)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                markdown(ui, cache, m.id, &m.content_text);
            });
    });
}

fn assistant(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &str,
) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 4.0, theme::CLAUDE);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "A",
            egui::FontId::proportional(12.0),
            theme::BG_APP,
        );
        ui.vertical(|ui| markdown(ui, cache, id, text));
    });
}

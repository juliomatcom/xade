//! Reusable pieces of the session windows.

pub mod about_modal;
pub mod changes_summary;
pub mod delete_modal;
pub mod diff_modal;
pub mod file_picker;
pub mod help_modal;
pub mod keymaps_modal;
pub mod markdown;
pub mod multi_modal;
pub mod path_links;
pub mod quit_modal;
pub mod scroll_nav;
pub mod selection_menu;
pub mod session_picker;
pub mod status_modal;
pub mod tool_card;
pub mod working;

/// Space between a modal's edge and its content, so dialogs do not feel tight.
pub const MODAL_MARGIN: i8 = 24;

/// A modal with the project's padding; every dialog starts here.
pub fn modal(ctx: &egui::Context, id: egui::Id) -> egui::Modal {
    egui::Modal::new(id).frame(
        egui::Frame::popup(&ctx.global_style()).inner_margin(egui::Margin::same(MODAL_MARGIN)),
    )
}

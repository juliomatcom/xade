//! The diff review modal and the VS Code launcher.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use egui::{Align, Color32, Layout, RichText, TextStyle};
use egui_extras::syntax_highlighting::{highlight_with, CodeTheme};

use crate::diff::{Cell, DiffData, Kind, Row};
use crate::editor::SYNTAX;
use crate::{icons, theme};

const GUTTER: f32 = 44.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModalAction {
    None,
    Accept,
    AcceptAuto,
    Cancel,
    /// Hide the modal without deciding.
    Close,
    /// Hide the modal and open the file in Xade's editor.
    OpenInXade,
}

/// `code` binary: `$PATH`, then the usual macOS locations (GUI apps lack the shell PATH).
pub fn find_code() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from(
        "/Applications/Visual Studio Code.app/Contents/Resources/app/bin",
    ));
    dirs.into_iter()
        .map(|d| d.join("code"))
        .find(|p| p.is_file())
}

/// Runs `code --goto <file>:<line>` without waiting for it.
pub fn open_in_vscode(code: &Path, file: &Path, line: usize) {
    let mut cmd = Command::new(code);
    cmd.arg("--goto").arg(format!("{}:{line}", file.display()));
    spawn_detached(cmd);
}

/// Starts `cmd` with no stdio and reaps it in the background.
pub fn spawn_detached(mut cmd: Command) {
    let spawned = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        // Reap the launcher so it does not linger as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(error = %e, "could not start {:?}", cmd.get_program()),
    }
}

/// Shows the modal. `can_decide` adds Accept / Accept & Enable Auto / Cancel; otherwise it is read-only.
pub fn show(
    ctx: &egui::Context,
    id: egui::Id,
    data: &DiffData,
    rel_path: &str,
    can_decide: bool,
    vscode: Option<&Path>,
) -> ModalAction {
    let mut action = ModalAction::None;
    let size = ctx.content_rect().size() * 0.8;
    let resp = egui::Modal::new(id).show(ctx, |ui| {
        ui.set_min_size(size);
        ui.set_max_size(size);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Review File Changes: {rel_path}")).strong());
            if ui
                .button(icons::with(icons::FILE, "Open in Xade"))
                .clicked()
            {
                action = ModalAction::OpenInXade;
            }
            if let Some(code) = vscode {
                if ui
                    .button(icons::with(icons::VSCODE, "Open in VS Code"))
                    .clicked()
                {
                    open_in_vscode(code, &data.path, data.first_changed_line);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if can_decide {
                    if ui.button(icons::with(icons::CANCEL, "Cancel")).clicked() {
                        action = ModalAction::Cancel;
                    }
                    let accept = |t: &str| {
                        egui::Button::new(RichText::new(t).color(theme::TEXT)).fill(theme::ACCENT)
                    };
                    if ui
                        .add(accept(&icons::with(icons::ACCEPT, "Accept")))
                        .clicked()
                    {
                        action = ModalAction::Accept;
                    }
                    if ui
                        .add(accept(&icons::with(icons::ACCEPT, "Accept & Enable Auto")))
                        .clicked()
                    {
                        action = ModalAction::AcceptAuto;
                    }
                } else if ui.button(icons::with(icons::CLOSE, "Close")).clicked() {
                    action = ModalAction::Close;
                }
            });
        });
        ui.separator();
        if data.too_large {
            ui.label(RichText::new("File too large to diff").color(theme::TEXT_MUTED));
        } else {
            body(ui, id, data);
        }
    });
    if action == ModalAction::None && resp.should_close() {
        action = ModalAction::Close;
    }
    if action != ModalAction::None {
        // The next open of this file starts at its first change again.
        ctx.data_mut(|d| d.remove::<bool>(scrolled_key(id, data)));
    }
    action
}

/// Index of the first row holding a change, backed up to leave a little context above it.
fn first_change_row(data: &DiffData) -> usize {
    let changed = |c: &Option<Cell>| c.as_ref().is_some_and(|c| c.kind != Kind::Same);
    data.rows
        .iter()
        .position(|r| matches!(r, Row::Pair(l, r) if changed(l) || changed(r)))
        .map_or(0, |i| i.saturating_sub(3))
}

fn scrolled_key(id: egui::Id, data: &DiffData) -> egui::Id {
    egui::Id::unique(("diff_scrolled", id, &data.path))
}

fn body(ui: &mut egui::Ui, id: egui::Id, data: &DiffData) {
    let row_h = ui.text_style_height(&TextStyle::Monospace) + 4.0;
    let code_theme = CodeTheme::from_style(ui.style());
    // Syntect matches a syntax by extension (or file name), not by language name.
    let token = data
        .path
        .extension()
        .or_else(|| data.path.file_name())
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let area_h = ui.available_height();
    // Jump to the first change on the first frame only; later frames keep the user's scroll.
    let started = scrolled_key(id, data);
    let first_frame = !ui.data(|d| d.get_temp::<bool>(started)).unwrap_or(false);
    ui.data_mut(|d| d.insert_temp(started, true));
    let mut area = egui::ScrollArea::vertical().id_salt(("diff_rows", &data.path));
    if first_frame {
        area = area.vertical_scroll_offset(
            first_change_row(data) as f32 * (row_h + ui.spacing().item_spacing.y),
        );
    }
    area.auto_shrink([false, false])
        .max_height(area_h)
        .show_rows(ui, row_h, data.rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let width = ui.available_width();
            let half = (width - 8.0) / 2.0;
            for row in &data.rows[range] {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(width, row_h), egui::Sense::hover());
                match row {
                    Row::Sep(label) => {
                        ui.painter().text(
                            rect.left_center(),
                            egui::Align2::LEFT_CENTER,
                            label,
                            TextStyle::Small.resolve(ui.style()),
                            theme::TEXT_MUTED,
                        );
                    }
                    Row::Pair(l, r) => {
                        let left = egui::Rect::from_min_size(rect.min, egui::vec2(half, row_h));
                        let right = egui::Rect::from_min_size(
                            egui::pos2(rect.min.x + half + 8.0, rect.min.y),
                            egui::vec2(half, row_h),
                        );
                        paint_cell(ui, left, l.as_ref(), &token, &code_theme);
                        paint_cell(ui, right, r.as_ref(), &token, &code_theme);
                    }
                }
            }
        });
}

fn paint_cell(
    ui: &egui::Ui,
    rect: egui::Rect,
    cell: Option<&Cell>,
    language: &str,
    code_theme: &CodeTheme,
) {
    let Some(cell) = cell else {
        ui.painter().rect_filled(rect, 0.0, theme::BG_APP);
        return;
    };
    let bg = match cell.kind {
        Kind::Same => Color32::TRANSPARENT,
        Kind::Del => theme::DIFF_DEL_BG,
        Kind::Add => theme::DIFF_ADD_BG,
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, bg);
    let font = TextStyle::Monospace.resolve(ui.style());
    painter.text(
        egui::pos2(rect.left() + GUTTER - 6.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        cell.line_no.to_string(),
        font,
        theme::TEXT_MUTED,
    );
    let job = highlight_with(
        ui.ctx(),
        ui.style(),
        code_theme,
        &cell.text,
        language,
        &SYNTAX,
    );
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let pos = egui::pos2(
        rect.left() + GUTTER,
        rect.center().y - galley.size().y / 2.0,
    );
    painter.galley(pos, galley, theme::TEXT);
}

//! Editor windows: one text file per window, tiled in the workspace grid beside the sessions.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime};

use egui::{RichText, TextStyle};
use egui_commonmark::CommonMarkCache;
use egui_extras::syntax_highlighting::{highlight_with, CodeTheme, SyntectSettings};

use crate::session_view::ViewOut;
use crate::widgets::markdown::preview;
use crate::widgets::tool_card::language_from_path;
use crate::{chrome, theme};

/// Larger text files are refused.
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// Larger image files are refused.
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// How often open files are compared with their mtime on disk.
pub const DISK_POLL: Duration = Duration::from_secs(2);
const IMAGE_EXTS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico"];

/// Whether `path` has a raster image extension supported by the file window.
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTS.iter().any(|ext| ext.eq_ignore_ascii_case(e)))
}

/// Content read off the UI thread when opening a file.
pub enum FileContent {
    Text(String),
    Image(egui::ColorImage),
}

/// Picture state when a file window displays an image instead of text.
pub struct ImageState {
    /// Decoded pixels waiting for GPU upload on the next frame; cleared right after upload.
    pending: Option<egui::ColorImage>,
    texture: Option<egui::TextureHandle>,
    /// Pixel dimensions `[width, height]`.
    pub size: [usize; 2],
    /// `false` fits to the window; `true` shows 1:1 pixels inside a scroll area.
    pub actual_size: bool,
    /// A background decode after an on-disk mtime change is in flight.
    pub reloading: bool,
}

impl ImageState {
    fn new(pixels: egui::ColorImage) -> Self {
        let size = pixels.size;
        ImageState {
            pending: Some(pixels),
            texture: None,
            size,
            actual_size: false,
            reloading: false,
        }
    }
}

/// bat's syntax set (two-face): far more languages than syntect's defaults. One static, since
/// egui memoizes highlighting by the settings' address.
pub static SYNTAX: LazyLock<SyntectSettings> = LazyLock::new(|| SyntectSettings {
    ps: two_face::syntax::extra_newlines(),
    ..SyntectSettings::default()
});

/// Loads `SYNTAX` on a background worker once per process so the first opened editor or diff
/// modal never stalls a frame.
pub fn warm_syntax(rt: &tokio::runtime::Handle) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        rt.spawn_blocking(|| {
            LazyLock::force(&SYNTAX);
        });
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EditorId(pub u32);

pub struct EditorVm {
    pub id: EditorId,
    pub path: Option<PathBuf>,
    /// The N of "Untitled-N.md" while there is no path.
    pub untitled: u32,
    pub text: String,
    pub dirty: bool,
    /// Bumped on every edit; a save clears `dirty` only if it saw the latest one.
    pub revision: u64,
    /// A save is in flight.
    pub busy: bool,
    pub close_after_save: bool,
    pub preview: bool,
    pub md_cache: CommonMarkCache,
    pub disk_mtime: Option<SystemTime>,
    /// The file changed on disk while this editor had unsaved edits.
    pub disk_changed: bool,
    pub collapsed: bool,
    pub window_rect: Option<egui::Rect>,
    pub apply_rect: Option<egui::Rect>,
    /// 1-based inclusive lines of the last non-empty selection; kept while focus is elsewhere.
    pub selection: Option<(usize, usize)>,
    /// Present when this window displays an image rather than text.
    pub image: Option<ImageState>,
}

impl EditorVm {
    pub fn title(&self) -> String {
        match &self.path {
            Some(p) => p
                .file_name()
                .map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into()),
            None => format!("Untitled-{}.md", self.untitled),
        }
    }

    fn language(&self) -> &'static str {
        self.path
            .as_deref()
            .map_or("md", |p| language_from_path(&p.to_string_lossy()))
    }

    fn is_markdown(&self) -> bool {
        self.language() == "md"
    }

    /// What syntect matches a syntax by: the extension, else the file name (`Dockerfile`).
    fn syntax_token(&self) -> String {
        let Some(path) = self.path.as_deref() else {
            return "md".into();
        };
        path.extension()
            .or_else(|| path.file_name())
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
    }

    /// Replaces the text with the file's current content.
    pub fn reload(&mut self) -> Result<(), String> {
        let path = self.path.clone().ok_or("no file to reload")?;
        self.text = read_file(&path)?;
        self.dirty = false;
        self.revision += 1;
        self.disk_changed = false;
        self.disk_mtime = mtime(&path);
        Ok(())
    }

    pub fn is_image(&self) -> bool {
        self.image.is_some()
    }

    /// Replaces the displayed image after an off-thread reload, keeping the zoom choice.
    pub fn set_image(&mut self, pixels: egui::ColorImage) {
        let size = pixels.size;
        if let Some(img) = self.image.as_mut() {
            img.size = size;
            img.pending = Some(pixels);
            img.reloading = false;
        } else {
            self.image = Some(ImageState::new(pixels));
        }
        self.revision += 1;
        self.disk_changed = false;
        self.disk_mtime = self.path.as_deref().and_then(mtime);
    }

    /// Returns the path to decode off-thread when an open image's mtime changed on disk.
    pub fn image_needs_reload(&mut self) -> Option<PathBuf> {
        let img = self.image.as_mut()?;
        if img.reloading {
            return None;
        }
        let path = self.path.as_ref()?;
        let now = mtime(path)?;
        if Some(now) == self.disk_mtime {
            return None;
        }
        img.reloading = true;
        self.disk_mtime = Some(now);
        Some(path.clone())
    }

    /// Notices a file changed by someone else: clean editors reload, dirty ones flag it.
    /// Images reload asynchronously via `image_needs_reload`.
    pub fn check_disk(&mut self) -> Result<(), String> {
        if self.is_image() {
            return Ok(());
        }
        let Some(now) = self.path.as_deref().and_then(mtime) else {
            return Ok(());
        };
        if self.busy || Some(now) == self.disk_mtime {
            return Ok(());
        }
        if self.dirty {
            self.disk_changed = true;
            Ok(())
        } else {
            self.reload()
        }
    }
}

/// All editors plus the state around them.
#[derive(Default)]
pub struct Editors {
    pub map: HashMap<EditorId, EditorVm>,
    next_id: u32,
    next_untitled: u32,
    /// The editor the "Save changes?" modal is about.
    pub pending_close: Option<EditorId>,
    pub checked: Option<Instant>,
    /// The editor window focused most recently; sessions are told about its file.
    pub last_focused: Option<EditorId>,
}

impl Editors {
    pub fn create(&mut self, path: Option<PathBuf>, text: String) -> EditorId {
        self.next_id += 1;
        let id = EditorId(self.next_id);
        let untitled = if path.is_none() {
            self.next_untitled += 1;
            self.next_untitled
        } else {
            0
        };
        let disk_mtime = path.as_deref().and_then(mtime);
        self.map.insert(
            id,
            EditorVm {
                id,
                path,
                untitled,
                text,
                dirty: false,
                revision: 0,
                busy: false,
                close_after_save: false,
                preview: false,
                md_cache: crate::widgets::markdown::new_cache(),
                disk_mtime,
                disk_changed: false,
                collapsed: false,
                window_rect: None,
                apply_rect: None,
                selection: None,
                image: None,
            },
        );
        id
    }

    pub fn create_image(&mut self, path: PathBuf, pixels: egui::ColorImage) -> EditorId {
        self.next_id += 1;
        let id = EditorId(self.next_id);
        let disk_mtime = mtime(&path);
        self.map.insert(
            id,
            EditorVm {
                id,
                path: Some(path),
                untitled: 0,
                text: String::new(),
                dirty: false,
                revision: 0,
                busy: false,
                close_after_save: false,
                preview: false,
                md_cache: crate::widgets::markdown::new_cache(),
                disk_mtime,
                disk_changed: false,
                collapsed: false,
                window_rect: None,
                apply_rect: None,
                selection: None,
                image: Some(ImageState::new(pixels)),
            },
        );
        id
    }

    /// The context tag for the most recently focused editor that has a file; `None` when that
    /// editor was closed or never saved.
    pub fn ide_context(&self) -> Option<String> {
        let vm = self.map.get(&self.last_focused?)?;
        Some(ide_context(vm.path.as_deref()?, vm.selection))
    }

    pub fn find_path(&self, path: &Path) -> Option<EditorId> {
        self.map
            .values()
            .find(|e| e.path.as_deref() == Some(path))
            .map(|e| e.id)
    }
}

pub enum SaveOutcome {
    Saved(PathBuf),
    /// The save dialog was dismissed.
    Cancelled,
    Failed(String),
}

pub fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Reads a UTF-8 text file of at most `MAX_FILE_BYTES`.
pub fn read_file(path: &Path) -> Result<String, String> {
    let name = display_name(path);
    let meta = std::fs::metadata(path).map_err(|e| format!("Cannot open {name}: {e}"))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("{name} is larger than 5 MB"));
    }
    std::fs::read_to_string(path).map_err(|e| format!("Cannot open {name}: {e}"))
}

/// Decodes a raster image of at most `MAX_IMAGE_BYTES` into RGBA pixels (blocking).
pub fn read_image(path: &Path) -> Result<egui::ColorImage, String> {
    let name = display_name(path);
    let meta = std::fs::metadata(path).map_err(|e| format!("Cannot open {name}: {e}"))?;
    if meta.len() > MAX_IMAGE_BYTES {
        let mb = MAX_IMAGE_BYTES / (1024 * 1024);
        return Err(format!("{name} is larger than {mb} MB"));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Cannot open {name}: {e}"))?;
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("Cannot open {name}: {e}"))?;
    let rgba = reader
        .decode()
        .map_err(|e| format!("Cannot open {name}: {e}"))?
        .into_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        rgba.as_raw(),
    ))
}

/// Reads `path` as an image when its extension is a supported raster format, else as UTF-8 text.
pub fn read_entry(path: &Path) -> Result<FileContent, String> {
    if is_image_path(path) {
        read_image(path).map(FileContent::Image)
    } else {
        read_file(path).map(FileContent::Text)
    }
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Writes through a temp file in the same directory, then renames it over `path`.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let tmp = dir.join(format!(
        ".{}.xade-{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        if let Ok(meta) = std::fs::metadata(path) {
            if let Err(e) = std::fs::set_permissions(&tmp, meta.permissions()) {
                tracing::warn!(error = %e, "could not keep the file's permissions");
            }
        }
        std::fs::rename(&tmp, path)
    };
    let res = write();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

pub struct EditorOut {
    pub view: ViewOut,
    pub error: Option<String>,
}

/// 1-based inclusive lines covered by the char range, `None` when it selects nothing. A range that
/// ends at the very start of a line does not count that line.
pub fn selected_lines(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    if start >= end {
        return None;
    }
    let line_of = |index: usize| 1 + text.chars().take(index).filter(|c| *c == '\n').count();
    let first = line_of(start);
    let last = line_of(end - 1).max(first);
    Some((first, last))
}

/// The tag the VS Code extension puts before a prompt so the model knows what the user is looking
/// at; it saves the discovery turns (Glob, Grep) a bare prompt would start with.
pub fn ide_context(path: &Path, selection: Option<(usize, usize)>) -> String {
    let path = path.display();
    let selected = match selection {
        Some((a, b)) if a == b => format!(" Line {a} is selected."),
        Some((a, b)) => format!(" Lines {a} to {b} are selected."),
        None => String::new(),
    };
    format!(
        "<ide_opened_file>The user opened the file {path} in the IDE.{selected} \
This may or may not be related to the current task.</ide_opened_file>"
    )
}

/// One editor window's content: header, optional "changed on disk" bar, and the text.
pub fn show(
    ui: &mut egui::Ui,
    vm: &mut EditorVm,
    maximized: bool,
    focused: bool,
    show_whitespace: bool,
) -> EditorOut {
    let mut out = EditorOut {
        view: ViewOut::default(),
        error: None,
    };
    let title = vm.title();
    let hover = vm
        .path
        .as_ref()
        .map_or_else(|| title.clone(), |p| p.display().to_string());
    let label = if vm.dirty {
        format!("{title} \u{25CF}")
    } else {
        title.clone()
    };
    let is_image = vm.is_image();
    let markdown_file = !is_image && vm.is_markdown();
    chrome::header(
        ui,
        chrome::HeaderState {
            maximized,
            collapsed: vm.collapsed,
            focused,
        },
        "Close file",
        &mut out.view,
        &chrome::file_badge,
        |ui| {
            ui.add(egui::Label::new(RichText::new(label).strong()).truncate())
                .on_hover_text(hover);
            if let Some(img) = vm.image.as_mut() {
                ui.label(
                    RichText::new(format!("{} \u{00D7} {}", img.size[0], img.size[1]))
                        .small()
                        .color(theme::text_muted()),
                );
                if ui.selectable_label(img.actual_size, "1:1").clicked() {
                    img.actual_size = !img.actual_size;
                }
            } else if markdown_file && ui.selectable_label(vm.preview, "Preview").clicked() {
                vm.preview = !vm.preview;
            }
        },
    );
    if vm.collapsed {
        return out;
    }
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(6)))
        .show(ui, |ui| {
            if is_image {
                show_image(ui, vm, &title);
            } else {
                if vm.disk_changed {
                    out.error = disk_bar(ui, vm);
                }
                if vm.preview && markdown_file {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let id = crate::find::preview_id(vm.id);
                            let mark = crate::find::mark(
                                ui.ctx(),
                                crate::workspace::WindowKind::Editor(vm.id),
                            );
                            let jump = mark.as_ref().is_some_and(|m| crate::find::take_jump(ui, m));
                            let from = ui
                                .ctx()
                                .graphics(|g| g.get(ui.layer_id()).map_or(0, |l| l.next_idx().0));
                            preview(ui, &mut vm.md_cache, id, &vm.text, mark.as_ref(), jump);
                            if mark.is_some() {
                                crate::find::blacken_current(ui, from);
                            }
                        });
                } else {
                    edit(ui, vm, show_whitespace);
                }
            }
        });
    out
}

/// Draws an image centered inside the window, scaled to fit or at 1:1 pixels inside a scroll area.
fn show_image(ui: &mut egui::Ui, vm: &mut EditorVm, title: &str) {
    let Some(img) = vm.image.as_mut() else {
        return;
    };
    if let Some(pixels) = img.pending.take() {
        let name = format!("file-image-{}", vm.id.0);
        img.texture = Some(
            ui.ctx()
                .load_texture(name, pixels, egui::TextureOptions::LINEAR),
        );
    }
    let Some(tex) = &img.texture else {
        return;
    };
    let natural = egui::vec2(img.size[0].max(1) as f32, img.size[1].max(1) as f32);
    let draw = |ui: &mut egui::Ui, size: egui::Vec2| {
        let resp = ui.add(
            egui::Image::from_texture((tex.id(), size))
                .fit_to_exact_size(size)
                .alt_text(title),
        );
        ui.painter().rect_stroke(
            resp.rect,
            0.0,
            egui::Stroke::new(1.0, theme::border()),
            egui::StrokeKind::Outside,
        );
    };
    if img.actual_size {
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let avail = ui.available_size();
                let pad_x = ((avail.x - natural.x) * 0.5).max(0.0);
                let pad_y = ((avail.y - natural.y) * 0.5).max(0.0);
                ui.add_space(pad_y);
                ui.horizontal(|ui| {
                    ui.add_space(pad_x);
                    draw(ui, natural);
                });
            });
    } else {
        let avail = (ui.available_size() - egui::vec2(4.0, 4.0)).max(egui::vec2(1.0, 1.0));
        let scale = (avail.x / natural.x).min(avail.y / natural.y).min(1.0);
        let size = (natural * scale).max(egui::vec2(1.0, 1.0));
        let pad = ((ui.available_size() - size) * 0.5).max(egui::Vec2::ZERO);
        ui.add_space(pad.y);
        ui.horizontal(|ui| {
            ui.add_space(pad.x);
            draw(ui, size);
        });
    }
}

fn disk_bar(ui: &mut egui::Ui, vm: &mut EditorVm) -> Option<String> {
    let mut err = None;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Changed on disk")
                .small()
                .color(theme::text_muted()),
        );
        if ui.small_button("Reload").clicked() {
            err = vm.reload().err();
        }
        if ui.small_button("Keep mine").clicked() {
            vm.disk_changed = false;
            vm.disk_mtime = vm.path.as_deref().and_then(mtime);
        }
    });
    err
}

/// The line-number gutter and the text edit, scrolling together.
fn edit(ui: &mut egui::Ui, vm: &mut EditorVm, show_whitespace: bool) {
    let font = TextStyle::Monospace.resolve(ui.style());
    let row_h = ui.fonts_mut(|f| f.row_height(&font));
    let avail = ui.available_size();
    let lines = vm.text.split('\n').count();
    let digits = lines.to_string().len().max(2);
    let numbers = (1..=lines)
        .map(|n| format!("{n:>digits$}"))
        .collect::<Vec<_>>()
        .join("\n");
    let theme = CodeTheme::from_style(ui.style());
    let language = vm.syntax_token();
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let mut job = highlight_with(
            ui.ctx(),
            ui.style(),
            &theme,
            buf.as_str(),
            &language,
            &SYNTAX,
        );
        job.wrap.max_width = f32::INFINITY;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let gutter = ui.add(
                    egui::Label::new(
                        RichText::new(numbers)
                            .font(font.clone())
                            .color(theme::text_muted()),
                    )
                    .selectable(false),
                );
                let width =
                    (avail.x - gutter.rect.width() - ui.spacing().item_spacing.x - 12.0).max(100.0);
                let rows = (avail.y / row_h).ceil() as usize;
                let output = egui::TextEdit::multiline(&mut vm.text)
                    .code_editor()
                    .frame(egui::Frame::NONE)
                    .desired_width(width)
                    .desired_rows(rows)
                    .layouter(&mut layouter)
                    .show(ui);
                if output.response.changed() {
                    vm.dirty = true;
                    vm.revision += 1;
                }
                if let Some(mark) =
                    crate::find::mark(ui.ctx(), crate::workspace::WindowKind::Editor(vm.id))
                {
                    crate::find::paint_text_matches(ui, &output.galley, output.galley_pos, &mark);
                }
                if show_whitespace {
                    paint_whitespace(ui, &output.galley, output.galley_pos);
                }
                // Follows the cursor only while the text has focus: clicking into a session's
                // composer must not forget what was selected here.
                if output.response.has_focus() {
                    crate::widgets::selection_menu::mark_text_box(ui.ctx());
                    vm.selection = output.cursor_range.and_then(|r| {
                        let range = r.as_sorted_char_range();
                        selected_lines(&vm.text, range.start.0, range.end.0)
                    });
                }
            });
        });
}

/// Indexes of the spaces and tabs of a row: all of them, like VS Code's `renderWhitespace: all`.
fn whitespace_in(row: &egui::epaint::text::Row) -> impl Iterator<Item = usize> + '_ {
    row.glyphs
        .iter()
        .enumerate()
        .filter(|(_, g)| g.chr == ' ' || g.chr == '\t')
        .map(|(i, _)| i)
}

/// Draws a faint dot over each space and a faint dash over each tab, only for the rows on
/// screen.
fn paint_whitespace(ui: &egui::Ui, galley: &egui::Galley, pos: egui::Pos2) {
    let clip = ui.clip_rect();
    let color = theme::whitespace();
    let painter = ui.painter();
    for placed in &galley.rows {
        let rect = placed.rect();
        if pos.y + rect.bottom() < clip.top() {
            continue;
        }
        if pos.y + rect.top() > clip.bottom() {
            break;
        }
        let y = pos.y + rect.center().y;
        for i in whitespace_in(&placed.row) {
            let glyph = &placed.glyphs[i];
            let x = pos.x + placed.pos.x + glyph.pos.x;
            if glyph.chr == ' ' {
                painter.circle_filled(egui::pos2(x + glyph.advance_width / 2.0, y), 1.0, color);
            } else {
                let (from, to) = (x + 2.0, x + glyph.advance_width - 2.0);
                painter.line_segment(
                    [egui::pos2(from, y), egui::pos2(to, y)],
                    egui::Stroke::new(1.0, color),
                );
            }
        }
    }
}

pub enum CloseChoice {
    Save,
    DontSave,
    Cancel,
}

/// "Save changes to <name>?"; Esc cancels.
pub fn close_modal(ctx: &egui::Context, name: &str) -> Option<CloseChoice> {
    let mut choice = None;
    let resp = crate::widgets::modal(ctx, egui::Id::unique("editor_close")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.label(
            RichText::new(format!("Save changes to {name}?"))
                .strong()
                .size(16.0),
        );
        ui.label(
            RichText::new("Your changes are lost if you don't save them.")
                .color(theme::text_muted()),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new("Save").fill(theme::accent()))
                .clicked()
            {
                choice = Some(CloseChoice::Save);
            }
            if ui.button("Don't Save").clicked() {
                choice = Some(CloseChoice::DontSave);
            }
            if ui.button("Cancel").clicked() {
                choice = Some(CloseChoice::Cancel);
            }
        });
    });
    if choice.is_none() && resp.should_close() {
        choice = Some(CloseChoice::Cancel);
    }
    choice
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn selected_lines_are_one_based_and_skip_a_trailing_line_start() {
        let text = "one\ntwo\nthree\n";
        assert_eq!(selected_lines(text, 2, 2), None);
        assert_eq!(selected_lines(text, 0, 3), Some((1, 1)));
        assert_eq!(selected_lines(text, 2, 6), Some((1, 2)));
        assert_eq!(
            selected_lines(text, 0, 8),
            Some((1, 2)),
            "ends at the start of line 3"
        );
        assert_eq!(selected_lines(text, 4, 14), Some((2, 3)));
    }

    #[test]
    fn ide_context_names_the_file_and_the_selection() {
        let p = Path::new("/w/src/a.rs");
        let plain = ide_context(p, None);
        assert!(plain.starts_with("<ide_opened_file>"), "{plain}");
        assert!(
            plain.contains("/w/src/a.rs") && !plain.contains("selected"),
            "{plain}"
        );
        assert!(ide_context(p, Some((3, 3))).contains("Line 3 is selected"));
        assert!(ide_context(p, Some((3, 9))).contains("Lines 3 to 9 are selected"));
    }

    #[test]
    fn the_last_focused_editor_with_a_file_supplies_the_context() {
        let mut eds = Editors::default();
        assert_eq!(eds.ide_context(), None);
        let a = eds.create(Some(PathBuf::from("/w/a.rs")), String::new());
        let untitled = eds.create(None, String::new());
        eds.last_focused = Some(a);
        eds.map.get_mut(&a).unwrap().selection = Some((2, 4));
        let tag = eds.ide_context().unwrap();
        assert!(
            tag.contains("/w/a.rs") && tag.contains("Lines 2 to 4"),
            "{tag}"
        );
        eds.last_focused = Some(untitled);
        assert_eq!(eds.ide_context(), None, "no path to name");
        eds.map.remove(&a);
        eds.last_focused = Some(a);
        assert_eq!(eds.ide_context(), None, "closed");
    }

    fn frames(vm: &mut EditorVm, n: usize) {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0));
        for _ in 0..n {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                show(ui, vm, false, false, true);
            })
            .drop_without_applying_deltas();
        }
    }

    /// Draws `vm` in a window `width` wide and returns the right edge of every text drawn and
    /// the most rows any one text takes.
    fn drawn_text(vm: &mut EditorVm, width: f32) -> (f32, usize) {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.0));
        let mut found = (0.0_f32, 0);
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                show(ui, vm, false, false, true);
                found = ui.ctx().graphics(|g| {
                    let mut found = (0.0_f32, 0);
                    for c in g
                        .get(ui.layer_id())
                        .into_iter()
                        .flat_map(|l| l.all_entries())
                    {
                        if let egui::Shape::Text(t) = &c.shape {
                            found.0 = found.0.max(t.pos.x + t.galley.size().x);
                            found.1 = found.1.max(t.galley.rows.len());
                        }
                    }
                    found
                });
            })
            .drop_without_applying_deltas();
        }
        found
    }

    #[test]
    fn a_preview_table_wraps_inside_the_window() {
        let mut ed = Editors::default();
        let id = ed.create(
            Some(PathBuf::from("/tmp/xade-nope/notes.md")),
            crate::widgets::markdown::wide_table(),
        );
        let vm = ed.map.get_mut(&id).unwrap();
        vm.preview = true;
        let (right, rows) = drawn_text(vm, 500.0);
        assert!(right <= 500.0, "text runs {right}px, past the window");
        assert!(rows > 3, "no sentence wrapped: {rows} rows at most");
    }

    #[test]
    fn every_space_and_tab_is_marked() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let text = "  a b  \n\tx\t\n\n  ";
        let marks = std::cell::RefCell::new(Vec::new());
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let font = TextStyle::Monospace.resolve(ui.style());
            let galley =
                ui.fonts_mut(|f| f.layout_no_wrap(text.to_string(), font, egui::Color32::WHITE));
            let mut found = Vec::new();
            for (r, placed) in galley.rows.iter().enumerate() {
                found.extend(whitespace_in(&placed.row).map(|g| (r, g)));
            }
            *marks.borrow_mut() = found;
        })
        .drop_without_applying_deltas();
        // Leading, inner and trailing ones, on text and whitespace-only lines alike.
        assert_eq!(
            marks.into_inner(),
            [
                (0, 0),
                (0, 1),
                (0, 3),
                (0, 5),
                (0, 6),
                (1, 0),
                (1, 2),
                (3, 0),
                (3, 1)
            ]
        );
    }

    fn long_text() -> String {
        (0..2000)
            .map(|i| format!("line {i}: fn main() {{ let x = {i}; }} // some text\n"))
            .collect()
    }

    #[test]
    fn renders_a_2000_line_file_headless() {
        let mut ed = Editors::default();
        for (name, preview) in [
            ("a.rs", false),
            ("b.md", false),
            ("c.md", true),
            ("d.xyz", false),
            ("e.tsx", false),
            ("Dockerfile", false),
        ] {
            let id = ed.create(
                Some(PathBuf::from(format!("/tmp/xade-nope/{name}"))),
                long_text(),
            );
            let vm = ed.map.get_mut(&id).unwrap();
            vm.preview = preview;
            frames(vm, 3);
            vm.collapsed = true;
            frames(vm, 2);
        }
        let id = ed.create(None, String::new());
        let vm = ed.map.get_mut(&id).unwrap();
        assert_eq!(vm.title(), "Untitled-1.md");
        vm.disk_changed = true;
        frames(vm, 2);
    }

    #[test]
    fn extended_syntax_set_covers_common_languages() {
        for token in [
            "ts",
            "tsx",
            "toml",
            "swift",
            "kt",
            "Dockerfile",
            "tf",
            "vue",
            "svelte",
            "zig",
            "proto",
        ] {
            assert!(
                SYNTAX.ps.find_syntax_by_extension(token).is_some(),
                "{token}"
            );
        }
    }

    #[test]
    fn close_modal_renders_headless() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        for _ in 0..2 {
            ctx.run_pass(egui::RawInput::default(), |ctx| {
                assert!(close_modal(ctx, "a.md").is_none());
            })
            .drop_without_applying_deltas();
        }
    }

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("xade-ed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.md");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert!(read_file(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_editors_reload_and_dirty_ones_flag_changes() {
        let dir = std::env::temp_dir().join(format!("xade-ed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.md");
        std::fs::write(&path, "one").unwrap();
        let mut ed = Editors::default();
        let id = ed.create(Some(path.clone()), "one".into());
        let vm = ed.map.get_mut(&id).unwrap();
        vm.disk_mtime = Some(SystemTime::UNIX_EPOCH);
        std::fs::write(&path, "two").unwrap();
        vm.check_disk().unwrap();
        assert_eq!(vm.text, "two");
        assert!(!vm.disk_changed);
        vm.dirty = true;
        vm.disk_mtime = Some(SystemTime::UNIX_EPOCH);
        vm.check_disk().unwrap();
        assert!(vm.disk_changed && vm.text == "two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_paths_are_recognized_case_insensitively() {
        for name in [
            "a.png", "b.PNG", "c.jpg", "d.JPEG", "e.gif", "f.webp", "g.bmp", "h.ico",
        ] {
            assert!(is_image_path(Path::new(name)), "{name}");
        }
        for name in ["a.rs", "README.md", "Makefile", "diagram.svg", "data.bin"] {
            assert!(!is_image_path(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn read_entry_decodes_raster_formats_and_rejects_bad_or_oversized_images() {
        let dir = std::env::temp_dir().join(format!("xade-img-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let rgba = image::RgbaImage::from_pixel(4, 3, image::Rgba([20, 140, 220, 255]));
        let rgb = image::RgbImage::from_pixel(4, 3, image::Rgb([20, 140, 220]));
        for (name, format, use_rgb) in [
            ("a.png", image::ImageFormat::Png, false),
            ("b.jpg", image::ImageFormat::Jpeg, true),
            ("c.gif", image::ImageFormat::Gif, false),
            ("d.webp", image::ImageFormat::WebP, false),
            ("e.bmp", image::ImageFormat::Bmp, true),
            ("f.ico", image::ImageFormat::Ico, false),
        ] {
            let p = dir.join(name);
            if use_rgb {
                rgb.save_with_format(&p, format).unwrap();
            } else {
                rgba.save_with_format(&p, format).unwrap();
            }
            match read_entry(&p).unwrap() {
                FileContent::Image(img) => assert_eq!(img.size, [4, 3], "{name}"),
                FileContent::Text(_) => panic!("expected image for {name}"),
            }
        }

        let txt = dir.join("note.txt");
        std::fs::write(&txt, "hello").unwrap();
        assert!(matches!(read_entry(&txt), Ok(FileContent::Text(s)) if s == "hello"));

        let corrupt = dir.join("broken.png");
        std::fs::write(&corrupt, b"not a png").unwrap();
        assert!(read_entry(&corrupt).is_err());

        let huge = dir.join("huge.png");
        std::fs::File::create(&huge)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .unwrap();
        assert!(matches!(read_entry(&huge), Err(e) if e.contains("larger than")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_window_uploads_texture_and_reloads_on_mtime_change() {
        let dir = std::env::temp_dir().join(format!("xade-imgvm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pic.png");
        std::fs::write(&path, b"placeholder").unwrap();

        let mut ed = Editors::default();
        let pixels = egui::ColorImage::filled([64, 32], egui::Color32::from_rgb(10, 20, 30));
        let id = ed.create_image(path.clone(), pixels);
        let vm = ed.map.get_mut(&id).unwrap();
        assert!(vm.is_image());
        assert!(vm.image.as_ref().unwrap().pending.is_some());

        frames(vm, 2);
        let img = vm.image.as_ref().unwrap();
        assert!(img.pending.is_none(), "uploaded and cleared");
        assert!(img.texture.is_some());

        vm.image.as_mut().unwrap().actual_size = true;
        frames(vm, 2);

        // Unchanged mtime needs no reload; changed mtime queues one once.
        assert!(vm.image_needs_reload().is_none());
        vm.disk_mtime = Some(SystemTime::UNIX_EPOCH);
        assert_eq!(vm.image_needs_reload(), Some(path));
        assert!(vm.image_needs_reload().is_none(), "one reload in flight");

        vm.set_image(egui::ColorImage::filled([128, 64], egui::Color32::WHITE));
        let img = vm.image.as_ref().unwrap();
        assert_eq!(img.size, [128, 64]);
        assert!(img.actual_size, "zoom choice kept across reloads");
        assert!(!img.reloading);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Pasting copied files or an image into the input.

use std::path::PathBuf;

/// Files on the system clipboard (e.g. copied in Finder), if any.
pub(super) fn clipboard_files() -> Option<Vec<PathBuf>> {
    let files = arboard::Clipboard::new().ok()?.get().file_list().ok()?;
    (!files.is_empty()).then_some(files)
}

/// When the events hold a paste and `files` finds copied files, drops the paste (it would
/// only insert the file names as text) and returns them.
pub(super) fn take_pasted_files(
    events: &mut Vec<egui::Event>,
    files: impl FnOnce() -> Option<Vec<PathBuf>>,
) -> Option<Vec<PathBuf>> {
    let is_paste =
        |e: &egui::Event| matches!(e, egui::Event::Paste(_) | egui::Event::PasteImage(_));
    if !events.iter().any(is_paste) {
        return None;
    }
    let files = files()?;
    events.retain(|e| !is_paste(e));
    Some(files)
}

/// When the events hold a pasted image, drops them and returns the image.
pub(super) fn take_pasted_image(
    events: &mut Vec<egui::Event>,
) -> Option<std::sync::Arc<egui::ColorImage>> {
    let mut image = None;
    events.retain(|e| match e {
        egui::Event::PasteImage(img) => {
            image = Some(img.clone());
            false
        }
        _ => true,
    });
    image
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn pasting_copied_files_replaces_the_text_paste() {
        let files = || Some(vec![PathBuf::from("/p/a.rs")]);
        let typed = egui::Event::Text("x".into());
        let paste = egui::Event::Paste("a.rs".into());

        let mut events = vec![typed.clone(), paste.clone()];
        assert_eq!(
            take_pasted_files(&mut events, files),
            Some(vec![PathBuf::from("/p/a.rs")])
        );
        assert_eq!(events.len(), 1, "the text paste is dropped");
        assert!(matches!(events[0], egui::Event::Text(_)));

        // Plain text on the clipboard keeps its normal paste.
        let mut events = vec![paste.clone()];
        assert_eq!(take_pasted_files(&mut events, || None), None);
        assert_eq!(events, [paste]);

        // Without a paste the clipboard is not even read.
        let mut events = vec![typed];
        assert_eq!(take_pasted_files(&mut events, || panic!("read")), None);
    }

    #[test]
    fn a_pasted_image_event_is_taken_and_other_events_stay() {
        let img = std::sync::Arc::new(egui::ColorImage::example());
        let typed = egui::Event::Text("x".into());
        let mut events = vec![typed, egui::Event::PasteImage(img.clone())];
        let got = take_pasted_image(&mut events).unwrap();
        assert!(std::sync::Arc::ptr_eq(&got, &img));
        assert_eq!(events.len(), 1);
        assert_eq!(take_pasted_image(&mut events), None);
    }
}

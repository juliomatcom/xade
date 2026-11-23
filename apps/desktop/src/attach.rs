//! What may be attached to a prompt (text and code, images, PDFs, folders; at most 10 MB) and
//! saving pasted clipboard images. The CLI expands the resulting `@path` itself.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Largest file that can be attached, and largest pasted image once encoded.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
const IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
/// Bytes read to tell text from binary.
const SNIFF_BYTES: usize = 8192;
/// Pasted images older than this are deleted when a new one is saved.
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, PartialEq, Eq)]
pub enum Reject {
    TooBig,
    Binary,
    Unreadable,
}

/// Whether `path` may be attached: a folder, an image or PDF by extension, or a file that
/// reads as UTF-8 text. Reads at most `SNIFF_BYTES`.
pub fn check(path: &Path) -> Result<(), Reject> {
    let meta = std::fs::metadata(path).map_err(|_| Reject::Unreadable)?;
    if meta.is_dir() {
        return Ok(());
    }
    if meta.len() > MAX_BYTES {
        return Err(Reject::TooBig);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if ext
        .as_deref()
        .is_some_and(|e| e == "pdf" || IMAGE_EXTS.contains(&e))
    {
        return Ok(());
    }
    let mut buf = vec![0u8; SNIFF_BYTES];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut buf))
        .map_err(|_| Reject::Unreadable)?;
    if is_text(&buf[..n]) {
        Ok(())
    } else {
        Err(Reject::Binary)
    }
}

/// No NUL byte and valid UTF-8, allowing a character cut off at the end of the sample.
fn is_text(sample: &[u8]) -> bool {
    !sample.contains(&0)
        && match std::str::from_utf8(sample) {
            Ok(_) => true,
            Err(e) => e.error_len().is_none(),
        }
}

/// The status-bar text for the files that were not attached.
pub fn rejected_message(rejected: &[(PathBuf, Reject)]) -> String {
    let limit = MAX_BYTES / (1024 * 1024);
    let parts: Vec<String> = rejected
        .iter()
        .map(|(path, why)| {
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into(),
            );
            match why {
                Reject::TooBig => format!("{name} (over {limit} MB)"),
                Reject::Binary => format!("{name} (binary)"),
                Reject::Unreadable => format!("{name} (unreadable)"),
            }
        })
        .collect();
    format!(
        "Not attached: {}. Text, code, images and PDFs up to {limit} MB only.",
        parts.join(", ")
    )
}

/// Saves a pasted clipboard image as a PNG under the temp dir and returns its path.
/// Blocking: encoding a large screenshot takes a while.
pub fn save_pasted_image(image: &egui::ColorImage) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("xade-attachments");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not save the image: {e}"))?;
    prune(&dir);
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|c| c.to_srgba_unmultiplied())
        .collect();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let path = dir.join(format!("pasted-{}.png", &id[..8]));
    image::save_buffer_with_format(
        &path,
        &rgba,
        image.size[0] as u32,
        image.size[1] as u32,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .map_err(|e| format!("Could not save the image: {e}"))?;
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    if size > MAX_BYTES {
        let _ = std::fs::remove_file(&path);
        return Err(format!(
            "The pasted image is over {} MB.",
            MAX_BYTES / (1024 * 1024)
        ));
    }
    Ok(path)
}

/// Deletes old pasted images.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > KEEP);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xade-attach-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn standard_formats_pass_and_binaries_do_not() {
        let dir = temp_dir();
        let write = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        assert_eq!(check(&write("a.rs", b"fn main() {}\n")), Ok(()));
        assert_eq!(check(&write("Makefile", b"all:\n\ttrue\n")), Ok(()));
        assert_eq!(check(&write("empty.txt", b"")), Ok(()));
        assert_eq!(check(&write("ünï.md", "héllo wörld".as_bytes())), Ok(()));
        assert_eq!(check(&write("x.PNG", &[0x89, b'P', 0, 1])), Ok(()));
        assert_eq!(check(&write("doc.pdf", b"%PDF-1.4\0")), Ok(()));
        assert_eq!(check(&dir), Ok(()), "folders are fine");
        assert_eq!(
            check(&write("a.zip", b"PK\x03\x04\0\0")),
            Err(Reject::Binary)
        );
        assert_eq!(check(&write("blob", &[1, 2, 0, 3])), Err(Reject::Binary));
        assert_eq!(
            check(&write("latin1.txt", &[0x68, 0xe9, 0x6c])),
            Err(Reject::Binary)
        );
        assert_eq!(check(&dir.join("missing.txt")), Err(Reject::Unreadable));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_character_cut_at_the_sample_edge_is_still_text() {
        let mut bytes = vec![b'a'; SNIFF_BYTES - 1];
        bytes.extend("é".as_bytes()); // second byte falls outside the sample
        let dir = temp_dir();
        let p = dir.join("long.txt");
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(check(&p), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_over_the_limit_are_refused() {
        let dir = temp_dir();
        let p = dir.join("big.png"); // an image extension skips the text sniff
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        assert_eq!(check(&p), Err(Reject::TooBig));
        f.set_len(MAX_BYTES).unwrap();
        assert_eq!(check(&p), Ok(()), "exactly at the limit passes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_message_names_each_file_and_the_limit() {
        let msg = rejected_message(&[
            ("/p/a.zip".into(), Reject::Binary),
            ("/p/b.mov".into(), Reject::TooBig),
        ]);
        assert_eq!(
            msg,
            "Not attached: a.zip (binary), b.mov (over 10 MB). Text, code, images and PDFs up to 10 MB only."
        );
    }

    #[test]
    fn a_pasted_image_is_saved_as_a_readable_png() {
        let pixels = vec![egui::Color32::from_rgb(255, 0, 0); 4 * 3];
        let image = egui::ColorImage {
            size: [4, 3],
            source_size: egui::vec2(4.0, 3.0),
            pixels,
        };
        let path = save_pasted_image(&image).unwrap();
        assert!(path.extension().is_some_and(|e| e == "png"));
        assert_eq!(check(&path), Ok(()));
        let back = eframe::icon_data::from_png_bytes(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((back.width, back.height), (4, 3));
        assert_eq!(&back.rgba[..4], &[255, 0, 0, 255]);
        let _ = std::fs::remove_file(&path);
    }
}

//! Turns file paths in message markdown into links that open the file in Xade's editor.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use egui_commonmark::CommonMarkCache;
use pulldown_cmark::utils::TextMergeWithOffset;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use uuid::Uuid;

use super::markdown::markdown;

/// Link destinations with this prefix are link hooks, never URLs.
const SCHEME: &str = "xade-file:";

struct Linked {
    text: String,
    dests: Vec<String>,
}

/// Linked markdown per message, so paths are checked on disk once, not every frame.
#[derive(Default)]
pub struct PathLinks {
    linked: HashMap<(Uuid, usize), Linked>,
}

impl PathLinks {
    /// Renders `text` (message `id`) with existing file paths as links; returns a clicked path.
    pub fn markdown(
        &mut self,
        ui: &mut egui::Ui,
        cache: &mut CommonMarkCache,
        cwd: &Path,
        id: Uuid,
        text: &str,
    ) -> Option<PathBuf> {
        let linked = self.linked.entry((id, text.len())).or_insert_with(|| {
            let (text, dests) = linkify(text, cwd, Path::is_file);
            Linked { text, dests }
        });
        for d in &linked.dests {
            if cache.get_link_hook(d).is_none() {
                cache.add_link_hook(d.clone());
            }
        }
        markdown(ui, cache, id, &linked.text);
        let mut clicked = None;
        for d in &linked.dests {
            if cache.get_link_hook(d) == Some(true) {
                cache.link_hooks_mut().insert(d.clone(), false);
                clicked = d.strip_prefix(SCHEME).map(PathBuf::from);
            }
        }
        clicked
    }
}

/// Rewrites plain-text and inline-code paths that `exists` accepts (resolved against `cwd`)
/// into links; returns the new markdown and the link destinations it added.
fn linkify(text: &str, cwd: &Path, exists: impl Fn(&Path) -> bool) -> (String, Vec<String>) {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut seen: HashMap<String, Option<PathBuf>> = HashMap::new();
    let mut resolve = |token: &str| -> Option<PathBuf> {
        seen.entry(token.to_string())
            .or_insert_with(|| {
                let path = candidate(token, cwd)?;
                exists(&path).then_some(path)
            })
            .clone()
    };
    // (source range, link text, destination)
    let mut edits: Vec<(Range<usize>, String, String)> = Vec::new();
    let mut skip = 0usize;
    for (event, range) in TextMergeWithOffset::new(Parser::new_ext(text, opts).into_offset_iter()) {
        match event {
            Event::Start(Tag::CodeBlock(_) | Tag::Link { .. } | Tag::Image { .. })
            | Event::Start(Tag::HtmlBlock) => skip += 1,
            Event::End(TagEnd::CodeBlock | TagEnd::Link | TagEnd::Image | TagEnd::HtmlBlock) => {
                skip = skip.saturating_sub(1);
            }
            Event::Code(code) if skip == 0 => {
                let token = strip_line_suffix(code.trim());
                if token.contains(char::is_whitespace) {
                    continue;
                }
                if let Some(path) = resolve(token) {
                    let label = text[range.clone()].to_string();
                    edits.push((range, label, dest(&path)));
                }
            }
            // Only text that is its own source, so byte offsets map 1:1.
            Event::Text(t) if skip == 0 && text.get(range.clone()) == Some(&*t) => {
                for (start, word) in words(&t) {
                    let (lead, core) = trim_punct(word);
                    let token = strip_line_suffix(core);
                    if let Some(path) = resolve(token) {
                        let at = range.start + start + lead;
                        edits.push((at..at + token.len(), escape(token), dest(&path)));
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut dests = Vec::new();
    let mut at = 0;
    for (range, label, dest) in edits {
        if range.start < at {
            continue;
        }
        out.push_str(&text[at..range.start]);
        out.push_str(&format!("[{label}](<{dest}>)"));
        dests.push(dest);
        at = range.end;
    }
    out.push_str(&text[at..]);
    (out, dests)
}

/// The path a token names, when it looks like one; never touches the disk.
fn candidate(token: &str, cwd: &Path) -> Option<PathBuf> {
    let looks_like_path = (token.contains('/') || token.contains('.'))
        && !token.contains("://")
        && !token
            .chars()
            .any(|c| matches!(c, '<' | '>' | '[' | ']' | '\\' | '&' | '`'))
        && token.chars().any(char::is_alphanumeric)
        && token.len() <= 1024;
    if !looks_like_path {
        return None;
    }
    if let Some(rest) = token.strip_prefix("~/") {
        return dirs::home_dir().map(|h| h.join(rest));
    }
    Some(cwd.join(token))
}

fn dest(path: &Path) -> String {
    format!("{SCHEME}{}", path.display())
}

/// Backslash-escapes markdown punctuation so link text renders as typed.
fn escape(token: &str) -> String {
    let mut s = String::with_capacity(token.len());
    for c in token.chars() {
        if matches!(c, '*' | '_' | '~' | '|') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// Whitespace-separated words with their byte offsets.
fn words(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.split(char::is_whitespace)
        .filter(|w| !w.is_empty())
        .map(move |w| (w.as_ptr() as usize - text.as_ptr() as usize, w))
}

/// Drops quotes, brackets and sentence punctuation around a word; returns the bytes cut in front.
fn trim_punct(word: &str) -> (usize, &str) {
    let lead = |c: char| matches!(c, '"' | '\'' | '(' | '[' | '{' | '<');
    let trail = |c: char| {
        matches!(
            c,
            '"' | '\'' | ')' | ']' | '}' | '>' | ',' | ';' | ':' | '.' | '!' | '?'
        )
    };
    let core = word.trim_start_matches(lead);
    let cut = word.len() - core.len();
    (cut, core.trim_end_matches(trail))
}

/// `src/a.rs:12` and `src/a.rs:12:4` name `src/a.rs`.
fn strip_line_suffix(token: &str) -> &str {
    let mut t = token;
    for _ in 0..2 {
        match t.rsplit_once(':') {
            Some((head, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => t = head,
            _ => break,
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> (String, Vec<String>) {
        let files = [
            "/w/src/main.rs",
            "/w/Cargo.toml",
            "/abs/x.md",
            "/w/my_mod.rs",
        ];
        linkify(text, Path::new("/w"), |p| {
            files.iter().any(|f| Path::new(f) == p)
        })
    }

    #[test]
    fn links_existing_paths_in_text_and_inline_code() {
        let (out, dests) = run("See src/main.rs:12, `Cargo.toml` and (/abs/x.md).");
        assert_eq!(
            out,
            "See [src/main.rs](<xade-file:/w/src/main.rs>):12, [`Cargo.toml`](<xade-file:/w/Cargo.toml>) and ([/abs/x.md](<xade-file:/abs/x.md>))."
        );
        assert_eq!(dests.len(), 3);
        assert_eq!(dests[0], "xade-file:/w/src/main.rs");
    }

    #[test]
    fn leaves_missing_paths_code_blocks_and_links_alone() {
        for text in [
            "and/or e.g. src/nope.rs",
            "```\nsrc/main.rs\n```",
            "[src/main.rs](https://x.y)",
            "https://example.com/src/main.rs",
        ] {
            let (out, dests) = run(text);
            assert_eq!(out, text);
            assert!(dests.is_empty(), "{text}");
        }
    }

    #[test]
    fn escapes_markdown_in_link_text() {
        let (out, _) = run("edit my_mod.rs now");
        assert_eq!(out, "edit [my\\_mod.rs](<xade-file:/w/my_mod.rs>) now");
    }

    #[test]
    fn linked_markdown_renders_and_registers_hooks() {
        let ctx = egui::Context::default();
        let mut cache = CommonMarkCache::default();
        let mut links = PathLinks::default();
        let dir = std::env::temp_dir();
        let id = Uuid::new_v4();
        for _ in 0..2 {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                assert!(links
                    .markdown(ui, &mut cache, &dir, id, "plain `text`")
                    .is_none());
            })
            .drop_without_applying_deltas();
        }
    }
}

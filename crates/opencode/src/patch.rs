//! The `patch` tool's `patchText` (`*** Begin Patch` … `*** End Patch`) as a Claude-shaped
//! `Write` or `Edit` input, so a patch is reviewed in the diff modal like any other change.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Add,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub kind: Kind,
    pub path: String,
    pub move_to: Option<String>,
    /// Lines prefixed `' '`, `'-'` or `'+'`; an `@@` line starts a new hunk.
    pub hunks: Vec<Vec<String>>,
}

/// `None` for anything not understood: the caller then shows the raw text instead of a partial
/// patch, so nothing that will be applied is hidden from the reviewer.
pub fn parse(text: &str) -> Option<Vec<FilePatch>> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut lines = text.lines().map(|l| l.trim_end_matches('\r'));
    lines.by_ref().find(|l| l.trim() == "*** Begin Patch")?;
    let mut ended = false;
    for line in lines {
        if line.trim() == "*** End Patch" {
            ended = true;
            break;
        }
        let header = [
            ("*** Add File: ", Kind::Add),
            ("*** Update File: ", Kind::Update),
            ("*** Delete File: ", Kind::Delete),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| Some((line.strip_prefix(prefix)?.trim(), kind)));
        if let Some((path, kind)) = header {
            if path.is_empty() {
                return None;
            }
            files.push(FilePatch {
                kind,
                path: path.to_string(),
                move_to: None,
                hunks: vec![Vec::new()],
            });
            continue;
        }
        let file = files.last_mut()?;
        if let Some(to) = line.strip_prefix("*** Move to: ") {
            if file.kind != Kind::Update || to.trim().is_empty() {
                return None;
            }
            file.move_to = Some(to.trim().to_string());
            continue;
        }
        if line.trim() == "*** End of File" {
            continue;
        }
        if line.starts_with("*** ") {
            return None;
        }
        match file.kind {
            Kind::Add if line.starts_with('+') => push(file, line),
            Kind::Update if line.starts_with("@@") => {
                if file.hunks.last().is_some_and(|h| !h.is_empty()) {
                    file.hunks.push(Vec::new());
                }
            }
            Kind::Update if line.starts_with([' ', '-', '+']) => push(file, line),
            // An empty context line often loses its leading space.
            Kind::Update if line.is_empty() => push(file, " "),
            Kind::Delete if line.is_empty() => {}
            _ => return None,
        }
    }
    (ended && !files.is_empty()).then_some(files)
}

fn push(file: &mut FilePatch, line: &str) {
    if let Some(hunk) = file.hunks.last_mut() {
        hunk.push(line.to_string());
    }
}

/// The content a single new file gets, when that is all the patch does.
pub fn single_add(files: &[FilePatch]) -> Option<(&str, String)> {
    let [file] = files else { return None };
    if file.kind != Kind::Add {
        return None;
    }
    let content: String = file
        .hunks
        .iter()
        .flatten()
        .map(|l| format!("{}\n", l.strip_prefix('+').unwrap_or(l)))
        .collect();
    Some((&file.path, content))
}

/// `structuredPatch` hunks for the diff modal. `patchText` carries no line numbers, so every hunk
/// starts at 1. A line naming the file is added wherever the modal's single path would not tell:
/// several files, a move, or a deletion.
pub fn structured_hunks(files: &[FilePatch]) -> Vec<Value> {
    let mut out = Vec::new();
    for file in files {
        let path = &file.path;
        let header = match (file.kind, &file.move_to) {
            (Kind::Delete, _) => {
                out.push(hunk(vec![format!("-{path} (deleted)")]));
                continue;
            }
            (_, Some(to)) => Some(format!(" *** Move: {path} -> {to}")),
            (Kind::Add, None) => (files.len() > 1).then(|| format!(" *** Add File: {path}")),
            (Kind::Update, None) => (files.len() > 1).then(|| format!(" *** Update File: {path}")),
        };
        let mut hunks: Vec<Vec<String>> = file
            .hunks
            .iter()
            .filter(|h| !h.is_empty())
            .cloned()
            .collect();
        if let Some(header) = header {
            match hunks.first_mut() {
                Some(first) => first.insert(0, header),
                None => hunks.push(vec![header]),
            }
        }
        out.extend(hunks.into_iter().map(hunk));
    }
    out
}

/// One hunk showing `text` as it is, for a patch that could not be parsed.
pub fn raw_hunk(text: &str) -> Value {
    hunk(text.lines().map(|l| format!(" {l}")).collect())
}

fn hunk(lines: Vec<String>) -> Value {
    json!({"oldStart": 1, "newStart": 1, "lines": lines})
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn wrap(body: &str) -> String {
        format!("*** Begin Patch\n{body}*** End Patch\n")
    }

    fn lines(hunks: &[Value]) -> Vec<Vec<&str>> {
        hunks
            .iter()
            .map(|h| {
                h["lines"]
                    .as_array()
                    .expect("lines")
                    .iter()
                    .map(|l| l.as_str().expect("line"))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_single_added_file_gives_its_content() {
        let files = parse(&wrap("*** Add File: d/a.txt\n+one\n+\n+two\n")).expect("parsed");
        assert_eq!(single_add(&files), Some(("d/a.txt", "one\n\ntwo\n".into())));
    }

    #[test]
    fn an_empty_added_file_has_empty_content() {
        let files = parse(&wrap("*** Add File: a\n")).expect("parsed");
        assert_eq!(single_add(&files), Some(("a", String::new())));
    }

    #[test]
    fn two_adds_are_not_a_single_add_and_each_is_named() {
        let files = parse(&wrap("*** Add File: a\n+1\n*** Add File: b\n+2\n")).expect("parsed");
        assert_eq!(single_add(&files), None);
        assert_eq!(
            lines(&structured_hunks(&files)),
            [
                vec![" *** Add File: a", "+1"],
                vec![" *** Add File: b", "+2"]
            ]
        );
    }

    #[test]
    fn an_update_splits_hunks_at_at_signs_and_keeps_prefixes() {
        let body = "*** Update File: a\n@@ fn x\n ctx\n-old\n+new\n@@\n\n-z\n*** End of File\n";
        let files = parse(&wrap(body)).expect("parsed");
        let hunks = structured_hunks(&files);
        assert_eq!(
            lines(&hunks),
            [vec![" ctx", "-old", "+new"], vec![" ", "-z"]]
        );
        assert_eq!(
            (hunks[0]["oldStart"].as_u64(), hunks[0]["newStart"].as_u64()),
            (Some(1), Some(1))
        );
    }

    #[test]
    fn a_deletion_has_a_visible_line() {
        let files = parse(&wrap("*** Update File: a\n-x\n*** Delete File: b\n")).expect("parsed");
        assert_eq!(
            lines(&structured_hunks(&files)),
            [vec![" *** Update File: a", "-x"], vec!["-b (deleted)"]]
        );
        let only = parse(&wrap("*** Delete File: b\n")).expect("parsed");
        assert_eq!(lines(&structured_hunks(&only)), [vec!["-b (deleted)"]]);
    }

    #[test]
    fn a_move_is_named_even_with_nothing_else_to_show() {
        let edit =
            parse(&wrap("*** Update File: a\n*** Move to: c\n@@\n-x\n+y\n")).expect("parsed");
        assert_eq!(
            lines(&structured_hunks(&edit)),
            [vec![" *** Move: a -> c", "-x", "+y"]]
        );
        let pure = parse(&wrap("*** Update File: a\n*** Move to: c\n")).expect("parsed");
        assert_eq!(lines(&structured_hunks(&pure)), [vec![" *** Move: a -> c"]]);
    }

    #[test]
    fn carriage_returns_and_text_around_the_patch_are_ignored() {
        let text =
            "<<'EOF'\r\n*** Begin Patch\r\n*** Add File: a\r\n+x\r\n*** End Patch\r\nEOF\r\n";
        let files = parse(text).expect("parsed");
        assert_eq!(single_add(&files), Some(("a", "x\n".into())));
    }

    #[test]
    fn anything_not_understood_is_rejected_whole() {
        for bad in [
            String::new(),
            "just text".into(),
            wrap(""),
            "*** Begin Patch\n*** Add File: a\n+x\n".into(),
            wrap("+orphan\n"),
            wrap("*** Add File: \n+x\n"),
            wrap("*** Add File: a\nunprefixed\n"),
            wrap("*** Update File: a\nunprefixed\n"),
            wrap("*** Delete File: a\n-x\n"),
            wrap("*** Add File: a\n*** Move to: b\n"),
            wrap("*** Update File: a\n*** Chmod: 777\n"),
        ] {
            assert_eq!(parse(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_raw_hunk_shows_every_line_as_context() {
        assert_eq!(lines(&[raw_hunk("a\n-b")]), [vec![" a", " -b"]]);
    }
}

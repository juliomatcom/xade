//! Pure diff model for the review modal: rows are built from old/new text or from `structuredPatch` hunks.

use std::path::{Path, PathBuf};

use serde_json::Value;
use similar::{DiffOp, TextDiff};

/// Past this many lines on either side the modal says "File too large to diff".
pub const MAX_LINES: usize = 5_000;
/// Files bigger than this are never read from disk for a pending diff.
const MAX_READ_BYTES: u64 = 2_000_000;

pub fn is_code_change(tool: &str) -> bool {
    matches!(tool, "Edit" | "MultiEdit" | "Write" | "NotebookEdit")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Same,
    Del,
    Add,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub line_no: usize,
    pub text: String,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// Section label between edits or hunks.
    Sep(String),
    Pair(Option<Cell>, Option<Cell>),
}

#[derive(Clone, Debug, Default)]
pub struct DiffData {
    /// Absolute path of the changed file.
    pub path: PathBuf,
    pub language: &'static str,
    pub rows: Vec<Row>,
    /// Line (in the new text) of the first change; 1 when there is none.
    pub first_changed_line: usize,
    pub too_large: bool,
    /// Add and Del cells, counted before a too-large diff drops its rows.
    pub added: usize,
    pub removed: usize,
}

impl DiffData {
    /// Builds the diff of a code-change tool. `result_json` is the stored `tool_use_result`, when the tool has finished.
    pub fn build(
        tool: &str,
        input_json: &str,
        result_json: Option<&str>,
        cwd: &Path,
    ) -> Option<DiffData> {
        Self::build_with(tool, input_json, result_json, cwd, true)
    }

    /// Like `build` but never reads the file, so line numbers may restart at 1; for counting.
    pub fn build_offline(
        tool: &str,
        input_json: &str,
        result_json: Option<&str>,
        cwd: &Path,
    ) -> Option<DiffData> {
        Self::build_with(tool, input_json, result_json, cwd, false)
    }

    /// The diff of a whole file from `old` to `new` text.
    pub fn from_texts(path: &Path, old: &str, new: &str) -> DiffData {
        let rows = collapse_unchanged(rows_from_text(old, new, 1));
        let too_large = rows.len() > 2 * MAX_LINES;
        let first_changed_line = first_changed(&rows);
        let (added, removed) = count_changes(&rows);
        DiffData {
            language: crate::widgets::tool_card::language_from_path(&path.to_string_lossy()),
            path: path.to_path_buf(),
            rows: if too_large { Vec::new() } else { rows },
            first_changed_line,
            too_large,
            added,
            removed,
        }
    }

    /// One diff of several edits to the same file, in order, a `Row::Sep` section per part.
    pub fn merge(mut parts: Vec<DiffData>) -> Option<DiffData> {
        if parts.len() < 2 {
            return parts.pop();
        }
        let total = parts.len();
        let mut merged = DiffData {
            path: parts[0].path.clone(),
            language: parts[0].language,
            first_changed_line: parts[0].first_changed_line,
            ..DiffData::default()
        };
        for (i, p) in parts.into_iter().enumerate() {
            merged.too_large |= p.too_large;
            merged.added += p.added;
            merged.removed += p.removed;
            merged
                .rows
                .push(Row::Sep(format!("Edit {} of {total}", i + 1)));
            merged
                .rows
                .extend(p.rows.into_iter().filter(|r| !matches!(r, Row::Sep(_))));
        }
        merged.too_large |= merged.rows.len() > 2 * MAX_LINES;
        if merged.too_large {
            merged.rows.clear();
        }
        Some(merged)
    }

    fn build_with(
        tool: &str,
        input_json: &str,
        result_json: Option<&str>,
        cwd: &Path,
        disk: bool,
    ) -> Option<DiffData> {
        let read = |p: &Path| if disk { read_capped(p) } else { None };
        let input: Value = serde_json::from_str(input_json).ok()?;
        let result: Option<Value> = result_json.and_then(|s| serde_json::from_str(s).ok());
        let key = if tool == "NotebookEdit" {
            "notebook_path"
        } else {
            "file_path"
        };
        let path = PathBuf::from(input.get(key)?.as_str()?);
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(&path)
        };
        let patch = result
            .as_ref()
            .and_then(|r| r.get("structuredPatch"))
            .and_then(Value::as_array)
            .filter(|h| !h.is_empty());
        let rows = if let Some(hunks) = patch {
            rows_from_hunks(hunks)
        } else {
            match tool {
                "Edit" => {
                    let old = str_of(&input, "old_string");
                    let start = read(&abs).and_then(|f| start_line(&f, &old)).unwrap_or(1);
                    rows_from_text(&old, &str_of(&input, "new_string"), start)
                }
                "MultiEdit" => multi_edit_rows(&input, read(&abs)),
                "Write" => {
                    let old = match &result {
                        Some(r) => str_of(r, "originalFile"),
                        None => read(&abs).unwrap_or_default(),
                    };
                    rows_from_text(&old, &str_of(&input, "content"), 1)
                }
                "NotebookEdit" => {
                    let old = result
                        .as_ref()
                        .map(|r| str_of(r, "old_source"))
                        .unwrap_or_default();
                    rows_from_text(&old, &str_of(&input, "new_source"), 1)
                }
                _ => return None,
            }
        };
        let too_large = rows.len() > 2 * MAX_LINES;
        let first_changed_line = first_changed(&rows);
        let (added, removed) = count_changes(&rows);
        Some(DiffData {
            language: crate::widgets::tool_card::language_from_path(&path.to_string_lossy()),
            path: abs,
            rows: if too_large { Vec::new() } else { rows },
            first_changed_line,
            too_large,
            added,
            removed,
        })
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn read_capped(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_READ_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// 1-based line where `needle` starts, only when it occurs exactly once.
pub fn start_line(file: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || file.matches(needle).count() != 1 {
        return None;
    }
    let at = file.find(needle)?;
    Some(file[..at].matches('\n').count() + 1)
}

fn multi_edit_rows(input: &Value, file: Option<String>) -> Vec<Row> {
    let edits = input
        .get("edits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = edits.len();
    let mut rows = Vec::new();
    for (i, e) in edits.iter().enumerate() {
        let old = str_of(e, "old_string");
        let start = file
            .as_deref()
            .and_then(|f| start_line(f, &old))
            .unwrap_or(1);
        rows.push(Row::Sep(format!("Edit {} of {total}", i + 1)));
        rows.extend(rows_from_text(&old, &str_of(e, "new_string"), start));
    }
    rows
}

/// Unchanged lines kept around each change in a whole-file diff.
const CONTEXT: usize = 3;

/// Replaces long runs of unchanged rows with a `Sep`, so a change deep in a file is on screen.
fn collapse_unchanged(rows: Vec<Row>) -> Vec<Row> {
    let same = |r: &Row| matches!(r, Row::Pair(Some(l), Some(r)) if l.kind == Kind::Same && r.kind == Kind::Same);
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if !same(&rows[i]) {
            out.push(rows[i].clone());
            i += 1;
            continue;
        }
        let end = (i..rows.len())
            .find(|&j| !same(&rows[j]))
            .unwrap_or(rows.len());
        let keep_head = if i == 0 { 0 } else { CONTEXT };
        let keep_tail = if end == rows.len() { 0 } else { CONTEXT };
        let hidden = (end - i).saturating_sub(keep_head + keep_tail);
        if hidden < 2 {
            out.extend(rows[i..end].iter().cloned());
        } else {
            out.extend(rows[i..i + keep_head].iter().cloned());
            out.push(Row::Sep(format!("{hidden} unchanged lines")));
            out.extend(rows[end - keep_tail..end].iter().cloned());
        }
        i = end;
    }
    out
}

fn count_changes(rows: &[Row]) -> (usize, usize) {
    let (mut added, mut removed) = (0, 0);
    for cell in rows.iter().flat_map(|r| match r {
        Row::Pair(l, r) => [l, r],
        Row::Sep(_) => [&None, &None],
    }) {
        match cell.as_ref().map(|c| c.kind) {
            Some(Kind::Add) => added += 1,
            Some(Kind::Del) => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

fn first_changed(rows: &[Row]) -> usize {
    rows.iter()
        .find_map(|r| match r {
            Row::Pair(l, r) => [r, l]
                .into_iter()
                .flatten()
                .find(|c| c.kind != Kind::Same)
                .map(|c| c.line_no),
            Row::Sep(_) => None,
        })
        .unwrap_or(1)
}

fn cell(line_no: usize, text: &str, kind: Kind) -> Cell {
    Cell {
        line_no,
        text: text.trim_end_matches(['\n', '\r']).to_string(),
        kind,
    }
}

/// Side-by-side rows; `start` is the line number of the first line on both sides.
pub fn rows_from_text(old: &str, new: &str, start: usize) -> Vec<Row> {
    let diff = TextDiff::from_lines(old, new);
    let a = |i: usize| diff.old_slice(i).unwrap_or_default();
    let b = |i: usize| diff.new_slice(i).unwrap_or_default();
    let off = start.saturating_sub(1);
    let dels = |at: usize, n: usize| -> Vec<Cell> {
        (0..n)
            .map(|i| cell(off + at + i + 1, a(at + i), Kind::Del))
            .collect()
    };
    let adds = |at: usize, n: usize| -> Vec<Cell> {
        (0..n)
            .map(|i| cell(off + at + i + 1, b(at + i), Kind::Add))
            .collect()
    };
    let mut rows = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for i in 0..len {
                    rows.push(Row::Pair(
                        Some(cell(off + old_index + i + 1, a(old_index + i), Kind::Same)),
                        Some(cell(off + new_index + i + 1, b(new_index + i), Kind::Same)),
                    ));
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => pair_runs(&mut rows, dels(old_index, old_len), Vec::new()),
            DiffOp::Insert {
                new_index, new_len, ..
            } => pair_runs(&mut rows, Vec::new(), adds(new_index, new_len)),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => pair_runs(
                &mut rows,
                dels(old_index, old_len),
                adds(new_index, new_len),
            ),
        }
    }
    rows
}

/// Aligns a run of removed lines with the run of added lines that replaces it.
fn pair_runs(rows: &mut Vec<Row>, dels: Vec<Cell>, adds: Vec<Cell>) {
    let n = dels.len().max(adds.len());
    let (mut d, mut a) = (dels.into_iter(), adds.into_iter());
    for _ in 0..n {
        rows.push(Row::Pair(d.next(), a.next()));
    }
}

/// Rows from the CLI's `structuredPatch` (`oldStart`, `newStart`, `lines` prefixed ' ', '-', '+').
pub fn rows_from_hunks(hunks: &[Value]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (i, h) in hunks.iter().enumerate() {
        let num = |k: &str| h.get(k).and_then(Value::as_u64).unwrap_or(1) as usize;
        let (mut old_no, mut new_no) = (num("oldStart"), num("newStart"));
        if hunks.len() > 1 {
            rows.push(Row::Sep(format!("Hunk {} of {}", i + 1, hunks.len())));
        }
        let (mut dels, mut adds) = (Vec::new(), Vec::new());
        let lines = h
            .get("lines")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for line in lines.iter().filter_map(Value::as_str) {
            let (mark, text) = line.split_at(line.chars().next().map_or(0, char::len_utf8));
            match mark {
                "-" => {
                    dels.push(cell(old_no, text, Kind::Del));
                    old_no += 1;
                }
                "+" => {
                    adds.push(cell(new_no, text, Kind::Add));
                    new_no += 1;
                }
                "\\" => {}
                _ => {
                    pair_runs(
                        &mut rows,
                        std::mem::take(&mut dels),
                        std::mem::take(&mut adds),
                    );
                    rows.push(Row::Pair(
                        Some(cell(old_no, text, Kind::Same)),
                        Some(cell(new_no, text, Kind::Same)),
                    ));
                    old_no += 1;
                    new_no += 1;
                }
            }
        }
        pair_runs(&mut rows, dels, adds);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type C<'a> = Option<(usize, &'a str, Kind)>;

    fn pair(r: &Row) -> (C<'_>, C<'_>) {
        let Row::Pair(l, r) = r else {
            panic!("not a pair")
        };
        fn f(c: &Option<Cell>) -> C<'_> {
            c.as_ref().map(|c| (c.line_no, c.text.as_str(), c.kind))
        }
        (f(l), f(r))
    }

    #[test]
    fn text_diff_aligns_replace_and_numbers_from_start() {
        let rows = rows_from_text("a\nb\nc\n", "a\nB\nc\nd\n", 10);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            pair(&rows[0]),
            (Some((10, "a", Kind::Same)), Some((10, "a", Kind::Same)))
        );
        assert_eq!(
            pair(&rows[1]),
            (Some((11, "b", Kind::Del)), Some((11, "B", Kind::Add)))
        );
        assert_eq!(pair(&rows[3]), (None, Some((13, "d", Kind::Add))));
        assert_eq!(first_changed(&rows), 11);
    }

    #[test]
    fn a_whole_file_diff_hides_far_unchanged_lines() {
        let old: String = (1..=100).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l50\n", "changed\n");
        let d = DiffData::from_texts(Path::new("/w/a.txt"), &old, &new);
        assert_eq!((d.added, d.removed, d.first_changed_line), (1, 1, 50));
        assert!(d.rows.len() < 15, "{}", d.rows.len());
        assert!(matches!(&d.rows[0], Row::Sep(t) if t == "46 unchanged lines"));
    }

    #[test]
    fn pure_delete_leaves_filler_on_the_right() {
        let rows = rows_from_text("x\ny\n", "x\n", 1);
        assert_eq!(pair(&rows[1]), (Some((2, "y", Kind::Del)), None));
    }

    #[test]
    fn hunks_use_real_line_numbers_and_skip_markers() {
        let hunks = vec![json!({
            "oldStart": 5, "newStart": 5, "oldLines": 3, "newLines": 3,
            "lines": [" keep", "-old one", "-old two", "+new one", "\\ No newline at end of file"]
        })];
        let rows = rows_from_hunks(&hunks);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            pair(&rows[0]),
            (Some((5, "keep", Kind::Same)), Some((5, "keep", Kind::Same)))
        );
        assert_eq!(
            pair(&rows[1]),
            (
                Some((6, "old one", Kind::Del)),
                Some((6, "new one", Kind::Add))
            )
        );
        assert_eq!(pair(&rows[2]), (Some((7, "old two", Kind::Del)), None));
    }

    #[test]
    fn two_hunks_get_separators() {
        let h = json!({"oldStart": 1, "newStart": 1, "lines": ["-a", "+b"]});
        let rows = rows_from_hunks(&[h.clone(), h]);
        assert!(matches!(rows[0], Row::Sep(_)));
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn start_line_requires_a_unique_match() {
        assert_eq!(start_line("a\nb\nc\n", "c"), Some(3));
        assert_eq!(start_line("x\nx\n", "x"), None);
        assert_eq!(start_line("a", "z"), None);
    }

    #[test]
    fn build_prefers_stored_patch_then_falls_back() {
        let input = r#"{"file_path":"/nonexistent/f.rs","old_string":"a","new_string":"b"}"#;
        let patch = r#"{"structuredPatch":[{"oldStart":7,"newStart":7,"lines":["-a","+b"]}]}"#;
        let d = DiffData::build("Edit", input, Some(patch), Path::new("/")).unwrap();
        assert_eq!(d.first_changed_line, 7);
        assert_eq!(d.language, "rust");
        let d = DiffData::build("Edit", input, None, Path::new("/")).unwrap();
        assert_eq!(d.first_changed_line, 1);
    }

    #[test]
    fn write_uses_original_file_when_patch_is_empty() {
        let input = r#"{"file_path":"/nonexistent/n.txt","content":"hi\n"}"#;
        let res = r#"{"type":"update","structuredPatch":[],"originalFile":"yo\n"}"#;
        let d = DiffData::build("Write", input, Some(res), Path::new("/")).unwrap();
        assert_eq!(
            pair(&d.rows[0]),
            (Some((1, "yo", Kind::Del)), Some((1, "hi", Kind::Add)))
        );
        let created = r#"{"type":"create","structuredPatch":[],"originalFile":null}"#;
        let d = DiffData::build("Write", input, Some(created), Path::new("/")).unwrap();
        assert_eq!(pair(&d.rows[0]), (None, Some((1, "hi", Kind::Add))));
    }

    #[test]
    fn multi_edit_has_one_section_per_edit() {
        let input = r#"{"file_path":"/nonexistent/m.rs","edits":[{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"d"}]}"#;
        let d = DiffData::build("MultiEdit", input, None, Path::new("/")).unwrap();
        let seps = d.rows.iter().filter(|r| matches!(r, Row::Sep(_))).count();
        assert_eq!(seps, 2);
    }

    #[test]
    fn counts_and_merge_cover_every_edit() {
        let input = r#"{"file_path":"/nonexistent/m.rs","edits":[{"old_string":"a","new_string":"b\nc"},{"old_string":"x\ny","new_string":"z"}]}"#;
        let d = DiffData::build_offline("MultiEdit", input, None, Path::new("/")).unwrap();
        assert_eq!((d.added, d.removed), (3, 3));
        let m = DiffData::merge(vec![d.clone(), d]).unwrap();
        assert_eq!((m.added, m.removed), (6, 6));
        let seps: Vec<_> = m.rows.iter().filter(|r| matches!(r, Row::Sep(_))).collect();
        assert_eq!(seps.len(), 2);
        assert_eq!(m.rows[0], Row::Sep("Edit 1 of 2".into()));
    }
}

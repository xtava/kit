use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use similar::{Algorithm, ChangeTag, DiffOp, InlineChangeOptions, TextDiff};

const CONTEXT_LINES: usize = 3;
const DIFF_TIMEOUT: Duration = Duration::from_millis(250);
const INLINE_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChangeGroup {
    Staged,
    Changes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflict,
    Submodule,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpecialState {
    Conflict,
    Submodule { state: String },
}

#[derive(Clone, Debug)]
pub enum SourceSnapshot {
    Absent,
    Bytes(Arc<[u8]>),
    Unavailable(String),
}

impl SourceSnapshot {
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self::Bytes(bytes.into())
    }
}

#[derive(Clone, Debug)]
pub struct DiffInput {
    pub group: ChangeGroup,
    pub kind: ChangeKind,
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub old: SourceSnapshot,
    pub new: SourceSnapshot,
    pub special: Option<SpecialState>,
}

#[derive(Clone, Debug)]
pub struct DiffDocument {
    pub group: ChangeGroup,
    pub kind: ChangeKind,
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub additions: Option<usize>,
    pub deletions: Option<usize>,
    pub body: DiffBody,
}

#[derive(Clone, Debug)]
pub enum DiffBody {
    Text(TextDiffDocument),
    Binary,
    NonUtf8,
    TooLarge { old_bytes: usize, new_bytes: usize },
    Unavailable(String),
    Special(SpecialState),
}

#[derive(Clone, Debug)]
pub struct TextDiffDocument {
    pub old: TextSnapshot,
    pub new: TextSnapshot,
    pub hunks: Vec<Hunk>,
}

#[derive(Clone, Debug)]
pub struct TextSnapshot {
    text: Arc<str>,
    lines: Vec<Range<usize>>,
}

impl TextSnapshot {
    fn new(text: Arc<str>) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                lines.push(start..index + 1);
                start = index + 1;
            }
        }
        if start < text.len() {
            lines.push(start..text.len());
        }
        Self { text, lines }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn line(&self, index: usize) -> &str {
        &self.text[self.lines[index].clone()]
    }

    pub fn display_line(&self, index: usize) -> &str {
        let line = self.line(index);
        let line = line.strip_suffix('\n').unwrap_or(line);
        line.strip_suffix('\r').unwrap_or(line)
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn missing_final_newline(&self, index: usize) -> bool {
        !self.line(index).ends_with('\n')
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hunk {
    pub id: usize,
    pub old_range: Range<usize>,
    pub new_range: Range<usize>,
    pub rows: Vec<AlignedRow>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlignedRow {
    pub kind: RowKind,
    pub old: Option<LineCell>,
    pub new: Option<LineCell>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowKind {
    Context,
    Changed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineCell {
    pub line_index: usize,
    pub emphasis: Vec<Range<usize>>,
    pub missing_newline: bool,
}

impl DiffDocument {
    pub fn build(input: DiffInput) -> Self {
        let DiffInput { group, kind, old_path, new_path, old, new, special } = input;
        let (body, additions, deletions) = if let Some(special) = special {
            (DiffBody::Special(special), None, None)
        } else {
            build_body(old, new)
        };
        Self { group, kind, old_path, new_path, additions, deletions, body }
    }

    pub fn display_path(&self) -> Option<&std::path::Path> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }
}

fn build_body(
    old: SourceSnapshot,
    new: SourceSnapshot,
) -> (DiffBody, Option<usize>, Option<usize>) {
    let old = match snapshot_bytes(old) {
        Ok(bytes) => bytes,
        Err(error) => return (DiffBody::Unavailable(error), None, None),
    };
    let new = match snapshot_bytes(new) {
        Ok(bytes) => bytes,
        Err(error) => return (DiffBody::Unavailable(error), None, None),
    };

    if old.len().saturating_add(new.len()) > MAX_DIFF_BYTES {
        return (DiffBody::TooLarge { old_bytes: old.len(), new_bytes: new.len() }, None, None);
    }
    if old.contains(&0) || new.contains(&0) {
        return (DiffBody::Binary, None, None);
    }
    let (Ok(old), Ok(new)) = (std::str::from_utf8(&old), std::str::from_utf8(&new)) else {
        return (DiffBody::NonUtf8, None, None);
    };

    let old = Arc::<str>::from(old);
    let new = Arc::<str>::from(new);
    let mut config = TextDiff::configure();
    config.algorithm(Algorithm::Histogram).timeout(DIFF_TIMEOUT);
    let diff = config.diff_lines(old.as_ref(), new.as_ref());
    let old_snapshot = TextSnapshot::new(Arc::clone(&old));
    let new_snapshot = TextSnapshot::new(Arc::clone(&new));
    let hunks = build_hunks(&diff, &old_snapshot, &new_snapshot);
    let additions = hunks
        .iter()
        .flat_map(|hunk| &hunk.rows)
        .filter(|row| row.kind == RowKind::Changed && row.new.is_some())
        .count();
    let deletions = hunks
        .iter()
        .flat_map(|hunk| &hunk.rows)
        .filter(|row| row.kind == RowKind::Changed && row.old.is_some())
        .count();

    (
        DiffBody::Text(TextDiffDocument { old: old_snapshot, new: new_snapshot, hunks }),
        Some(additions),
        Some(deletions),
    )
}

fn snapshot_bytes(snapshot: SourceSnapshot) -> Result<Arc<[u8]>, String> {
    match snapshot {
        SourceSnapshot::Absent => Ok(Arc::from([])),
        SourceSnapshot::Bytes(bytes) => Ok(bytes),
        SourceSnapshot::Unavailable(error) => Err(error),
    }
}

fn build_hunks(diff: &TextDiff<'_, '_, str>, old: &TextSnapshot, new: &TextSnapshot) -> Vec<Hunk> {
    diff.grouped_ops(CONTEXT_LINES)
        .into_iter()
        .enumerate()
        .map(|(id, ops)| {
            let old_range = ops.first().map(DiffOp::old_range).unwrap_or(0..0).start
                ..ops.last().map(DiffOp::old_range).unwrap_or(0..0).end;
            let new_range = ops.first().map(DiffOp::new_range).unwrap_or(0..0).start
                ..ops.last().map(DiffOp::new_range).unwrap_or(0..0).end;
            let mut rows = Vec::new();
            for op in ops {
                append_rows(&mut rows, diff, &op, old, new);
            }
            Hunk { id, old_range, new_range, rows }
        })
        .collect()
}

fn append_rows(
    rows: &mut Vec<AlignedRow>,
    diff: &TextDiff<'_, '_, str>,
    op: &DiffOp,
    old: &TextSnapshot,
    new: &TextSnapshot,
) {
    let old_range = op.old_range();
    let new_range = op.new_range();
    match op {
        DiffOp::Equal { .. } => {
            for (old_index, new_index) in old_range.zip(new_range) {
                rows.push(AlignedRow {
                    kind: RowKind::Context,
                    old: Some(line_cell(old, old_index, Vec::new())),
                    new: Some(line_cell(new, new_index, Vec::new())),
                });
            }
        }
        DiffOp::Delete { .. } => {
            for old_index in old_range {
                rows.push(AlignedRow {
                    kind: RowKind::Changed,
                    old: Some(line_cell(old, old_index, Vec::new())),
                    new: None,
                });
            }
        }
        DiffOp::Insert { .. } => {
            for new_index in new_range {
                rows.push(AlignedRow {
                    kind: RowKind::Changed,
                    old: None,
                    new: Some(line_cell(new, new_index, Vec::new())),
                });
            }
        }
        DiffOp::Replace { .. } => {
            let (old_emphasis, new_emphasis) = replacement_emphasis(diff, op);
            let row_count = old_range.len().max(new_range.len());
            for offset in 0..row_count {
                let old_index = old_range.get(offset);
                let new_index = new_range.get(offset);
                rows.push(AlignedRow {
                    kind: RowKind::Changed,
                    old: old_index.map(|index| {
                        line_cell(old, index, old_emphasis.get(&index).cloned().unwrap_or_default())
                    }),
                    new: new_index.map(|index| {
                        line_cell(new, index, new_emphasis.get(&index).cloned().unwrap_or_default())
                    }),
                });
            }
        }
    }
}

type EmphasisByLine = HashMap<usize, Vec<Range<usize>>>;

fn replacement_emphasis(
    diff: &TextDiff<'_, '_, str>,
    op: &DiffOp,
) -> (EmphasisByLine, EmphasisByLine) {
    let mut options = InlineChangeOptions::new();
    options.semantic_cleanup(true);
    let deadline = Some(Instant::now() + INLINE_TIMEOUT);
    let mut old = HashMap::new();
    let mut new = HashMap::new();
    for change in diff.iter_inline_changes_with_options_deadline(op, options, deadline) {
        let mut offset = 0;
        let mut emphasis = Vec::new();
        for (emphasized, value) in change.values() {
            let end = offset + value.len();
            if *emphasized {
                emphasis.push(offset..end);
            }
            offset = end;
        }
        match change.tag() {
            ChangeTag::Delete => {
                if let Some(index) = change.old_index() {
                    old.insert(index, emphasis);
                }
            }
            ChangeTag::Insert => {
                if let Some(index) = change.new_index() {
                    new.insert(index, emphasis);
                }
            }
            ChangeTag::Equal => {}
        }
    }
    (old, new)
}

fn line_cell(snapshot: &TextSnapshot, line_index: usize, emphasis: Vec<Range<usize>>) -> LineCell {
    LineCell { line_index, emphasis, missing_newline: snapshot.missing_final_newline(line_index) }
}

trait RangeIndex {
    fn get(&self, offset: usize) -> Option<usize>;
}

impl RangeIndex for Range<usize> {
    fn get(&self, offset: usize) -> Option<usize> {
        (offset < self.len()).then_some(self.start + offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(old: &[u8], new: &[u8]) -> DiffDocument {
        DiffDocument::build(DiffInput {
            group: ChangeGroup::Changes,
            kind: ChangeKind::Modified,
            old_path: Some("sample.rs".into()),
            new_path: Some("sample.rs".into()),
            old: SourceSnapshot::from_bytes(old),
            new: SourceSnapshot::from_bytes(new),
            special: None,
        })
    }

    fn text(document: &DiffDocument) -> &TextDiffDocument {
        match &document.body {
            DiffBody::Text(text) => text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn replacement_has_one_canonical_aligned_row_and_inline_emphasis() {
        let document = document(b"let answer = 41;\n", b"let answer = 42;\n");
        let text = text(&document);
        let changed = text.hunks[0].rows.iter().find(|row| row.kind == RowKind::Changed).unwrap();

        assert!(changed.old.is_some());
        assert!(changed.new.is_some());
        assert!(!changed.old.as_ref().unwrap().emphasis.is_empty());
        assert!(!changed.new.as_ref().unwrap().emphasis.is_empty());
        assert_eq!((document.additions, document.deletions), (Some(1), Some(1)));
    }

    #[test]
    fn asymmetric_replacement_uses_explicit_empty_side() {
        let document = document(b"one\ntwo\n", b"ONE\nTWO\nthree\n");
        let changed: Vec<_> = text(&document)
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.rows)
            .filter(|row| row.kind == RowKind::Changed)
            .collect();

        assert_eq!(changed.len(), 3);
        assert!(changed[2].old.is_none());
        assert!(changed[2].new.is_some());
        assert_eq!((document.additions, document.deletions), (Some(3), Some(2)));
    }

    #[test]
    fn final_newline_and_crlf_are_preserved() {
        let document = document(b"first\r\nsecond", b"first\r\nsecond\r\n");
        let text = text(&document);

        assert_eq!(text.old.line(0), "first\r\n");
        assert_eq!(text.old.display_line(0), "first");
        assert_eq!(text.old.line(1), "second");
        assert!(text.old.missing_final_newline(1));
        assert!(!text.new.missing_final_newline(1));
    }

    #[test]
    fn unicode_grapheme_changes_remain_valid_byte_ranges() {
        let document = document("emoji 👨‍👩‍👧\n".as_bytes(), "emoji 👨‍👩‍👦\n".as_bytes());
        let text = text(&document);
        for row in text.hunks.iter().flat_map(|hunk| &hunk.rows) {
            if let Some(cell) = &row.old {
                let line = text.old.line(cell.line_index);
                assert!(cell
                    .emphasis
                    .iter()
                    .all(|range| line.is_char_boundary(range.start)
                        && line.is_char_boundary(range.end)));
            }
            if let Some(cell) = &row.new {
                let line = text.new.line(cell.line_index);
                assert!(cell
                    .emphasis
                    .iter()
                    .all(|range| line.is_char_boundary(range.start)
                        && line.is_char_boundary(range.end)));
            }
        }
    }

    #[test]
    fn binary_non_utf8_and_unavailable_are_explicit() {
        assert!(matches!(document(b"a\0b", b"a\0c").body, DiffBody::Binary));
        assert!(matches!(document(&[0xff], &[0xfe]).body, DiffBody::NonUtf8));
        let document = DiffDocument::build(DiffInput {
            group: ChangeGroup::Changes,
            kind: ChangeKind::Modified,
            old_path: Some("gone".into()),
            new_path: Some("gone".into()),
            old: SourceSnapshot::Unavailable("read failed".into()),
            new: SourceSnapshot::Absent,
            special: None,
        });
        assert!(
            matches!(document.body, DiffBody::Unavailable(ref error) if error == "read failed")
        );
    }

    #[test]
    fn conflict_and_submodule_bypass_text_diffing() {
        for special in [SpecialState::Conflict, SpecialState::Submodule { state: "SCMU".into() }] {
            let document = DiffDocument::build(DiffInput {
                group: ChangeGroup::Changes,
                kind: ChangeKind::Conflict,
                old_path: Some("path".into()),
                new_path: Some("path".into()),
                old: SourceSnapshot::Absent,
                new: SourceSnapshot::Absent,
                special: Some(special.clone()),
            });
            assert!(matches!(document.body, DiffBody::Special(ref value) if value == &special));
        }
    }

    #[test]
    fn context_empty_tabs_and_long_lines_preserve_canonical_source() {
        let old = (0..20).map(|index| format!("line {index}\n")).collect::<String>();
        let new = old.replace("line 1\n", "changed 1\n").replace("line 18\n", "changed 18\n");
        let context_document = document(old.as_bytes(), new.as_bytes());
        assert_eq!(text(&context_document).hunks.len(), 2);

        let long = format!("\t{}\n", "wide".repeat(4_000));
        let changed = long.replace("wide\n", "changed\n");
        let long_document = document(long.as_bytes(), changed.as_bytes());
        assert_eq!(text(&long_document).old.text(), long);

        let added = document(b"", b"first\n");
        assert_eq!((added.additions, added.deletions), (Some(1), Some(0)));
        assert_eq!(text(&added).old.line_count(), 0);
    }

    #[test]
    fn oversized_text_is_an_explicit_state() {
        let old = vec![b'a'; MAX_DIFF_BYTES];
        let document = document(&old, b"b");

        assert!(matches!(
            document.body,
            DiffBody::TooLarge { old_bytes: MAX_DIFF_BYTES, new_bytes: 1 }
        ));
    }
}

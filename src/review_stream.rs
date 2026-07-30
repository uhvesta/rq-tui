//! A renderer-independent semantic stream for Review mode.
//!
//! The stream deliberately has no terminal, ratatui, or I/O concerns.  A
//! renderer may display the same rows as a unified stream or as a split pair
//! of source columns.  Inline annotation rows are still one-dimensional rows
//! in both cases, which keeps cursor movement and source anchoring stable.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::diff::{DiffFile, FileStatus, LineKind};
use crate::terminal_text::{cell_width, grapheme_indices};

/// Which side of a source file a line anchor refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SourceSide {
    Old,
    New,
}

/// A changed file together with the repository identity needed to disambiguate
/// equal relative paths in a multi-repository Work Item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewFile {
    pub(crate) repo_id: String,
    pub(crate) repo_name: String,
    pub(crate) file: DiffFile,
}

impl ReviewFile {
    pub(crate) fn new(
        repo_id: impl Into<String>,
        repo_name: impl Into<String>,
        file: DiffFile,
    ) -> Self {
        Self {
            repo_id: repo_id.into(),
            repo_name: repo_name.into(),
            file,
        }
    }
}

/// A stable, renderer-independent source location.
///
/// `hunk` and `line` identify the parsed diff row, while `source_line` is the
/// file line number used for placement and display.  Keeping both makes the
/// mapping resilient to duplicate line numbers in a split diff.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SourceAnchor {
    pub(crate) repo_id: String,
    pub(crate) file: String,
    pub(crate) hunk: usize,
    pub(crate) line: usize,
    pub(crate) visible_line: usize,
    pub(crate) side: SourceSide,
    pub(crate) source_line: usize,
}

impl SourceAnchor {
    fn new(
        repo_id: &str,
        file: &Path,
        hunk: usize,
        line: usize,
        visible_line: usize,
        side: SourceSide,
        source_line: usize,
    ) -> Self {
        Self {
            repo_id: repo_id.to_owned(),
            file: file.to_string_lossy().into_owned(),
            hunk,
            line,
            visible_line,
            side,
            source_line,
        }
    }
}

/// Input for one inline comment or ask block.
///
/// The body is kept as text and split only at explicit newlines.  Renderers
/// can soft-wrap it to their available width without changing the semantic
/// row count or any source mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InlineAnnotation {
    pub(crate) id: String,
    pub(crate) repo_id: String,
    pub(crate) file_path: PathBuf,
    pub(crate) side: SourceSide,
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
    pub(crate) title: String,
    pub(crate) body: String,
    /// Optional affordance rendered inside this annotation's border.  Keeping
    /// the prompt on the annotation makes follow-up focus stable while the
    /// response body grows.
    pub(crate) prompt: Option<String>,
    pub(crate) collapsed: bool,
}

impl InlineAnnotation {
    pub(crate) fn new(
        id: impl Into<String>,
        file_path: impl Into<PathBuf>,
        side: SourceSide,
        line_start: usize,
        line_end: usize,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            repo_id: String::new(),
            file_path: file_path.into(),
            side,
            line_start,
            line_end: line_end.max(line_start),
            title: title.into(),
            body: body.into(),
            prompt: None,
            collapsed: false,
        }
    }

    pub(crate) fn in_repo(mut self, repo_id: impl Into<String>) -> Self {
        self.repo_id = repo_id.into();
        self
    }
}

/// Stable identity for a semantic row.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ReviewRowKey {
    FileHeader {
        repo_id: String,
        file: String,
    },
    HunkHeader {
        repo_id: String,
        file: String,
        hunk: usize,
    },
    Source {
        repo_id: String,
        file: String,
        hunk: usize,
        line: usize,
    },
    Fold {
        repo_id: String,
        file: String,
        hunk: usize,
        line: usize,
    },
    Annotation {
        id: String,
        block_line: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnnotationRowPart {
    Header { collapsed: bool },
    Body { line: usize, total: usize },
    Prompt,
    Footer,
}

/// The semantic payload of an inline annotation row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AnnotationRow {
    pub(crate) annotation_id: String,
    pub(crate) anchor: SourceAnchor,
    pub(crate) part: AnnotationRowPart,
    pub(crate) text: String,
}

/// A single row in the Review stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReviewRow {
    FileHeader {
        key: ReviewRowKey,
        repo_id: String,
        repo_name: String,
        path: String,
        status: FileStatus,
    },
    HunkHeader {
        key: ReviewRowKey,
        repo_id: String,
        file: String,
        hunk: usize,
        text: String,
    },
    Source {
        key: ReviewRowKey,
        repo_id: String,
        file: String,
        hunk: usize,
        line: usize,
        kind: LineKind,
        old_line: Option<usize>,
        new_line: Option<usize>,
        content: String,
        old_anchor: Option<SourceAnchor>,
        new_anchor: Option<SourceAnchor>,
    },
    Fold {
        key: ReviewRowKey,
        repo_id: String,
        file: String,
        hunk: usize,
        line: usize,
        hidden_lines: usize,
        text: String,
    },
    Annotation {
        key: ReviewRowKey,
        block: AnnotationRow,
    },
}

impl ReviewRow {
    #[cfg(test)]
    pub(crate) fn key(&self) -> &ReviewRowKey {
        match self {
            Self::FileHeader { key, .. }
            | Self::HunkHeader { key, .. }
            | Self::Source { key, .. }
            | Self::Fold { key, .. }
            | Self::Annotation { key, .. } => key,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_annotation(&self) -> bool {
        matches!(self, Self::Annotation { .. })
    }

    pub(crate) fn is_annotation_header(&self) -> bool {
        matches!(
            self,
            Self::Annotation {
                block: AnnotationRow {
                    part: AnnotationRowPart::Header { .. },
                    ..
                },
                ..
            }
        )
    }

    pub(crate) fn source_anchor(&self) -> Option<&SourceAnchor> {
        match self {
            Self::Source {
                old_anchor,
                new_anchor,
                ..
            } => new_anchor.as_ref().or(old_anchor.as_ref()),
            Self::Annotation { block, .. } => Some(&block.anchor),
            _ => None,
        }
    }
}

/// Cursor movement understood by the semantic stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamMovement {
    Up,
    Down,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    First,
    Last,
}

/// The complete Review stream and its semantic cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReviewStream {
    files: Vec<ReviewFile>,
    annotations: Vec<InlineAnnotation>,
    rows: Vec<ReviewRow>,
    cursor: usize,
}

/// One semantic row's position in a width-specific terminal layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReviewDisplayRow {
    semantic_row: usize,
    start: usize,
    height: usize,
}

impl ReviewDisplayRow {
    pub(crate) fn semantic_row(self) -> usize {
        self.semantic_row
    }

    pub(crate) fn start(self) -> usize {
        self.start
    }

    pub(crate) fn height(self) -> usize {
        self.height
    }
}

/// Compact, width-specific Review geometry. There is exactly one entry per
/// semantic row; wrapped terminal rows are represented by a start and height.
#[derive(Clone, Debug)]
pub(crate) struct ReviewDisplayLayout {
    semantic_revision: u64,
    width: usize,
    rows: Vec<ReviewDisplayRow>,
    total_height: usize,
    composer_cursor: Option<usize>,
    composer_visible_rows: usize,
}

impl ReviewDisplayLayout {
    pub(crate) fn for_rows(
        semantic_revision: u64,
        rows: &[ReviewRow],
        width: usize,
        composer_visible_rows: usize,
    ) -> Self {
        let width = width.max(1);
        let composer_visible_rows = composer_visible_rows.max(1);
        let mut layout_rows = Vec::with_capacity(rows.len());
        let mut composer_cursor = None;
        let mut start = 0usize;
        for (semantic_row, row) in rows.iter().enumerate() {
            let (height, cursor_line) = review_row_geometry(row, width, composer_visible_rows);
            let height = height.max(1);
            layout_rows.push(ReviewDisplayRow {
                semantic_row,
                start,
                height,
            });
            if let Some(cursor_line) = cursor_line {
                // Follow one display row past the insertion cursor so the
                // contextual editor's closing border remains in view.
                composer_cursor = Some(start.saturating_add(cursor_line).saturating_add(1));
            }
            start = start.saturating_add(height);
        }
        Self {
            semantic_revision,
            width,
            rows: layout_rows,
            total_height: start,
            composer_cursor,
            composer_visible_rows,
        }
    }

    pub(crate) fn matches(
        &self,
        semantic_revision: u64,
        width: usize,
        composer_visible_rows: usize,
    ) -> bool {
        self.semantic_revision == semantic_revision
            && self.width == width.max(1)
            && self.composer_visible_rows == composer_visible_rows.max(1)
    }

    pub(crate) fn rows(&self) -> &[ReviewDisplayRow] {
        &self.rows
    }

    pub(crate) fn row(&self, semantic_row: usize) -> Option<ReviewDisplayRow> {
        self.rows.get(semantic_row).copied()
    }

    pub(crate) fn total_height(&self) -> usize {
        self.total_height
    }

    pub(crate) fn composer_cursor(&self) -> Option<usize> {
        self.composer_cursor
    }

    pub(crate) fn viewport_start(&self, scroll: usize) -> usize {
        self.rows
            .partition_point(|display| display.start.saturating_add(display.height) <= scroll)
    }
}

fn review_row_geometry(
    row: &ReviewRow,
    width: usize,
    composer_visible_rows: usize,
) -> (usize, Option<usize>) {
    let ReviewRow::Annotation { block, .. } = row else {
        return (1, None);
    };
    let available = width.saturating_sub(4).max(1);
    if !matches!(
        block.part,
        AnnotationRowPart::Body { .. } | AnnotationRowPart::Prompt
    ) {
        return (1, None);
    }
    let (height, cursor_line) = wrapped_text_geometry(&block.text, available);
    let cursor = (block.annotation_id == "zzzzzzzz-inline-composer"
        || matches!(block.part, AnnotationRowPart::Prompt))
    .then_some(cursor_line)
    .flatten();
    let Some(cursor) = cursor else {
        return (height, None);
    };
    let visible_rows = composer_visible_rows.max(1).min(height);
    let scroll = cursor
        .saturating_add(1)
        .saturating_sub(visible_rows)
        .min(height.saturating_sub(visible_rows));
    (visible_rows, Some(cursor.saturating_sub(scroll)))
}

/// Matches the editor's terminal-cell wrapping: tabs occupy one cell and
/// grapheme clusters are never split.
fn wrapped_text_geometry(text: &str, width: usize) -> (usize, Option<usize>) {
    let width = width.max(1);
    let mut row = 0usize;
    let mut column = 0usize;
    let mut cursor_line = None;
    for (_, grapheme) in grapheme_indices(text) {
        if grapheme == "\n" {
            row = row.saturating_add(1);
            column = 0;
            continue;
        }
        let grapheme_width = cell_width(if grapheme == "\t" { " " } else { grapheme });
        if column > 0 && column.saturating_add(grapheme_width) > width {
            row = row.saturating_add(1);
            column = 0;
        }
        if grapheme == "▏" {
            cursor_line = Some(row);
        }
        column = column.saturating_add(grapheme_width);
    }
    (row.saturating_add(1), cursor_line)
}

impl ReviewStream {
    #[cfg(test)]
    pub(crate) fn new(files: &[DiffFile], annotations: &[InlineAnnotation]) -> Self {
        let files = files
            .iter()
            .cloned()
            .map(|file| ReviewFile::new("", "", file))
            .collect::<Vec<_>>();
        Self::for_files(&files, annotations)
    }

    pub(crate) fn for_files(files: &[ReviewFile], annotations: &[InlineAnnotation]) -> Self {
        let mut stream = Self {
            files: files.to_vec(),
            annotations: annotations.to_vec(),
            rows: Vec::new(),
            cursor: 0,
        };
        stream.rows = build_rows(&stream.files, &stream.annotations);
        stream
    }

    pub(crate) fn rows(&self) -> &[ReviewRow] {
        &self.rows
    }

    #[cfg(test)]
    pub(crate) fn row_count(&self) -> usize {
        self.rows.len()
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub(crate) fn current(&self) -> Option<&ReviewRow> {
        self.rows.get(self.cursor)
    }

    /// Return the cursor resulting from `movement` without mutating this
    /// immutable semantic snapshot.  The TUI owns its cursor separately, so
    /// this lets navigation share one cached stream instead of cloning all
    /// review rows for every key press.
    pub(crate) fn moved_cursor(
        &self,
        cursor: usize,
        movement: StreamMovement,
        viewport_rows: usize,
    ) -> usize {
        if self.rows.is_empty() {
            return 0;
        }
        let cursor = cursor.min(self.rows.len() - 1);
        let half = (viewport_rows / 2).max(1);
        let page = viewport_rows.max(1);
        match movement {
            StreamMovement::Up => cursor.saturating_sub(1),
            StreamMovement::Down => (cursor + 1).min(self.rows.len() - 1),
            StreamMovement::HalfPageUp => cursor.saturating_sub(half),
            StreamMovement::HalfPageDown => (cursor + half).min(self.rows.len() - 1),
            StreamMovement::PageUp => cursor.saturating_sub(page),
            StreamMovement::PageDown => (cursor + page).min(self.rows.len() - 1),
            StreamMovement::First => 0,
            StreamMovement::Last => self.rows.len() - 1,
        }
    }

    /// Find the next annotation header relative to the TUI-owned cursor.
    pub(crate) fn annotation_after(&self, cursor: usize, forward: bool) -> Option<usize> {
        let cursor = cursor.min(self.rows.len().saturating_sub(1));
        let mut headers = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.is_annotation_header().then_some(index));
        if forward {
            headers
                .find(|index| *index > cursor)
                .or_else(|| self.rows.iter().position(ReviewRow::is_annotation_header))
        } else {
            self.rows
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, row)| {
                    (index < cursor && row.is_annotation_header()).then_some(index)
                })
                .or_else(|| self.rows.iter().rposition(ReviewRow::is_annotation_header))
        }
    }

    #[cfg(test)]
    pub(crate) fn current_key(&self) -> Option<&ReviewRowKey> {
        self.current().map(ReviewRow::key)
    }

    #[cfg(test)]
    pub(crate) fn set_cursor(&mut self, row: usize) {
        self.cursor = row.min(self.rows.len().saturating_sub(1));
    }

    #[cfg(test)]
    pub(crate) fn move_by(&mut self, movement: StreamMovement, viewport_rows: usize) {
        if self.rows.is_empty() {
            self.cursor = 0;
            return;
        }
        let half = (viewport_rows / 2).max(1);
        let page = viewport_rows.max(1);
        match movement {
            StreamMovement::Up => self.cursor = self.cursor.saturating_sub(1),
            StreamMovement::Down => self.cursor = (self.cursor + 1).min(self.rows.len() - 1),
            StreamMovement::HalfPageUp => self.cursor = self.cursor.saturating_sub(half),
            StreamMovement::HalfPageDown => {
                self.cursor = (self.cursor + half).min(self.rows.len() - 1)
            }
            StreamMovement::PageUp => self.cursor = self.cursor.saturating_sub(page),
            StreamMovement::PageDown => self.cursor = (self.cursor + page).min(self.rows.len() - 1),
            StreamMovement::First => self.cursor = 0,
            StreamMovement::Last => self.cursor = self.rows.len() - 1,
        }
    }

    #[cfg(test)]
    pub(crate) fn half_page_up(&mut self, viewport_rows: usize) {
        self.move_by(StreamMovement::HalfPageUp, viewport_rows);
    }

    #[cfg(test)]
    pub(crate) fn half_page_down(&mut self, viewport_rows: usize) {
        self.move_by(StreamMovement::HalfPageDown, viewport_rows);
    }

    #[cfg(test)]
    pub(crate) fn page_down(&mut self, viewport_rows: usize) {
        self.move_by(StreamMovement::PageDown, viewport_rows);
    }

    #[cfg(test)]
    pub(crate) fn gg(&mut self) {
        self.move_by(StreamMovement::First, 1);
    }

    #[cfg(test)]
    pub(crate) fn g_end(&mut self) {
        self.move_by(StreamMovement::Last, 1);
    }

    #[cfg(test)]
    pub(crate) fn jump_annotation(&mut self, forward: bool) -> Option<usize> {
        let candidates: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.is_annotation_header().then_some(index))
            .collect();
        let target = if forward {
            candidates
                .iter()
                .copied()
                .find(|index| *index > self.cursor)
                .or_else(|| candidates.first().copied())
        } else {
            candidates
                .iter()
                .rev()
                .copied()
                .find(|index| *index < self.cursor)
                .or_else(|| candidates.last().copied())
        };
        if let Some(target) = target {
            self.cursor = target;
        }
        target
    }

    #[cfg(test)]
    pub(crate) fn row_for_source(&self, anchor: &SourceAnchor) -> Option<usize> {
        self.rows.iter().position(|row| match row {
            ReviewRow::Source {
                old_anchor,
                new_anchor,
                ..
            } => old_anchor.as_ref() == Some(anchor) || new_anchor.as_ref() == Some(anchor),
            ReviewRow::Annotation { block, .. } => block.anchor == *anchor,
            _ => false,
        })
    }

    #[cfg(test)]
    pub(crate) fn annotation_row(&self, id: &str) -> Option<usize> {
        self.rows.iter().position(
            |row| matches!(row, ReviewRow::Annotation { block, .. } if block.annotation_id == id),
        )
    }

    /// Re-fold an annotation while preserving the user's semantic position.
    #[cfg(test)]
    pub(crate) fn set_annotation_collapsed(&mut self, id: &str, collapsed: bool) -> bool {
        let Some(annotation_index) = self
            .annotations
            .iter()
            .position(|annotation| annotation.id == id)
        else {
            return false;
        };
        if self.annotations[annotation_index].collapsed == collapsed {
            return true;
        }
        let previous_key = self.current_key().cloned();
        let previous_anchor = self.current().and_then(ReviewRow::source_anchor).cloned();
        self.annotations[annotation_index].collapsed = collapsed;
        self.rows = build_rows(&self.files, &self.annotations);
        self.restore_cursor(previous_key.as_ref(), previous_anchor.as_ref());
        true
    }

    /// Restore a cursor after rebuilding the stream, preferring stable row
    /// identity and falling back to its source anchor when a folded row went
    /// away.
    #[cfg(test)]
    pub(crate) fn restore_cursor(
        &mut self,
        key: Option<&ReviewRowKey>,
        anchor: Option<&SourceAnchor>,
    ) {
        if let Some(key) = key {
            if let Some(index) = self.rows.iter().position(|row| row.key() == key) {
                self.cursor = index;
                return;
            }
            if let ReviewRowKey::Annotation { id, .. } = key {
                if let Some(index) = self.annotation_row(id) {
                    self.cursor = index;
                    return;
                }
            }
        }
        if let Some(anchor) = anchor {
            if let Some(index) = self.row_for_source(anchor) {
                self.cursor = index;
                return;
            }
        }
        self.set_cursor(self.cursor);
    }
}

#[derive(Clone)]
struct SourceSlot {
    row_index: usize,
    old_anchor: Option<SourceAnchor>,
    new_anchor: Option<SourceAnchor>,
}

fn build_rows(files: &[ReviewFile], annotations: &[InlineAnnotation]) -> Vec<ReviewRow> {
    let mut rows = Vec::new();
    let mut slots = Vec::new();

    for review_file in files {
        let file = &review_file.file;
        let file_name = file.path().to_string_lossy().into_owned();
        rows.push(ReviewRow::FileHeader {
            key: ReviewRowKey::FileHeader {
                repo_id: review_file.repo_id.clone(),
                file: file_name.clone(),
            },
            repo_id: review_file.repo_id.clone(),
            repo_name: review_file.repo_name.clone(),
            path: file_name.clone(),
            status: file.status,
        });
        let mut visible_line = 0usize;
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            rows.push(ReviewRow::HunkHeader {
                key: ReviewRowKey::HunkHeader {
                    repo_id: review_file.repo_id.clone(),
                    file: file_name.clone(),
                    hunk: hunk_index,
                },
                repo_id: review_file.repo_id.clone(),
                file: file_name.clone(),
                hunk: hunk_index,
                text: hunk.header.clone(),
            });
            for (line_index, line) in hunk.lines.iter().enumerate() {
                let old_anchor = line.old_line.map(|source_line| {
                    SourceAnchor::new(
                        &review_file.repo_id,
                        file.path(),
                        hunk_index,
                        line_index,
                        visible_line,
                        SourceSide::Old,
                        source_line,
                    )
                });
                let new_anchor = line.new_line.map(|source_line| {
                    SourceAnchor::new(
                        &review_file.repo_id,
                        file.path(),
                        hunk_index,
                        line_index,
                        visible_line,
                        SourceSide::New,
                        source_line,
                    )
                });
                let row_index = rows.len();
                if line.kind == LineKind::Meta {
                    rows.push(ReviewRow::Fold {
                        key: ReviewRowKey::Fold {
                            repo_id: review_file.repo_id.clone(),
                            file: file_name.clone(),
                            hunk: hunk_index,
                            line: line_index,
                        },
                        repo_id: review_file.repo_id.clone(),
                        file: file_name.clone(),
                        hunk: hunk_index,
                        line: line_index,
                        hidden_lines: hidden_line_count(&line.content),
                        text: line.content.clone(),
                    });
                } else {
                    rows.push(ReviewRow::Source {
                        key: ReviewRowKey::Source {
                            repo_id: review_file.repo_id.clone(),
                            file: file_name.clone(),
                            hunk: hunk_index,
                            line: line_index,
                        },
                        repo_id: review_file.repo_id.clone(),
                        file: file_name.clone(),
                        hunk: hunk_index,
                        line: line_index,
                        kind: line.kind,
                        old_line: line.old_line,
                        new_line: line.new_line,
                        content: line.content.clone(),
                        old_anchor: old_anchor.clone(),
                        new_anchor: new_anchor.clone(),
                    });
                    slots.push(SourceSlot {
                        row_index,
                        old_anchor,
                        new_anchor,
                    });
                }
                visible_line = visible_line.saturating_add(1);
            }
        }
    }

    let mut insertions: Vec<(usize, InlineAnnotation, SourceAnchor)> = annotations
        .iter()
        .cloned()
        .filter_map(|annotation| {
            let (target, anchor) = annotation_target(&annotation, files, &slots, &rows)?;
            Some((target, annotation, anchor))
        })
        .collect();
    let anchored_annotation_ids = insertions
        .iter()
        .map(|(_, annotation, _)| annotation.id.clone())
        .collect::<HashSet<_>>();
    insertions.sort_by(|(left_target, left, _), (right_target, right, _)| {
        left_target
            .cmp(right_target)
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut rebuilt = Vec::with_capacity(rows.len() + insertions.len());
    let mut insertion_index = 0;
    for (row_index, row) in rows.into_iter().enumerate() {
        rebuilt.push(row);
        while insertion_index < insertions.len() && insertions[insertion_index].0 == row_index {
            let (_, annotation, anchor) = &insertions[insertion_index];
            append_annotation_rows(&mut rebuilt, annotation, anchor);
            insertion_index += 1;
        }
    }

    // A placement whose file vanished from the current diff remains part of
    // review history. Keep those annotations readable in deterministic
    // synthetic file sections instead of dropping them from the stream.
    let mut orphaned = annotations
        .iter()
        .filter(|annotation| !anchored_annotation_ids.contains(&annotation.id))
        .cloned()
        .collect::<Vec<_>>();
    orphaned.sort_by(|left, right| {
        left.repo_id
            .cmp(&right.repo_id)
            .then_with(|| left.file_path.cmp(&right.file_path))
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut previous_file: Option<(String, PathBuf)> = None;
    for annotation in orphaned {
        let file_key = (annotation.repo_id.clone(), annotation.file_path.clone());
        if previous_file.as_ref() != Some(&file_key) {
            let path = annotation.file_path.to_string_lossy().into_owned();
            let repo_name = files
                .iter()
                .find(|file| file.repo_id == annotation.repo_id)
                .map(|file| file.repo_name.clone())
                .unwrap_or_else(|| annotation.repo_id.clone());
            rebuilt.push(ReviewRow::FileHeader {
                key: ReviewRowKey::FileHeader {
                    repo_id: annotation.repo_id.clone(),
                    file: path.clone(),
                },
                repo_id: annotation.repo_id.clone(),
                repo_name,
                path,
                status: FileStatus::Deleted,
            });
            previous_file = Some(file_key);
        }
        let anchor = SourceAnchor {
            repo_id: annotation.repo_id.clone(),
            file: annotation.file_path.to_string_lossy().into_owned(),
            hunk: usize::MAX,
            line: usize::MAX,
            visible_line: 0,
            side: annotation.side,
            source_line: annotation.line_start,
        };
        append_annotation_rows(&mut rebuilt, &annotation, &anchor);
    }
    rebuilt
}

fn annotation_target(
    annotation: &InlineAnnotation,
    files: &[ReviewFile],
    slots: &[SourceSlot],
    rows: &[ReviewRow],
) -> Option<(usize, SourceAnchor)> {
    let path = annotation.file_path.to_string_lossy().into_owned();
    let mut matching = slots.iter().filter(|slot| {
        let anchor = match annotation.side {
            SourceSide::Old => slot.old_anchor.as_ref(),
            SourceSide::New => slot.new_anchor.as_ref(),
        };
        anchor.is_some_and(|anchor| {
            anchor.repo_id == annotation.repo_id
                && anchor.file == path
                && (annotation.line_start..=annotation.line_end).contains(&anchor.source_line)
        })
    });
    if let Some(slot) = matching.next_back() {
        let anchor = match annotation.side {
            SourceSide::Old => slot.old_anchor.as_ref(),
            SourceSide::New => slot.new_anchor.as_ref(),
        }?;
        return Some((slot.row_index, anchor.clone()));
    }

    // An outdated or ambiguous placement still gets a deterministic home:
    // use the nearest source row on that file/side, then the file header.
    let nearest = slots
        .iter()
        .filter_map(|slot| {
            let anchor = match annotation.side {
                SourceSide::Old => slot.old_anchor.as_ref(),
                SourceSide::New => slot.new_anchor.as_ref(),
            }?;
            (anchor.repo_id == annotation.repo_id && anchor.file == path).then_some((
                anchor.source_line.abs_diff(annotation.line_start),
                slot.row_index,
            ))
        })
        .min_by_key(|(distance, row)| (*distance, *row));
    if let Some((_, row)) = nearest {
        let slot = slots.iter().find(|slot| slot.row_index == row)?;
        let anchor = match annotation.side {
            SourceSide::Old => slot.old_anchor.as_ref(),
            SourceSide::New => slot.new_anchor.as_ref(),
        }?;
        return Some((row, anchor.clone()));
    }

    files
        .iter()
        .position(|file| {
            file.repo_id == annotation.repo_id
                && file.file.path().to_string_lossy().as_ref() == path
        })
        .and_then(|_| {
            let row = rows.iter().position(|row| {
                matches!(
                    row,
                    ReviewRow::FileHeader {
                        repo_id,
                        path: row_path,
                        ..
                    } if repo_id == &annotation.repo_id && row_path == &path
                )
            })?;
            Some((
                row,
                SourceAnchor {
                    repo_id: annotation.repo_id.clone(),
                    file: path.clone(),
                    hunk: usize::MAX,
                    line: usize::MAX,
                    visible_line: 0,
                    side: annotation.side,
                    source_line: annotation.line_start,
                },
            ))
        })
}

fn append_annotation_rows(
    rows: &mut Vec<ReviewRow>,
    annotation: &InlineAnnotation,
    anchor: &SourceAnchor,
) {
    let total_body_lines = logical_lines(&annotation.body).len().max(1);
    let total_rows = if annotation.collapsed {
        1
    } else {
        total_body_lines + 2 + usize::from(annotation.prompt.is_some())
    };
    for block_line in 0..total_rows {
        let (part, text) = if annotation.collapsed {
            (
                AnnotationRowPart::Header { collapsed: true },
                annotation.title.clone(),
            )
        } else if block_line == 0 {
            (
                AnnotationRowPart::Header { collapsed: false },
                annotation.title.clone(),
            )
        } else if block_line <= total_body_lines {
            let text = logical_lines(&annotation.body)
                .get(block_line - 1)
                .cloned()
                .unwrap_or_default();
            (
                AnnotationRowPart::Body {
                    line: block_line - 1,
                    total: total_body_lines,
                },
                text,
            )
        } else if annotation.prompt.is_some() && block_line == total_body_lines + 1 {
            (
                AnnotationRowPart::Prompt,
                annotation.prompt.clone().unwrap_or_default(),
            )
        } else {
            (AnnotationRowPart::Footer, String::new())
        };
        rows.push(ReviewRow::Annotation {
            key: ReviewRowKey::Annotation {
                id: annotation.id.clone(),
                block_line,
            },
            block: AnnotationRow {
                annotation_id: annotation.id.clone(),
                anchor: anchor.clone(),
                part,
                text,
            },
        });
    }
}

fn logical_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        vec![String::new()]
    } else {
        text.split('\n').map(str::to_owned).collect()
    }
}

fn hidden_line_count(text: &str) -> usize {
    let digits: String = text
        .split_whitespace()
        .take_while(|token| *token != "unchanged")
        .filter_map(|token| token.trim_matches('·').parse::<usize>().ok())
        .map(|count| count.to_string())
        .next()
        .unwrap_or_default();
    digits.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffFile, DiffLine, FileStatus, Hunk};

    fn file() -> DiffFile {
        DiffFile {
            old_path: Some(PathBuf::from("src/lib.rs")),
            new_path: Some(PathBuf::from("src/lib.rs")),
            display_path: PathBuf::from("src/lib.rs"),
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
            hunks: vec![Hunk {
                header: "@@ -4,3 +4,4 @@".into(),
                old_start: 4,
                old_count: 3,
                new_start: 4,
                new_count: 4,
                lines: vec![
                    DiffLine {
                        kind: LineKind::Context,
                        old_line: Some(4),
                        new_line: Some(4),
                        content: "before".into(),
                    },
                    DiffLine {
                        kind: LineKind::Addition,
                        old_line: None,
                        new_line: Some(5),
                        content: "added".into(),
                    },
                    DiffLine {
                        kind: LineKind::Meta,
                        old_line: None,
                        new_line: None,
                        content: "··· 42 unchanged lines ···".into(),
                    },
                    DiffLine {
                        kind: LineKind::Deletion,
                        old_line: Some(5),
                        new_line: None,
                        content: "removed".into(),
                    },
                ],
            }],
        }
    }

    fn annotation(id: &str, line: usize, body: &str) -> InlineAnnotation {
        InlineAnnotation::new(
            id,
            "src/lib.rs",
            SourceSide::New,
            line,
            line,
            format!("Comment · src/lib.rs R{line}"),
            body,
        )
    }

    #[test]
    fn emits_headers_source_rows_folds_and_expanded_blocks() {
        let stream = ReviewStream::new(&[file()], &[annotation("a", 5, "first\nsecond")]);
        assert!(matches!(stream.rows()[0], ReviewRow::FileHeader { .. }));
        assert!(matches!(stream.rows()[1], ReviewRow::HunkHeader { .. }));
        assert!(stream.rows().iter().any(|row| matches!(
            row,
            ReviewRow::Fold {
                hidden_lines: 42,
                ..
            }
        )));
        let annotation_rows = stream
            .rows()
            .iter()
            .filter(|row| row.is_annotation())
            .count();
        assert_eq!(annotation_rows, 4); // title, two body rows, footer
        assert_eq!(stream.row_count(), 10);
    }

    #[test]
    fn ask_prompt_is_part_of_the_existing_annotation_and_disappears_when_folded() {
        let mut ask = annotation("ask", 5, "first\nsecond");
        ask.prompt = Some("❯ follow up · press i or Enter".into());
        let stream = ReviewStream::new(&[file()], &[ask.clone()]);
        let rows: Vec<_> = stream
            .rows()
            .iter()
            .filter_map(|row| match row {
                ReviewRow::Annotation { block, .. } => Some(block),
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 5);
        assert!(matches!(
            rows[0].part,
            AnnotationRowPart::Header { collapsed: false }
        ));
        assert!(matches!(rows[1].part, AnnotationRowPart::Body { .. }));
        assert!(matches!(rows[2].part, AnnotationRowPart::Body { .. }));
        assert!(matches!(rows[3].part, AnnotationRowPart::Prompt));
        assert!(matches!(rows[4].part, AnnotationRowPart::Footer));
        assert!(rows.iter().all(|row| row.annotation_id == "ask"));

        ask.collapsed = true;
        let folded = ReviewStream::new(&[file()], &[ask]);
        let folded_rows: Vec<_> = folded
            .rows()
            .iter()
            .filter_map(|row| match row {
                ReviewRow::Annotation { block, .. } => Some(block),
                _ => None,
            })
            .collect();
        assert_eq!(folded_rows.len(), 1);
        assert!(matches!(
            folded_rows[0].part,
            AnnotationRowPart::Header { collapsed: true }
        ));
    }

    #[test]
    fn long_body_is_not_width_dependent_and_preserves_text_mapping() {
        let body = "a very long line that a renderer may wrap without changing this row\n最後";
        let stream = ReviewStream::new(&[file()], &[annotation("long", 5, body)]);
        let rows: Vec<_> = stream
            .rows()
            .iter()
            .filter_map(|row| match row {
                ReviewRow::Annotation { block, .. } => Some(block),
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[1].text,
            "a very long line that a renderer may wrap without changing this row"
        );
        assert_eq!(rows[2].text, "最後");
        assert!(rows.iter().all(|row| row.anchor.source_line == 5));
    }

    #[test]
    fn collapsing_is_deterministic_and_reanchors_inside_block() {
        let mut stream = ReviewStream::new(&[file()], &[annotation("a", 5, "one\ntwo")]);
        let body_row = stream
            .rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    ReviewRow::Annotation {
                        key: ReviewRowKey::Annotation { block_line: 2, .. },
                        ..
                    }
                )
            })
            .unwrap();
        stream.set_cursor(body_row);
        assert!(stream.set_annotation_collapsed("a", true));
        assert_eq!(
            stream
                .rows()
                .iter()
                .filter(|row| row.is_annotation())
                .count(),
            1
        );
        assert_eq!(stream.annotation_row("a"), Some(stream.cursor()));
        assert!(stream.set_annotation_collapsed("a", false));
        assert_eq!(
            stream
                .rows()
                .iter()
                .filter(|row| row.is_annotation())
                .count(),
            4
        );
        assert_eq!(stream.annotation_row("a"), Some(stream.cursor()));
    }

    #[test]
    fn annotations_for_missing_files_remain_readable_and_navigable() {
        let orphan = InlineAnnotation::new(
            "orphan",
            "src/removed.rs",
            SourceSide::New,
            17,
            17,
            "!Comment · src/removed.rs R17",
            "the source vanished but this review note must remain",
        );
        let mut stream = ReviewStream::new(&[file()], &[orphan]);

        assert!(stream.rows().iter().any(|row| {
            matches!(
                row,
                ReviewRow::FileHeader {
                    path,
                    status: FileStatus::Deleted,
                    ..
                } if path == "src/removed.rs"
            )
        }));
        let orphan_row = stream.annotation_row("orphan").expect("orphan annotation");
        assert!(matches!(
            &stream.rows()[orphan_row],
            ReviewRow::Annotation { block, .. }
                if block.text == "!Comment · src/removed.rs R17"
                    && block.anchor.file == "src/removed.rs"
        ));
        stream.gg();
        assert_eq!(stream.jump_annotation(true), Some(orphan_row));
    }

    #[test]
    fn movement_supports_vim_edges_pages_and_annotation_wrap() {
        let second =
            InlineAnnotation::new("b", "src/lib.rs", SourceSide::Old, 5, 5, "Ask", "reply");
        let mut stream = ReviewStream::new(&[file()], &[annotation("a", 5, "one"), second]);
        stream.gg();
        assert_eq!(stream.cursor(), 0);
        stream.page_down(3);
        assert_eq!(stream.cursor(), 3);
        stream.half_page_down(4);
        assert_eq!(stream.cursor(), 5);
        stream.half_page_up(4);
        assert_eq!(stream.cursor(), 3);
        stream.g_end();
        assert_eq!(stream.cursor(), stream.row_count() - 1);
        assert!(stream.jump_annotation(true).is_some());
        let first = stream.cursor();
        assert!(stream.jump_annotation(true).is_some());
        assert!(stream.cursor() != first);
        assert!(stream.jump_annotation(true).is_some());
        assert_eq!(stream.cursor(), first);
    }

    #[test]
    fn source_mapping_distinguishes_old_and_new_sides() {
        let stream = ReviewStream::new(&[file()], &[]);
        let old = SourceAnchor::new("", Path::new("src/lib.rs"), 0, 3, 3, SourceSide::Old, 5);
        let new = SourceAnchor::new("", Path::new("src/lib.rs"), 0, 1, 1, SourceSide::New, 5);
        assert!(stream.row_for_source(&old).is_some());
        assert!(stream.row_for_source(&new).is_some());
        assert_ne!(stream.row_for_source(&old), stream.row_for_source(&new));
    }

    #[test]
    fn equal_paths_in_different_repositories_never_share_annotations() {
        let files = vec![
            ReviewFile::new("one", "one", file()),
            ReviewFile::new("two", "two", file()),
        ];
        let annotation = annotation("only-two", 5, "body").in_repo("two");
        let stream = ReviewStream::for_files(&files, &[annotation]);
        let block = stream
            .rows()
            .iter()
            .find_map(|row| match row {
                ReviewRow::Annotation { block, .. } => Some(block),
                _ => None,
            })
            .unwrap();
        assert_eq!(block.anchor.repo_id, "two");
        assert_eq!(
            stream
                .rows()
                .iter()
                .filter(|row| row.is_annotation_header())
                .count(),
            1
        );
    }
}

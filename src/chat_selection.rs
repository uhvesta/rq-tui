//! Semantic, renderer-independent selection primitives for Chat.
//!
//! The UI owns styled terminal rows. This module owns the stable relationship
//! between those rows and source text. A [`ChatPoint`] never contains a
//! viewport row number, so it remains meaningful when wrapping or the
//! terminal width changes.

use std::cmp::Ordering;
use std::fmt;
use std::ops::Range;

/// Stable identity for one transcript message.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChatMessageId(String);

impl ChatMessageId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ChatMessageId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ChatMessageId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for ChatMessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Stable identity for a semantic Markdown/source block within a message.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlockId(pub u32);

/// A half-open UTF-8 byte range in one message.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceRange {
    pub start: usize,
    pub end: usize,
}

impl SourceRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn contains(self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

/// Source metadata supplied by the Markdown/layout layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatBlock {
    pub id: BlockId,
    pub source: SourceRange,
}

/// Text and semantic block metadata for one transcript message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatMessage {
    pub id: ChatMessageId,
    pub speaker: Option<String>,
    pub text: String,
    pub blocks: Vec<ChatBlock>,
}

impl ChatMessage {
    fn block(&self, id: BlockId) -> Option<&ChatBlock> {
        self.blocks.iter().find(|block| block.id == id)
    }
}

/// A stable semantic cursor location.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PointAffinity {
    /// Resolve a boundary to the following rendered cell/row.
    Before,
    /// Resolve a boundary to the preceding rendered cell/row.
    After,
}

/// A stable semantic cursor location.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChatPoint {
    pub message_id: ChatMessageId,
    pub block_id: BlockId,
    /// UTF-8 byte offset, normalized to a grapheme-ish boundary by a layout.
    pub byte_offset: usize,
    pub affinity: PointAffinity,
}

impl ChatPoint {
    pub fn new(
        message_id: impl Into<ChatMessageId>,
        block_id: BlockId,
        byte_offset: usize,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            block_id,
            byte_offset,
            affinity: PointAffinity::Before,
        }
    }

    pub fn with_affinity(mut self, affinity: PointAffinity) -> Self {
        self.affinity = affinity;
        self
    }
}

/// The requested visual selection semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatSelectionMode {
    Character,
    Line,
    Block,
}

/// A semantic selection with stable endpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatSelection {
    pub mode: ChatSelectionMode,
    pub anchor: ChatPoint,
    pub active: ChatPoint,
    /// Terminal columns are used only to project Block mode after reflow.
    /// The semantic endpoints remain authoritative and resize-safe.
    pub block_columns: Option<(usize, usize)>,
}

impl ChatSelection {
    pub fn character(point: ChatPoint) -> Self {
        Self {
            mode: ChatSelectionMode::Character,
            anchor: point.clone(),
            active: point,
            block_columns: None,
        }
    }

    pub fn line(layout: &ChatLayout, point: &ChatPoint) -> Option<Self> {
        let point = layout.normalize_point(point)?;
        let (start, end) = layout.row_bounds_for_point(&point)?;
        Some(Self {
            mode: ChatSelectionMode::Line,
            anchor: start,
            active: end,
            block_columns: None,
        })
    }

    pub fn block(layout: &ChatLayout, point: &ChatPoint) -> Option<Self> {
        let point = layout.normalize_point(point)?;
        let location = layout.locate(&point)?;
        Some(Self {
            mode: ChatSelectionMode::Block,
            anchor: point.clone(),
            active: point,
            block_columns: Some((location.column, location.column)),
        })
    }

    /// Extend the active endpoint while retaining the selection mode.
    pub fn extend_to(&mut self, layout: &ChatLayout, point: &ChatPoint) -> bool {
        let Some(point) = layout.normalize_point(point) else {
            return false;
        };
        if self.mode == ChatSelectionMode::Line {
            let Some((start, end)) = layout.row_bounds_for_point(&point) else {
                return false;
            };
            self.active = if layout.compare_points(&self.anchor, &point) == Ordering::Greater {
                start
            } else {
                end
            };
        } else {
            self.active = point;
        }
        if self.mode == ChatSelectionMode::Block {
            let Some(anchor_location) = layout.locate(&self.anchor) else {
                return false;
            };
            let Some(active_location) = layout.locate(&self.active) else {
                return false;
            };
            self.block_columns = Some((
                anchor_location.column.min(active_location.column),
                anchor_location.column.max(active_location.column),
            ));
        }
        true
    }

    /// Return true when a rendered cell should receive selection styling.
    pub fn contains_cell(&self, layout: &ChatLayout, row: usize, cell: usize) -> bool {
        let Some(rendered_cell) = layout.rows.get(row).and_then(|item| item.cells.get(cell)) else {
            return false;
        };
        if rendered_cell.source.is_none() {
            return false;
        }
        match self.mode {
            ChatSelectionMode::Character => {
                let Some(cell_start) = layout.point_for_cell(row, cell) else {
                    return false;
                };
                let cell_end = layout
                    .point_after_cell(row, cell)
                    .unwrap_or_else(|| cell_start.clone());
                let (low, high) = layout.ordered_points(&self.anchor, &self.active);
                let start = layout.compare_points(&cell_end, low) == Ordering::Greater;
                let end = layout.compare_points(&cell_start, high) == Ordering::Less;
                (start && end) || (low == high && cell_start == *low)
            }
            ChatSelectionMode::Line => self
                .selected_rows(layout)
                .is_some_and(|rows| rows.contains(&row)),
            ChatSelectionMode::Block => {
                let Some(rows) = self.selected_rows(layout) else {
                    return false;
                };
                let Some((low_column, high_column)) = self.block_columns else {
                    return false;
                };
                if !rows.contains(&row) {
                    return false;
                }
                let start = layout.cell_column(row, cell);
                let end = start.saturating_add(rendered_cell.width);
                start < high_column.saturating_add(1) && end > low_column
            }
        }
    }

    /// Copy selected source text according to the plain-text policy.
    pub fn copy(&self, layout: &ChatLayout, policy: CopyPolicy) -> String {
        let Some(selected) = layout.selected_rows(self) else {
            return String::new();
        };
        let mut messages = Vec::<CopiedMessage>::new();

        for row in selected.clone() {
            let Some(row_data) = layout.rows.get(row) else {
                continue;
            };
            let mut chunks = Vec::new();
            for (cell_index, cell) in row_data.cells.iter().enumerate() {
                if !self.contains_cell(layout, row, cell_index) {
                    continue;
                }
                let Some(source) = cell.source else {
                    continue;
                };
                if chunks
                    .last()
                    .is_some_and(|last: &SourceRange| last.end >= source.start)
                {
                    if let Some(last) = chunks.last_mut() {
                        last.end = last.end.max(source.end);
                    }
                } else {
                    chunks.push(source);
                }
            }
            if chunks.is_empty() {
                continue;
            }

            let message_id = row_data.message_id.clone();
            let message_index = messages
                .iter()
                .position(|entry| entry.message_id == message_id);
            let entry = if let Some(message_index) = message_index {
                &mut messages[message_index]
            } else {
                messages.push(CopiedMessage {
                    message_id: message_id.clone(),
                    chunks: Vec::new(),
                    hard_breaks: Vec::new(),
                });
                messages.last_mut().expect("message was just pushed")
            };
            entry.chunks.extend(chunks);
            if row_data.break_after == RowBreak::Hard
                && row.saturating_add(1) < selected.end
                && layout
                    .rows
                    .get(row.saturating_add(1))
                    .is_some_and(|next_row| next_row.message_id == message_id)
            {
                entry.hard_breaks.push(entry.chunks.len());
            }
        }

        let mut output = String::new();
        let crosses_messages = messages.len() > 1;
        for (message_index, message) in messages.iter().enumerate() {
            if message_index > 0 {
                output.push_str("\n\n");
            }
            if policy.include_speaker_labels && crosses_messages {
                if let Some(speaker) = layout
                    .messages
                    .iter()
                    .find(|candidate| candidate.id == message.message_id)
                    .and_then(|candidate| candidate.speaker.as_deref())
                {
                    output.push_str(speaker);
                    output.push_str(": ");
                }
            }
            let Some(source_message) = layout.message(&message.message_id) else {
                continue;
            };
            for (chunk_index, source_range) in message.chunks.iter().enumerate() {
                if chunk_index > 0 && message.hard_breaks.contains(&chunk_index) {
                    output.push('\n');
                }
                let start = source_range.start.min(source_message.text.len());
                let end = source_range.end.min(source_message.text.len());
                if start <= end
                    && source_message.text.is_char_boundary(start)
                    && source_message.text.is_char_boundary(end)
                {
                    output.push_str(&source_message.text[start..end]);
                }
            }
        }
        if self.mode == ChatSelectionMode::Line && !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output
    }

    fn selected_rows(&self, layout: &ChatLayout) -> Option<Range<usize>> {
        layout.selected_rows(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopyPolicy {
    pub include_speaker_labels: bool,
}

impl Default for CopyPolicy {
    fn default() -> Self {
        Self {
            include_speaker_labels: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowBreak {
    Soft,
    Hard,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChatCell {
    pub source: Option<SourceRange>,
    pub width: usize,
}

impl ChatCell {
    pub fn source(source: SourceRange, width: usize) -> Self {
        Self {
            source: Some(source),
            width: width.max(1),
        }
    }

    pub fn display_only(width: usize) -> Self {
        Self {
            source: None,
            width: width.max(1),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRow {
    pub message_id: ChatMessageId,
    pub block_id: BlockId,
    pub cells: Vec<ChatCell>,
    pub break_after: RowBreak,
}

impl ChatRow {
    pub fn new(
        message_id: impl Into<ChatMessageId>,
        block_id: BlockId,
        cells: Vec<ChatCell>,
        break_after: RowBreak,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            block_id,
            cells,
            break_after,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorLocation {
    pub row: usize,
    pub column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Movement {
    Left,
    Right,
    Up,
    Down,
    #[allow(dead_code)]
    PageUp(usize),
    #[allow(dead_code)]
    PageDown(usize),
    Home,
    End,
    WordForward,
    WordBackward,
    Top,
    Bottom,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatCursor {
    pub point: ChatPoint,
    pub preferred_column: Option<usize>,
}

impl ChatCursor {
    pub fn new(point: ChatPoint) -> Self {
        Self {
            point,
            preferred_column: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    UnknownMessage(ChatMessageId),
    UnknownBlock {
        message_id: ChatMessageId,
        block_id: BlockId,
    },
    InvalidBlockRange {
        message_id: ChatMessageId,
        range: SourceRange,
    },
    InvalidCellRange {
        message_id: ChatMessageId,
        range: SourceRange,
    },
    CellRangeNotInRow {
        message_id: ChatMessageId,
        block_id: BlockId,
        range: SourceRange,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMessage(id) => write!(formatter, "unknown chat message {id}"),
            Self::UnknownBlock {
                message_id,
                block_id,
            } => {
                write!(
                    formatter,
                    "unknown block {:?} in chat message {message_id}",
                    block_id
                )
            }
            Self::InvalidBlockRange { message_id, range } => {
                write!(
                    formatter,
                    "invalid block range {range:?} in chat message {message_id}"
                )
            }
            Self::InvalidCellRange { message_id, range } => {
                write!(
                    formatter,
                    "invalid cell range {range:?} in chat message {message_id}"
                )
            }
            Self::CellRangeNotInRow {
                message_id,
                block_id,
                range,
            } => write!(
                formatter,
                "cell range {range:?} is not in message {message_id} block {:?}",
                block_id
            ),
        }
    }
}

/// A width-specific, ephemeral index from rendered cells to source spans.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatLayout {
    pub width: usize,
    pub messages: Vec<ChatMessage>,
    pub rows: Vec<ChatRow>,
}

impl ChatLayout {
    pub fn new(
        width: usize,
        messages: Vec<ChatMessage>,
        rows: Vec<ChatRow>,
    ) -> Result<Self, LayoutError> {
        let layout = Self {
            width: width.max(1),
            messages,
            rows,
        };
        layout.validate()?;
        Ok(layout)
    }

    pub fn message(&self, id: &ChatMessageId) -> Option<&ChatMessage> {
        self.messages.iter().find(|message| &message.id == id)
    }

    pub fn normalize_point(&self, point: &ChatPoint) -> Option<ChatPoint> {
        let message = self.message(&point.message_id)?;
        let block = message.block(point.block_id)?;
        let offset = normalize_boundary(&message.text, point.byte_offset)
            .max(block.source.start)
            .min(block.source.end);
        Some(ChatPoint {
            message_id: point.message_id.clone(),
            block_id: point.block_id,
            byte_offset: normalize_boundary(&message.text, offset),
            affinity: point.affinity,
        })
    }

    pub fn locate(&self, point: &ChatPoint) -> Option<CursorLocation> {
        let point = self.normalize_point(point)?;
        let mut boundary_fallback = None;
        for (row_index, row) in self.rows.iter().enumerate() {
            if row.message_id != point.message_id || row.block_id != point.block_id {
                continue;
            }
            let mut column = 0usize;
            let mut last_source_end = None;
            let mut last_source_column = None;
            for cell in &row.cells {
                if let Some(source) = cell.source {
                    last_source_end = Some(source.end);
                    last_source_column = Some(column.saturating_add(cell.width));
                    if point.byte_offset == source.end && point.affinity == PointAffinity::After {
                        return Some(CursorLocation {
                            row: row_index,
                            column: column.saturating_add(cell.width),
                        });
                    }
                    if point.byte_offset <= source.start {
                        return Some(CursorLocation {
                            row: row_index,
                            column,
                        });
                    }
                    if source.contains(point.byte_offset) {
                        return Some(CursorLocation {
                            row: row_index,
                            column,
                        });
                    }
                }
                column = column.saturating_add(cell.width);
            }
            if last_source_end == Some(point.byte_offset) {
                let candidate = CursorLocation {
                    row: row_index,
                    column: last_source_column.unwrap_or(column),
                };
                if point.affinity == PointAffinity::After {
                    return Some(candidate);
                }
                boundary_fallback = Some(candidate);
            }
        }
        boundary_fallback
    }

    pub fn row_bounds(&self, row: usize) -> Option<(ChatPoint, ChatPoint)> {
        let rendered = self.rows.get(row)?;
        let first = rendered.cells.iter().find_map(|cell| cell.source)?;
        let last = rendered.cells.iter().rev().find_map(|cell| cell.source)?;
        Some((
            ChatPoint::new(rendered.message_id.clone(), rendered.block_id, first.start)
                .with_affinity(PointAffinity::Before),
            ChatPoint::new(rendered.message_id.clone(), rendered.block_id, last.end)
                .with_affinity(PointAffinity::After),
        ))
    }

    pub fn row_bounds_for_point(&self, point: &ChatPoint) -> Option<(ChatPoint, ChatPoint)> {
        let location = self.locate(point)?;
        self.row_bounds(location.row)
    }

    pub fn point_for_cell(&self, row: usize, cell: usize) -> Option<ChatPoint> {
        let rendered = self.rows.get(row)?;
        let source = rendered.cells.get(cell)?.source?;
        Some(
            ChatPoint::new(rendered.message_id.clone(), rendered.block_id, source.start)
                .with_affinity(PointAffinity::Before),
        )
    }

    pub fn point_after_cell(&self, row: usize, cell: usize) -> Option<ChatPoint> {
        let rendered = self.rows.get(row)?;
        let source = rendered.cells.get(cell)?.source?;
        Some(
            ChatPoint::new(rendered.message_id.clone(), rendered.block_id, source.end)
                .with_affinity(PointAffinity::After),
        )
    }

    pub fn cell_column(&self, row: usize, cell: usize) -> usize {
        self.rows
            .get(row)
            .map(|rendered| {
                rendered
                    .cells
                    .iter()
                    .take(cell)
                    .map(|item| item.width)
                    .sum()
            })
            .unwrap_or(0)
    }

    pub fn compare_points(&self, left: &ChatPoint, right: &ChatPoint) -> Ordering {
        let left_location = self.locate(left);
        let right_location = self.locate(right);
        match (left_location, right_location) {
            (Some(left), Some(right)) => (left.row, left.column).cmp(&(right.row, right.column)),
            _ => Ordering::Equal,
        }
    }

    pub fn ordered_points<'a>(
        &'a self,
        left: &'a ChatPoint,
        right: &'a ChatPoint,
    ) -> (&'a ChatPoint, &'a ChatPoint) {
        if self.compare_points(left, right) == Ordering::Greater {
            (right, left)
        } else {
            (left, right)
        }
    }

    pub fn navigate(&self, cursor: &ChatCursor, movement: Movement) -> ChatCursor {
        let mut result = cursor.clone();
        match movement {
            Movement::Left => {
                result.point = self.left(&cursor.point);
                result.preferred_column = None;
            }
            Movement::Right => {
                result.point = self.right(&cursor.point);
                result.preferred_column = None;
            }
            Movement::Up => self.vertical(&mut result, -1),
            Movement::Down => self.vertical(&mut result, 1),
            Movement::PageUp(rows) => {
                for _ in 0..rows {
                    self.vertical(&mut result, -1);
                }
            }
            Movement::PageDown(rows) => {
                for _ in 0..rows {
                    self.vertical(&mut result, 1);
                }
            }
            Movement::Home => {
                if let Some(location) = self.locate(&cursor.point) {
                    result.point = self
                        .row_bounds(location.row)
                        .map(|bounds| bounds.0)
                        .unwrap_or_else(|| cursor.point.clone());
                }
                result.preferred_column = None;
            }
            Movement::End => {
                if let Some(location) = self.locate(&cursor.point) {
                    result.point = self
                        .row_bounds(location.row)
                        .map(|bounds| bounds.1)
                        .unwrap_or_else(|| cursor.point.clone());
                }
                result.preferred_column = None;
            }
            Movement::WordForward => {
                result.point = self.word_forward(&cursor.point);
                result.preferred_column = None;
            }
            Movement::WordBackward => {
                result.point = self.word_backward(&cursor.point);
                result.preferred_column = None;
            }
            Movement::Top => {
                result.point = self.first_point().unwrap_or_else(|| cursor.point.clone());
                result.preferred_column = None;
            }
            Movement::Bottom => {
                result.point = self.last_point().unwrap_or_else(|| cursor.point.clone());
                result.preferred_column = None;
            }
        }
        result
    }

    pub fn first_point(&self) -> Option<ChatPoint> {
        self.rows
            .iter()
            .enumerate()
            .find_map(|(row, _)| self.row_bounds(row).map(|bounds| bounds.0))
    }

    pub fn last_point(&self) -> Option<ChatPoint> {
        self.rows
            .iter()
            .enumerate()
            .rev()
            .find_map(|(row, _)| self.row_bounds(row).map(|bounds| bounds.1))
    }

    fn left(&self, point: &ChatPoint) -> ChatPoint {
        let Some(location) = self.locate(point) else {
            return point.clone();
        };
        if let Some(candidate) = self.point_before_column(location.row, location.column) {
            return candidate;
        }
        self.previous_point(location.row)
            .unwrap_or_else(|| point.clone())
    }

    fn right(&self, point: &ChatPoint) -> ChatPoint {
        let Some(location) = self.locate(point) else {
            return point.clone();
        };
        if let Some(candidate) = self.point_after_column(location.row, location.column) {
            return candidate;
        }
        self.next_point(location.row)
            .unwrap_or_else(|| point.clone())
    }

    fn vertical(&self, cursor: &mut ChatCursor, direction: i8) {
        let Some(location) = self.locate(&cursor.point) else {
            return;
        };
        let preferred = cursor.preferred_column.unwrap_or(location.column);
        let mut row = location.row as isize + direction as isize;
        while row >= 0 && (row as usize) < self.rows.len() {
            if let Some(point) = self.point_for_column(row as usize, preferred) {
                cursor.point = point;
                cursor.preferred_column = Some(preferred);
                return;
            }
            row += direction as isize;
        }
    }

    fn point_for_column(&self, row: usize, target: usize) -> Option<ChatPoint> {
        let rendered = self.rows.get(row)?;
        let mut column = 0usize;
        for (index, cell) in rendered.cells.iter().enumerate() {
            let end = column.saturating_add(cell.width);
            if cell.source.is_some() && target < end {
                return self.point_for_cell(row, index);
            }
            column = end;
        }
        rendered
            .cells
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, cell)| cell.source.map(|_| index))
            .and_then(|index| self.point_for_cell(row, index))
    }

    fn point_before_column(&self, row: usize, column: usize) -> Option<ChatPoint> {
        let rendered = self.rows.get(row)?;
        let mut previous = None;
        let mut current = 0usize;
        for (index, cell) in rendered.cells.iter().enumerate() {
            if current >= column {
                break;
            }
            if cell.source.is_some() {
                previous = self.point_for_cell(row, index);
            }
            current = current.saturating_add(cell.width);
        }
        previous
    }

    fn point_after_column(&self, row: usize, column: usize) -> Option<ChatPoint> {
        let rendered = self.rows.get(row)?;
        let mut current = 0usize;
        for (index, cell) in rendered.cells.iter().enumerate() {
            let end = current.saturating_add(cell.width);
            if end > column && cell.source.is_some() {
                return self.point_after_cell(row, index);
            }
            current = end;
        }
        None
    }

    fn previous_point(&self, row: usize) -> Option<ChatPoint> {
        (0..row)
            .rev()
            .find_map(|candidate| self.row_bounds(candidate).map(|bounds| bounds.1))
    }

    fn next_point(&self, row: usize) -> Option<ChatPoint> {
        ((row + 1)..self.rows.len())
            .find_map(|candidate| self.row_bounds(candidate).map(|bounds| bounds.0))
    }

    fn word_forward(&self, point: &ChatPoint) -> ChatPoint {
        let Some(message) = self.message(&point.message_id) else {
            return point.clone();
        };
        let mut offset = normalize_boundary(&message.text, point.byte_offset);
        if offset >= message.text.len() {
            return self
                .next_message_point(point)
                .unwrap_or_else(|| point.clone());
        }

        let mut consumed_word = false;
        while offset < message.text.len() {
            let character = message.text[offset..].chars().next().unwrap_or('\0');
            if !is_word_char(character) {
                if consumed_word {
                    break;
                }
                offset += character.len_utf8();
                continue;
            }
            consumed_word = true;
            offset += character.len_utf8();
        }
        while offset < message.text.len() {
            let character = message.text[offset..].chars().next().unwrap_or('\0');
            if is_word_char(character) {
                break;
            }
            offset += character.len_utf8();
        }
        let point = ChatPoint::new(point.message_id.clone(), point.block_id, offset);
        if let Some(normalized) = self.normalize_point(&point) {
            normalized
        } else {
            self.next_message_point(&point).unwrap_or(point)
        }
    }

    fn word_backward(&self, point: &ChatPoint) -> ChatPoint {
        let Some(message) = self.message(&point.message_id) else {
            return point.clone();
        };
        let mut offset = normalize_boundary(&message.text, point.byte_offset);
        if offset == 0 {
            return self
                .previous_message_point(point)
                .unwrap_or_else(|| point.clone());
        }
        while offset > 0 {
            let previous = previous_boundary(&message.text, offset);
            let character = message.text[previous..offset]
                .chars()
                .next()
                .unwrap_or('\0');
            if is_word_char(character) {
                break;
            }
            offset = previous;
        }
        while offset > 0 {
            let previous = previous_boundary(&message.text, offset);
            let character = message.text[previous..offset]
                .chars()
                .next()
                .unwrap_or('\0');
            if !is_word_char(character) {
                break;
            }
            offset = previous;
        }
        self.normalize_point(&ChatPoint::new(
            point.message_id.clone(),
            point.block_id,
            offset,
        ))
        .unwrap_or_else(|| point.clone())
    }

    fn next_message_point(&self, point: &ChatPoint) -> Option<ChatPoint> {
        let row = self.locate(point)?.row;
        self.next_point(row)
    }

    fn previous_message_point(&self, point: &ChatPoint) -> Option<ChatPoint> {
        let row = self.locate(point)?.row;
        self.previous_point(row)
    }

    fn selected_rows(&self, selection: &ChatSelection) -> Option<Range<usize>> {
        let anchor = self.locate(&selection.anchor)?;
        let active = self.locate(&selection.active)?;
        let start = anchor.row.min(active.row);
        let end = anchor.row.max(active.row).saturating_add(1);
        Some(start..end)
    }

    fn validate(&self) -> Result<(), LayoutError> {
        for message in &self.messages {
            let mut previous_end = 0;
            for block in &message.blocks {
                if block.source.start > block.source.end
                    || block.source.end > message.text.len()
                    || !message.text.is_char_boundary(block.source.start)
                    || !message.text.is_char_boundary(block.source.end)
                    || block.source.start < previous_end
                {
                    return Err(LayoutError::InvalidBlockRange {
                        message_id: message.id.clone(),
                        range: block.source,
                    });
                }
                previous_end = block.source.end;
            }
        }
        for row in &self.rows {
            let Some(message) = self.message(&row.message_id) else {
                return Err(LayoutError::UnknownMessage(row.message_id.clone()));
            };
            let Some(block) = message.block(row.block_id) else {
                return Err(LayoutError::UnknownBlock {
                    message_id: row.message_id.clone(),
                    block_id: row.block_id,
                });
            };
            for cell in &row.cells {
                let Some(source) = cell.source else {
                    continue;
                };
                if source.start > source.end
                    || source.end > message.text.len()
                    || !message.text.is_char_boundary(source.start)
                    || !message.text.is_char_boundary(source.end)
                {
                    return Err(LayoutError::InvalidCellRange {
                        message_id: row.message_id.clone(),
                        range: source,
                    });
                }
                if source.start < block.source.start || source.end > block.source.end {
                    return Err(LayoutError::CellRangeNotInRow {
                        message_id: row.message_id.clone(),
                        block_id: row.block_id,
                        range: source,
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct CopiedMessage {
    message_id: ChatMessageId,
    chunks: Vec<SourceRange>,
    hard_breaks: Vec<usize>,
}

fn is_word_char(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn normalize_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let ranges = grapheme_ranges(text);
    ranges
        .iter()
        .find(|range| range.start < offset && offset < range.end)
        .map_or(offset, |range| range.start)
}

fn previous_boundary(text: &str, offset: usize) -> usize {
    let offset = normalize_boundary(text, offset);
    grapheme_ranges(text)
        .into_iter()
        .rev()
        .find(|range| range.end <= offset)
        .map_or(0, |range| range.start)
}

fn grapheme_ranges(text: &str) -> Vec<Range<usize>> {
    let starts = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index + 1 < starts.len() {
        let start = starts[index];
        let mut end_index = index + 1;
        let first = text[start..].chars().next().unwrap_or('\0');
        let mut regional_count = if is_regional_indicator(first) { 1 } else { 0 };
        while end_index < starts.len() - 1 {
            let candidate_start = starts[end_index];
            let candidate = text[candidate_start..].chars().next().unwrap_or('\0');
            if is_grapheme_extend(candidate) {
                end_index += 1;
                continue;
            }
            if candidate == '\u{200d}' {
                end_index += 1;
                if end_index < starts.len() - 1 {
                    end_index += 1;
                }
                continue;
            }
            if regional_count == 1 && is_regional_indicator(candidate) {
                regional_count += 1;
                end_index += 1;
                continue;
            }
            break;
        }
        ranges.push(start..starts[end_index]);
        index = end_index;
    }
    ranges
}

fn is_regional_indicator(character: char) -> bool {
    matches!(character as u32, 0x1f1e6..=0x1f1ff)
}

fn is_grapheme_extend(character: char) -> bool {
    matches!(
        character as u32,
        0x0300..=0x036f
            | 0x0483..=0x0489
            | 0x0591..=0x05bd
            | 0x05bf..=0x05c7
            | 0x0610..=0x061a
            | 0x064b..=0x065f
            | 0x0670..=0x0670
            | 0x06d6..=0x06dc
            | 0x06df..=0x06e4
            | 0x06e7..=0x06e8
            | 0x06ea..=0x06ed
            | 0x0711..=0x0711
            | 0x0730..=0x074a
            | 0x07a6..=0x07b0
            | 0x07eb..=0x07f3
            | 0x0816..=0x0819
            | 0x081b..=0x0823
            | 0x0825..=0x0827
            | 0x0829..=0x082d
            | 0x0859..=0x085b
            | 0x08d3..=0x0903
            | 0x093a..=0x093c
            | 0x093e..=0x094f
            | 0x0951..=0x0957
            | 0x0962..=0x0963
            | 0x0981..=0x0983
            | 0x09bc..=0x09bc
            | 0x09be..=0x09cd
            | 0x09d7..=0x09d7
            | 0x09e2..=0x09e3
            | 0x0a01..=0x0a03
            | 0x0a3c..=0x0a3c
            | 0x0a3e..=0x0a42
            | 0x0a47..=0x0a48
            | 0x0a4b..=0x0a4d
            | 0x0a51..=0x0a51
            | 0x0a70..=0x0a71
            | 0x0a75..=0x0a75
            | 0x0abc..=0x0abc
            | 0x0abe..=0x0acc
            | 0x0b01..=0x0b03
            | 0x0b3c..=0x0b3c
            | 0x0b3e..=0x0b57
            | 0x0b62..=0x0b63
            | 0x0b82..=0x0b82
            | 0x0bbe..=0x0bcd
            | 0x0bd7..=0x0bd7
            | 0x0c00..=0x0c04
            | 0x0c3e..=0x0c56
            | 0x0c62..=0x0c63
            | 0x0c81..=0x0c83
            | 0x0cbc..=0x0cbc
            | 0x0cbe..=0x0cdc
            | 0x0ce2..=0x0ce3
            | 0x0d00..=0x0d03
            | 0x0d3b..=0x0d3c
            | 0x0d3e..=0x0d57
            | 0x0d62..=0x0d63
            | 0x0dca..=0x0dcf
            | 0x0dd2..=0x0dd4
            | 0x0dd6..=0x0dd6
            | 0x0dd8..=0x0ddf
            | 0x0df2..=0x0df3
            | 0x0e31..=0x0e31
            | 0x0e34..=0x0e3a
            | 0x0e47..=0x0e4e
            | 0x0eb1..=0x0eb1
            | 0x0eb4..=0x0ebc
            | 0x0ec8..=0x0ecd
            | 0x0f18..=0x0f19
            | 0x0f35..=0x0f35
            | 0x0f37..=0x0f37
            | 0x0f39..=0x0f39
            | 0x0f71..=0x0f84
            | 0x0f86..=0x0f87
            | 0x0f8d..=0x0f97
            | 0x0f99..=0x0fbc
            | 0x0fc6..=0x0fc6
            | 0x102d..=0x1030
            | 0x1032..=0x1037
            | 0x1039..=0x103a
            | 0x103d..=0x103e
            | 0x1058..=0x1059
            | 0x105e..=0x1060
            | 0x1071..=0x1074
            | 0x1082..=0x1082
            | 0x1085..=0x1086
            | 0x108d..=0x108d
            | 0x109d..=0x109d
            | 0x135d..=0x135f
            | 0x1712..=0x1714
            | 0x1732..=0x1734
            | 0x1752..=0x1753
            | 0x1772..=0x1773
            | 0x17b4..=0x17d3
            | 0x17dd..=0x17dd
            | 0x180b..=0x180f
            | 0x1885..=0x1886
            | 0x18a9..=0x18a9
            | 0x1920..=0x193b
            | 0x1a17..=0x1a1b
            | 0x1a55..=0x1a7f
            | 0x1ab0..=0x1ace
            | 0x1b00..=0x1b04
            | 0x1b34..=0x1b44
            | 0x1b6b..=0x1b73
            | 0x1b80..=0x1b82
            | 0x1ba1..=0x1bad
            | 0x1be6..=0x1bf3
            | 0x1c24..=0x1c37
            | 0x1cd0..=0x1cf9
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0x2cef..=0x2cf1
            | 0x2de0..=0x2dff
            | 0xa66f..=0xa67f
            | 0xa69e..=0xa69f
            | 0xa6f0..=0xa6f1
            | 0xa802..=0xa802
            | 0xa806..=0xa806
            | 0xa80b..=0xa80b
            | 0xa823..=0xa827
            | 0xa880..=0xa881
            | 0xa8b4..=0xa8c5
            | 0xa8e0..=0xa8f1
            | 0xa926..=0xa92f
            | 0xa947..=0xa953
            | 0xa980..=0xa983
            | 0xa9b3..=0xa9c0
            | 0xa9e5..=0xa9e5
            | 0xaa29..=0xaa3f
            | 0xaa43..=0xaa43
            | 0xaa4c..=0xaa4d
            | 0xaa7b..=0xaa7d
            | 0xaab0..=0xaab0
            | 0xaab2..=0xaab4
            | 0xaab7..=0xaab8
            | 0xaabe..=0xaabf
            | 0xaac1..=0xaac1
            | 0xaaec..=0xaaef
            | 0xaaf5..=0xaaf5
            | 0xabe3..=0xabea
            | 0xabec..=0xabed
            | 0xfb1e..=0xfb1e
            | 0xfe00..=0xfe0f
            | 0xfe20..=0xfe2f
            | 0xff9e..=0xff9f
            | 0x1f3fb..=0x1f3ff
            | 0xe0100..=0xe01ef
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, speaker: &str, text: &str) -> ChatMessage {
        ChatMessage {
            id: id.into(),
            speaker: Some(speaker.into()),
            text: text.into(),
            blocks: vec![ChatBlock {
                id: BlockId(0),
                source: SourceRange::new(0, text.len()),
            }],
        }
    }

    fn row(id: &str, start: usize, end: usize, break_after: RowBreak) -> ChatRow {
        let cells = (start..end)
            .map(|offset| ChatCell::source(SourceRange::new(offset, offset + 1), 1))
            .collect();
        ChatRow::new(id, BlockId(0), cells, break_after)
    }

    fn two_row_layout() -> ChatLayout {
        ChatLayout::new(
            5,
            vec![message("m1", "you", "hello world")],
            vec![
                row("m1", 0, 5, RowBreak::Soft),
                row("m1", 5, 10, RowBreak::Hard),
                row("m1", 10, 11, RowBreak::End),
            ],
        )
        .unwrap()
    }

    #[test]
    fn points_normalize_to_utf8_and_graphemeish_boundaries() {
        let text = "a e\u{301} 👩‍💻 b";
        let layout = ChatLayout::new(
            20,
            vec![message("m", "you", text)],
            vec![ChatRow::new(
                "m",
                BlockId(0),
                vec![ChatCell::source(SourceRange::new(0, text.len()), 1)],
                RowBreak::End,
            )],
        )
        .unwrap();
        let point = ChatPoint::new("m", BlockId(0), 4);
        let normalized = layout.normalize_point(&point).unwrap();
        assert!(text.is_char_boundary(normalized.byte_offset));
        assert_eq!(&text[normalized.byte_offset..], "e\u{301} 👩‍💻 b");
    }

    #[test]
    fn character_navigation_crosses_rows_and_preserves_semantic_offsets() {
        let layout = two_row_layout();
        let start = ChatPoint::new("m1", BlockId(0), 4);
        let right = layout.navigate(&ChatCursor::new(start), Movement::Right);
        assert_eq!(right.point.byte_offset, 5);
        let left = layout.navigate(&right, Movement::Left);
        assert_eq!(left.point.byte_offset, 4);
    }

    #[test]
    fn vertical_navigation_retains_preferred_column() {
        let layout = two_row_layout();
        let cursor = ChatCursor::new(ChatPoint::new("m1", BlockId(0), 4));
        let down = layout.navigate(&cursor, Movement::Down);
        assert_eq!(down.point.byte_offset, 9);
        assert_eq!(down.preferred_column, Some(4));
        let up = layout.navigate(&down, Movement::Up);
        assert_eq!(up.point.byte_offset, 4);
        assert_eq!(up.preferred_column, Some(4));
    }

    #[test]
    fn row_home_end_page_and_document_bounds_are_available() {
        let layout = two_row_layout();
        let cursor = ChatCursor::new(ChatPoint::new("m1", BlockId(0), 7));
        assert_eq!(
            layout.navigate(&cursor, Movement::Home).point.byte_offset,
            5
        );
        assert_eq!(
            layout.navigate(&cursor, Movement::End).point.byte_offset,
            10
        );
        assert_eq!(layout.navigate(&cursor, Movement::Top).point.byte_offset, 0);
        assert_eq!(
            layout.navigate(&cursor, Movement::Bottom).point.byte_offset,
            11
        );
        assert_eq!(
            layout
                .navigate(&cursor, Movement::PageUp(1))
                .point
                .byte_offset,
            2
        );
        assert_eq!(
            layout
                .navigate(&cursor, Movement::PageDown(1))
                .point
                .byte_offset,
            10
        );
    }

    #[test]
    fn word_navigation_uses_source_text_not_wrapped_rows() {
        let text = "one, two_three! 你好";
        let end = text.len();
        let layout = ChatLayout::new(
            4,
            vec![message("m", "you", text)],
            vec![ChatRow::new(
                "m",
                BlockId(0),
                vec![ChatCell::source(SourceRange::new(0, end), 1)],
                RowBreak::End,
            )],
        )
        .unwrap();
        let first = ChatPoint::new("m", BlockId(0), 0);
        let second = layout.navigate(&ChatCursor::new(first), Movement::WordForward);
        assert_eq!(&text[second.point.byte_offset..], "two_three! 你好");
        let previous = layout.navigate(&second, Movement::WordBackward);
        assert_eq!(previous.point.byte_offset, 0);
    }

    #[test]
    fn line_selection_expands_to_the_current_rendered_row() {
        let layout = two_row_layout();
        let selection = ChatSelection::line(&layout, &ChatPoint::new("m1", BlockId(0), 6)).unwrap();
        assert_eq!(selection.mode, ChatSelectionMode::Line);
        assert_eq!(selection.anchor.byte_offset, 5);
        assert_eq!(selection.active.byte_offset, 10);
        assert!(selection.contains_cell(&layout, 1, 0));
        assert!(!selection.contains_cell(&layout, 0, 0));
    }

    #[test]
    fn character_selection_highlights_only_the_source_interval() {
        let layout = two_row_layout();
        let mut selection = ChatSelection::character(ChatPoint::new("m1", BlockId(0), 1));
        assert!(selection.extend_to(&layout, &ChatPoint::new("m1", BlockId(0), 7)));
        assert!(!selection.contains_cell(&layout, 0, 0));
        assert!(selection.contains_cell(&layout, 0, 1));
        assert!(selection.contains_cell(&layout, 1, 1));
        assert!(!selection.contains_cell(&layout, 1, 3));
    }

    #[test]
    fn block_selection_uses_columns_and_survives_layout_reconstruction() {
        let layout = two_row_layout();
        let mut selection =
            ChatSelection::block(&layout, &ChatPoint::new("m1", BlockId(0), 1)).unwrap();
        assert!(selection.extend_to(&layout, &ChatPoint::new("m1", BlockId(0), 7)));
        assert_eq!(selection.block_columns, Some((1, 2)));
        assert!(selection.contains_cell(&layout, 0, 1));
        assert!(selection.contains_cell(&layout, 1, 1));

        let rebuilt = ChatLayout::new(
            10,
            vec![message("m1", "you", "hello world")],
            vec![
                row("m1", 0, 10, RowBreak::Soft),
                row("m1", 10, 11, RowBreak::End),
            ],
        )
        .unwrap();
        assert!(rebuilt.normalize_point(&selection.anchor).is_some());
        assert!(rebuilt.normalize_point(&selection.active).is_some());
    }

    #[test]
    fn copy_omits_display_only_cells_and_soft_wrap_newlines() {
        let text = "hello world";
        let layout = ChatLayout::new(
            5,
            vec![message("m", "you", text)],
            vec![
                ChatRow::new(
                    "m",
                    BlockId(0),
                    vec![
                        ChatCell::display_only(2),
                        ChatCell::source(SourceRange::new(0, 5), 5),
                    ],
                    RowBreak::Soft,
                ),
                row("m", 5, 11, RowBreak::End),
            ],
        )
        .unwrap();
        let mut selection = ChatSelection::character(ChatPoint::new("m", BlockId(0), 0));
        selection.extend_to(&layout, &ChatPoint::new("m", BlockId(0), 11));
        assert_eq!(selection.copy(&layout, CopyPolicy::default()), text);
    }

    #[test]
    fn copy_preserves_explicit_breaks_and_cross_message_policy() {
        let first = message("m1", "you", "hello\nthere");
        let second = message("m2", "copilot", "answer");
        let layout = ChatLayout::new(
            20,
            vec![first, second],
            vec![
                row("m1", 0, 5, RowBreak::Hard),
                row("m1", 6, 11, RowBreak::End),
                row("m2", 0, 6, RowBreak::End),
            ],
        )
        .unwrap();
        let mut selection = ChatSelection::character(ChatPoint::new("m1", BlockId(0), 0));
        selection.extend_to(&layout, &ChatPoint::new("m2", BlockId(0), 6));
        assert_eq!(
            selection.copy(&layout, CopyPolicy::default()),
            "you: hello\nthere\n\ncopilot: answer"
        );
        assert_eq!(
            selection.copy(
                &layout,
                CopyPolicy {
                    include_speaker_labels: false,
                }
            ),
            "hello\nthere\n\nanswer"
        );
    }

    #[test]
    fn line_copy_does_not_invent_soft_wrap_newlines() {
        let layout = two_row_layout();
        let selection = ChatSelection::line(&layout, &ChatPoint::new("m1", BlockId(0), 2)).unwrap();
        assert_eq!(
            selection.copy(
                &layout,
                CopyPolicy {
                    include_speaker_labels: false
                }
            ),
            "hello\n"
        );
    }

    #[test]
    fn invalid_layout_ranges_are_rejected() {
        let result = ChatLayout::new(
            10,
            vec![message("m", "you", "abc")],
            vec![ChatRow::new(
                "m",
                BlockId(0),
                vec![ChatCell::source(SourceRange::new(0, 4), 4)],
                RowBreak::End,
            )],
        );
        assert!(matches!(result, Err(LayoutError::InvalidCellRange { .. })));
    }
}

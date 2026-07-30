use std::ops::Range;
use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::highlight::{Highlighter, StyledSegment};
use crate::markdown::{inline_link, safe_link_destination};

const DEFAULT_WIDTH: usize = 1;
const TAB_WIDTH: usize = 4;

/// A stable UTF-8 byte range into the original chat message.
///
/// Display rows are deliberately ephemeral because they change on resize;
/// callers should retain source ranges when they need to preserve a selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl SourceRange {
    fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    fn join(&self, other: &Self) -> Option<Self> {
        (self.end == other.start).then(|| Self::new(self.start, other.end))
    }
}

/// Why a terminal cell was rendered. Decorations have a source range even
/// when the displayed glyph was normalized (for example, `-` becomes `•`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CellSource {
    Text(SourceRange),
    Decoration(SourceRange),
    Synthetic,
}

/// A contiguous terminal-cell interval with its message-source provenance.
/// A double-width glyph occupies a two-cell interval that maps to one source
/// range, so a pointer hit on either cell resolves to the same text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MappedCell {
    pub(crate) columns: Range<usize>,
    pub(crate) source: CellSource,
}

/// One terminal row plus semantic cell mappings.
#[derive(Clone, Debug)]
pub(crate) struct MappedRow {
    pub(crate) line: Line<'static>,
    /// Consumed by the future chat hit-testing/selection layer. `render_markdown`
    /// intentionally projects only `line` to preserve its existing API.
    #[allow(dead_code)]
    pub(crate) cells: Vec<MappedCell>,
}

/// Complete layout-oriented Markdown rendering result.
///
/// `elided` retains Markdown bytes which intentionally have no terminal cell:
/// inline delimiters and fence lines. A source-Markdown copy mode can use
/// these ranges to widen a semantic selection while a plain-text mode ignores
/// them.
#[derive(Clone, Debug)]
pub(crate) struct MappedMarkdown {
    pub(crate) rows: Vec<MappedRow>,
    pub(crate) elided: Vec<SourceRange>,
}

/// Render a chat message into terminal rows.
///
/// This is intentionally a small, layout-oriented Markdown renderer. It keeps
/// the returned rows owned so a caller can cache them and scroll by rendered
/// row rather than by message. Fenced code is handed to the application's
/// existing syntax highlighter one source line at a time.
#[cfg(test)]
pub(crate) fn render_markdown(
    text: &str,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> Vec<Line<'static>> {
    render_markdown_mapped(text, width, highlighter)
        .rows
        .into_iter()
        .map(|row| row.line)
        .collect()
}

/// Render Markdown into terminal rows while preserving a mapping from each
/// visible cell to stable source bytes in `text`.
pub(crate) fn render_markdown_mapped(
    text: &str,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> MappedMarkdown {
    let width = width.max(DEFAULT_WIDTH);
    let mut mapped = MappedMarkdown {
        rows: Vec::new(),
        elided: Vec::new(),
    };
    let mut fence: Option<Fence> = None;
    let mut table_columns = None;
    let mut source_line_start = 0usize;
    let source_lines = text.split('\n').collect::<Vec<_>>();

    // `split` (rather than `lines`) deliberately retains a final empty item,
    // which makes explicit trailing blank lines visible in the transcript.
    for (index, source_line) in source_lines.iter().copied().enumerate() {
        if fence
            .as_ref()
            .is_some_and(|active_fence| is_fence(source_line, active_fence.marker))
        {
            fence = None;
            table_columns = None;
            mapped.elided.push(SourceRange::new(
                source_line_start,
                source_line_start + source_line.len(),
            ));
            source_line_start = source_line_start.saturating_add(source_line.len() + 1);
            continue;
        }
        if let Some(active_fence) = fence.as_mut() {
            table_columns = None;
            let expanded = expand_tabs_with_sources(source_line, source_line_start);
            let segments = highlighter
                .highlight_line(&active_fence.path, active_fence.line_number, &expanded.text)
                .unwrap_or_else(|_| plain_segment(&expanded.text));
            active_fence.line_number += 1;
            let mut atoms = vec![code_prefix()];
            atoms.extend(atoms_from_highlighted_segments(&segments, &expanded));
            push_wrapped_atoms(&mut mapped.rows, coalesce_display_clusters(atoms), width);
            source_line_start = source_line_start.saturating_add(source_line.len() + 1);
            continue;
        }

        if let Some((marker, info)) = opening_fence(source_line) {
            table_columns = None;
            fence = Some(Fence {
                marker,
                path: synthetic_path(info),
                line_number: 1,
            });
            mapped.elided.push(SourceRange::new(
                source_line_start,
                source_line_start + source_line.len(),
            ));
            source_line_start = source_line_start.saturating_add(source_line.len() + 1);
            continue;
        }

        let separator_columns = table_separator_columns(source_line);
        let row_columns = table_column_count(source_line);
        let next_separator_columns = source_lines
            .get(index + 1)
            .and_then(|next| table_separator_columns(next));
        let table_kind = if separator_columns.is_some() && separator_columns == table_columns {
            Some(TableLineKind::Separator)
        } else if row_columns.is_some()
            && (row_columns == table_columns || row_columns == next_separator_columns)
        {
            table_columns = row_columns;
            Some(TableLineKind::Row)
        } else {
            table_columns = None;
            None
        };
        render_text_line_mapped(
            source_line,
            source_line_start,
            width,
            &mut mapped,
            table_kind,
        );
        source_line_start = source_line_start.saturating_add(source_line.len() + 1);
    }

    if mapped.rows.is_empty() {
        mapped.rows.push(MappedRow {
            line: Line::from(""),
            cells: Vec::new(),
        });
    }
    mapped
}

#[derive(Clone)]
struct Fence {
    marker: char,
    path: PathBuf,
    line_number: usize,
}

fn render_text_line_mapped(
    source_line: &str,
    source_line_start: usize,
    width: usize,
    mapped: &mut MappedMarkdown,
    table_kind: Option<TableLineKind>,
) {
    if source_line.is_empty() {
        mapped.rows.push(MappedRow {
            line: Line::from(""),
            cells: Vec::new(),
        });
        return;
    }

    let mut atoms = markdown_atoms(
        source_line,
        source_line_start,
        &mut mapped.elided,
        table_kind,
    );
    atoms = coalesce_display_clusters(atoms);
    push_wrapped_atoms(&mut mapped.rows, atoms, width);
}

fn markdown_atoms(
    line: &str,
    source_line_start: usize,
    elided: &mut Vec<SourceRange>,
    table_kind: Option<TableLineKind>,
) -> Vec<RenderAtom> {
    let leading = line.len() - line.trim_start_matches(' ').len();
    let (indent, rest) = line.split_at(leading);
    let normal = Style::default().fg(Color::Rgb(210, 210, 210));

    match table_kind {
        Some(TableLineKind::Separator) => {
            let mut atoms = direct_atoms_at(indent, source_line_start, 0, normal);
            atoms.extend(table_separator_atoms(rest, source_line_start + leading));
            return atoms;
        }
        Some(TableLineKind::Row) => {
            let mut atoms = direct_atoms_at(indent, source_line_start, 0, normal);
            atoms.extend(table_row_atoms(
                rest,
                source_line_start + leading,
                leading,
                normal,
                elided,
            ));
            return atoms;
        }
        None => {}
    }

    if let Some((level, content)) = heading(rest) {
        let content_offset = line.len().saturating_sub(content.len());
        let style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let mut atoms = decoration_atoms(
            format!("{}{} ", indent, "#".repeat(level)),
            SourceRange::new(source_line_start, source_line_start + content_offset),
            style,
        );
        atoms.extend(inline_atoms(
            content,
            source_line_start + content_offset,
            source_column_at(line, content_offset, 0),
            style,
            elided,
        ));
        return atoms;
    }

    if let Some((depth, content)) = quote_content(rest) {
        let content_offset = line.len().saturating_sub(content.len());
        let mut atoms = decoration_atoms(
            format!("{indent}{}", "│ ".repeat(depth)),
            SourceRange::new(source_line_start, source_line_start + content_offset),
            Style::default().fg(Color::DarkGray),
        );
        let mut content_atoms = inline_atoms(
            content,
            source_line_start + content_offset,
            source_column_at(line, content_offset, 0),
            Style::default().fg(Color::Rgb(185, 195, 205)),
            elided,
        );
        strip_legacy_prefix_tab_cell(content, &mut content_atoms);
        atoms.extend(content_atoms);
        return atoms;
    }

    if let Some((marker, content)) = unordered_item(rest) {
        let content_offset = line.len().saturating_sub(content.len());
        let mut atoms = decoration_atoms(
            format!("{indent}{marker} "),
            SourceRange::new(source_line_start, source_line_start + content_offset),
            Style::default().fg(Color::Cyan),
        );
        let mut content_atoms = inline_atoms(
            content,
            source_line_start + content_offset,
            source_column_at(line, content_offset, 0),
            normal,
            elided,
        );
        strip_legacy_prefix_tab_cell(content, &mut content_atoms);
        atoms.extend(content_atoms);
        return atoms;
    }

    if let Some((marker, content)) = ordered_item(rest) {
        let content_offset = line.len().saturating_sub(content.len());
        let mut atoms = decoration_atoms(
            format!("{indent}{marker} "),
            SourceRange::new(source_line_start, source_line_start + content_offset),
            Style::default().fg(Color::Cyan),
        );
        let mut content_atoms = inline_atoms(
            content,
            source_line_start + content_offset,
            source_column_at(line, content_offset, 0),
            normal,
            elided,
        );
        strip_legacy_prefix_tab_cell(content, &mut content_atoms);
        atoms.extend(content_atoms);
        return atoms;
    }

    let mut atoms = direct_atoms_at(indent, source_line_start, 0, normal);
    atoms.extend(inline_atoms(
        rest,
        source_line_start + leading,
        leading,
        normal,
        elided,
    ));
    atoms
}

fn quote_content(line: &str) -> Option<(usize, &str)> {
    let mut depth = 0;
    let mut content = line;
    while let Some(rest) = content.strip_prefix('>') {
        depth += 1;
        content = rest.strip_prefix(' ').unwrap_or(rest);
    }
    (depth > 0).then_some((depth, content))
}

fn table_separator_columns(line: &str) -> Option<usize> {
    let cells = table_cell_texts(line)?;
    cells
        .iter()
        .all(|cell| {
            let rule = cell.trim().trim_matches(':');
            rule.len() >= 3 && rule.bytes().all(|byte| byte == b'-')
        })
        .then_some(cells.len())
}

fn table_column_count(line: &str) -> Option<usize> {
    table_cell_texts(line).map(|cells| cells.len())
}

fn table_cell_texts(line: &str) -> Option<Vec<String>> {
    let mut trimmed = line.trim();
    if let Some(without_prefix) = trimmed.strip_prefix('|') {
        trimmed = without_prefix;
    }
    if let Some(without_suffix) = trimmed.strip_suffix('|') {
        trimmed = without_suffix;
    }
    let mut cells = Vec::new();
    let mut cell = String::new();
    for (offset, character) in trimmed.char_indices() {
        if character == '|' && !escaped_pipe(trimmed, offset) {
            cells.push(cell.trim().to_owned());
            cell.clear();
        } else {
            cell.push(character);
        }
    }
    cells.push(cell.trim().to_owned());
    (cells.len() >= 2).then_some(cells)
}

#[derive(Clone, Copy)]
enum TableLineKind {
    Row,
    Separator,
}

fn table_separator_atoms(line: &str, source_start: usize) -> Vec<RenderAtom> {
    let style = Style::default().fg(Color::DarkGray);
    let mut atoms = Vec::new();
    for (offset, character) in line.char_indices() {
        let rendered = match character {
            '|' => "┼",
            '-' => "─",
            ':' => "·",
            _ => {
                atoms.extend(direct_atoms_at(
                    &line[offset..offset + character.len_utf8()],
                    source_start + offset,
                    offset,
                    style,
                ));
                continue;
            }
        };
        atoms.extend(decoration_atoms(
            rendered,
            SourceRange::new(
                source_start + offset,
                source_start + offset + character.len_utf8(),
            ),
            style,
        ));
    }
    atoms
}

fn table_row_atoms(
    line: &str,
    source_start: usize,
    initial_source_column: usize,
    style: Style,
    elided: &mut Vec<SourceRange>,
) -> Vec<RenderAtom> {
    let mut atoms = Vec::new();
    let mut segment_start = 0;
    for (offset, character) in line.char_indices() {
        if character != '|' || escaped_pipe(line, offset) {
            continue;
        }
        if segment_start < offset {
            atoms.extend(inline_atoms(
                &line[segment_start..offset],
                source_start + segment_start,
                source_column_at(line, segment_start, initial_source_column),
                style,
                elided,
            ));
        }
        atoms.extend(decoration_atoms(
            "│",
            SourceRange::new(source_start + offset, source_start + offset + 1),
            Style::default().fg(Color::Cyan),
        ));
        segment_start = offset + 1;
    }
    if segment_start < line.len() {
        atoms.extend(inline_atoms(
            &line[segment_start..],
            source_start + segment_start,
            source_column_at(line, segment_start, initial_source_column),
            style,
            elided,
        ));
    }
    atoms
}

fn escaped_pipe(line: &str, offset: usize) -> bool {
    line[..offset]
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count()
        % 2
        == 1
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&level) && line.as_bytes().get(level) == Some(&b' ') {
        Some((level, line[level + 1..].trim_start()))
    } else {
        None
    }
}

fn unordered_item(line: &str) -> Option<(String, &str)> {
    let marker = line.as_bytes().first().copied()?;
    if !matches!(marker, b'-' | b'+' | b'*')
        || !line
            .as_bytes()
            .get(1)
            .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    let rest = strip_optional_space(&line[2..]).unwrap_or("");
    let marker = match rest.as_bytes() {
        [b'[', state @ (b' ' | b'x' | b'X'), b']', whitespace, ..]
            if whitespace.is_ascii_whitespace() =>
        {
            let checked = *state != b' ';
            let content = strip_optional_space(&rest[4..]).unwrap_or("");
            return Some((if checked { "☑" } else { "☐" }.to_owned(), content));
        }
        _ => match marker {
            b'-' => "•",
            b'+' => "+",
            b'*' => "•",
            _ => unreachable!(),
        },
    };
    Some((marker.to_owned(), rest))
}

fn ordered_item(line: &str) -> Option<(String, &str)> {
    let digit_count = line
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_count == 0 {
        return None;
    }
    let marker_end = digit_count + 1;
    let marker = line.as_bytes().get(digit_count)?;
    if !matches!(marker, b'.' | b')')
        || !line
            .as_bytes()
            .get(marker_end)
            .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    Some((
        line[..marker_end].to_owned(),
        strip_optional_space(&line[marker_end + 1..]).unwrap_or(""),
    ))
}

fn strip_optional_space(line: &str) -> Option<&str> {
    line.strip_prefix(' ').or(Some(line))
}

fn opening_fence(line: &str) -> Option<(char, &str)> {
    let trimmed = line.trim_start();
    let marker = trimmed.chars().next()?;
    if !matches!(marker, '`' | '~')
        || trimmed
            .chars()
            .take_while(|character| *character == marker)
            .count()
            < 3
    {
        return None;
    }
    let marker_len = trimmed
        .chars()
        .take_while(|character| *character == marker)
        .count();
    Some((marker, trimmed[marker_len..].trim()))
}

fn is_fence(line: &str, marker: char) -> bool {
    let trimmed = line.trim_start();
    trimmed
        .chars()
        .take_while(|character| *character == marker)
        .count()
        >= 3
        && trimmed
            .chars()
            .skip_while(|character| *character == marker)
            .all(char::is_whitespace)
}

fn synthetic_path(info: &str) -> PathBuf {
    let language = info
        .split_whitespace()
        .next()
        .unwrap_or("text")
        .trim_matches(|character| matches!(character, '{' | '}' | ','));
    let extension = match language.to_ascii_lowercase().as_str() {
        "rs" | "rust" => "rs",
        "py" | "python" | "python3" => "py",
        "ts" | "typescript" => "ts",
        "tsx" => "tsx",
        "js" | "javascript" | "node" => "js",
        "jsx" => "jsx",
        "go" | "golang" => "go",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "sh" | "shell" | "bash" | "zsh" | "fish" => "sh",
        "c" => "c",
        "cc" | "cpp" | "c++" | "cxx" => "cpp",
        "h" | "hpp" => "h",
        "java" => "java",
        "cs" | "csharp" => "cs",
        "rb" | "ruby" => "rb",
        "php" => "php",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "swift" => "swift",
        "md" | "markdown" => "md",
        "xml" => "xml",
        "" | "text" | "txt" | "plaintext" => "txt",
        _ => "txt",
    };
    PathBuf::from(format!("chat.{extension}"))
}

fn code_prefix() -> RenderAtom {
    RenderAtom::synthetic(
        "  ",
        Style::default()
            .fg(Color::DarkGray)
            .bg(Color::Rgb(25, 29, 35)),
    )
}

fn inline_atoms(
    text: &str,
    source_start: usize,
    initial_source_column: usize,
    base_style: Style,
    elided: &mut Vec<SourceRange>,
) -> Vec<RenderAtom> {
    let mut atoms = Vec::new();
    let mut plain_start = 0;
    let mut cursor = 0;

    while cursor < text.len() {
        let rest = &text[cursor..];
        if let Some(escaped) = escaped_markdown_character(rest) {
            if plain_start < cursor {
                atoms.extend(direct_atoms_at(
                    &text[plain_start..cursor],
                    source_start + plain_start,
                    source_column_at(text, plain_start, initial_source_column),
                    base_style,
                ));
            }
            elided.push(SourceRange::new(
                source_start + cursor,
                source_start + cursor + 1,
            ));
            let character_start = cursor + 1;
            atoms.extend(direct_atoms_at(
                &text[character_start..character_start + escaped.len_utf8()],
                source_start + character_start,
                source_column_at(text, character_start, initial_source_column),
                base_style,
            ));
            cursor = character_start + escaped.len_utf8();
            plain_start = cursor;
            continue;
        }
        if let Some(link) = inline_link(rest) {
            if plain_start < cursor {
                atoms.extend(direct_atoms_at(
                    &text[plain_start..cursor],
                    source_start + plain_start,
                    source_column_at(text, plain_start, initial_source_column),
                    base_style,
                ));
            }
            let label_start = cursor + link.label.start;
            let label_end = cursor + link.label.end;
            elided.push(SourceRange::new(
                source_start + cursor,
                source_start + label_start,
            ));
            elided.push(SourceRange::new(
                source_start + label_end,
                source_start + cursor + link.consumed,
            ));
            let safe_destination = safe_link_destination(&rest[link.destination.clone()]);
            let link_color = if safe_destination {
                Color::LightBlue
            } else {
                Color::Yellow
            };
            let link_style = base_style.fg(link_color).add_modifier(Modifier::UNDERLINED);
            atoms.extend(inline_atoms(
                &text[label_start..label_end],
                source_start + label_start,
                source_column_at(text, label_start, initial_source_column),
                link_style,
                elided,
            ));
            atoms.push(RenderAtom::synthetic(
                if safe_destination { "↗" } else { "⚠" },
                Style::default().fg(link_color),
            ));
            cursor += link.consumed;
            plain_start = cursor;
            continue;
        }
        let (delimiter, style) = if rest.starts_with("***") {
            (
                "***",
                base_style.add_modifier(Modifier::BOLD | Modifier::ITALIC),
            )
        } else if rest.starts_with("**") {
            ("**", base_style.add_modifier(Modifier::BOLD))
        } else if rest.starts_with('`') {
            (
                "`",
                Style::default()
                    .fg(Color::LightCyan)
                    .bg(Color::Rgb(40, 45, 55)),
            )
        } else if rest.starts_with('*')
            || (rest.starts_with('_') && !is_word_underscore(text, cursor))
        {
            (
                if rest.starts_with('*') { "*" } else { "_" },
                base_style.add_modifier(Modifier::ITALIC),
            )
        } else {
            cursor += text[cursor..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(1);
            continue;
        };

        let Some(end) = rest[delimiter.len()..].find(delimiter) else {
            cursor += delimiter.chars().next().unwrap().len_utf8();
            continue;
        };
        let content_start = cursor + delimiter.len();
        let content_end = content_start + end;
        if content_end == content_start
            || (delimiter != "`" && rest[delimiter.len()..content_end - cursor].contains('\n'))
        {
            cursor += delimiter.chars().next().unwrap().len_utf8();
            continue;
        }
        if plain_start < cursor {
            atoms.extend(direct_atoms_at(
                &text[plain_start..cursor],
                source_start + plain_start,
                source_column_at(text, plain_start, initial_source_column),
                base_style,
            ));
        }
        elided.push(SourceRange::new(
            source_start + cursor,
            source_start + content_start,
        ));
        elided.push(SourceRange::new(
            source_start + content_end,
            source_start + content_end + delimiter.len(),
        ));
        if delimiter == "`" {
            atoms.extend(direct_atoms_at(
                &text[content_start..content_end],
                source_start + content_start,
                source_column_at(text, content_start, initial_source_column),
                style,
            ));
        } else {
            atoms.extend(inline_atoms(
                &text[content_start..content_end],
                source_start + content_start,
                source_column_at(text, content_start, initial_source_column),
                style,
                elided,
            ));
        }
        cursor = content_end + delimiter.len();
        plain_start = cursor;
    }

    if plain_start < text.len() {
        atoms.extend(direct_atoms_at(
            &text[plain_start..],
            source_start + plain_start,
            source_column_at(text, plain_start, initial_source_column),
            base_style,
        ));
    }
    if atoms.is_empty() && !text.is_empty() {
        atoms.extend(direct_atoms_at(
            text,
            source_start,
            initial_source_column,
            base_style,
        ));
    }
    atoms
}

fn escaped_markdown_character(text: &str) -> Option<char> {
    let escaped = text.strip_prefix('\\')?.chars().next()?;
    matches!(
        escaped,
        '\\' | '`'
            | '*'
            | '_'
            | '{'
            | '}'
            | '['
            | ']'
            | '('
            | ')'
            | '#'
            | '+'
            | '-'
            | '.'
            | '!'
            | '|'
            | '>'
    )
    .then_some(escaped)
}

/// Markdown prefix detection historically runs after tab expansion. Quote and
/// list parsers then remove one optional leading display-space. Preserve that
/// behavior when the raw content began with a tab.
fn strip_legacy_prefix_tab_cell(content: &str, atoms: &mut Vec<RenderAtom>) {
    if content.starts_with('\t') && atoms.first().is_some_and(|atom| atom.text == " ") {
        atoms.remove(0);
    }
}

fn is_word_underscore(text: &str, position: usize) -> bool {
    let before = text[..position].chars().next_back();
    let after = text[position + 1..].chars().next();
    before.is_some_and(|character| character.is_alphanumeric())
        && after.is_some_and(|character| character.is_alphanumeric())
}

#[derive(Clone)]
struct RenderAtom {
    text: String,
    style: Style,
    source: CellSource,
}

impl RenderAtom {
    fn new(text: impl Into<String>, style: Style, source: CellSource) -> Self {
        Self {
            text: text.into(),
            style,
            source,
        }
    }

    fn synthetic(text: impl Into<String>, style: Style) -> Self {
        Self::new(text, style, CellSource::Synthetic)
    }
}

fn plain_segment(text: &str) -> Vec<StyledSegment> {
    vec![StyledSegment {
        text: text.to_owned(),
        foreground: (210, 210, 210),
        bold: false,
        italic: false,
    }]
}

fn atoms_from_highlighted_segments(
    segments: &[StyledSegment],
    expanded: &ExpandedLine,
) -> Vec<RenderAtom> {
    let mut unit_index = 0usize;
    let mut atoms = Vec::new();
    for segment in segments {
        let mut style = Style::default().fg(Color::Rgb(
            segment.foreground.0,
            segment.foreground.1,
            segment.foreground.2,
        ));
        if segment.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if segment.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        style = style.bg(Color::Rgb(25, 29, 35));
        for character in segment.text.chars() {
            let source = expanded
                .units
                .get(unit_index)
                .map(|unit| CellSource::Text(unit.source.clone()))
                .unwrap_or(CellSource::Synthetic);
            atoms.push(RenderAtom::new(character.to_string(), style, source));
            unit_index += 1;
        }
    }
    atoms
}

fn direct_atoms_at(
    text: &str,
    source_start: usize,
    initial_source_column: usize,
    style: Style,
) -> Vec<RenderAtom> {
    let mut atoms = Vec::new();
    let mut cursor = 0usize;
    let mut source_column = initial_source_column;
    while cursor < text.len() {
        let (end, is_tab) = next_display_cluster(text, cursor);
        let source = SourceRange::new(source_start + cursor, source_start + end);
        if is_tab {
            let spaces = TAB_WIDTH - (source_column % TAB_WIDTH);
            atoms.extend(
                (0..spaces).map(|_| RenderAtom::new(" ", style, CellSource::Text(source.clone()))),
            );
            source_column += spaces;
        } else {
            let cluster = &text[cursor..end];
            source_column += terminal_cell_width_text(cluster);
            atoms.push(RenderAtom::new(cluster, style, CellSource::Text(source)));
        }
        cursor = end;
    }
    atoms
}

fn source_column_at(text: &str, offset: usize, initial_column: usize) -> usize {
    let mut column = initial_column;
    for character in text[..offset].chars() {
        if character == '\t' {
            column += TAB_WIDTH - (column % TAB_WIDTH);
        } else {
            column += terminal_cell_width(character);
        }
    }
    column
}

fn decoration_atoms(text: impl Into<String>, source: SourceRange, style: Style) -> Vec<RenderAtom> {
    let text = text.into();
    let mut atoms = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        let (end, _) = next_display_cluster(&text, cursor);
        atoms.push(RenderAtom::new(
            &text[cursor..end],
            style,
            CellSource::Decoration(source.clone()),
        ));
        cursor = end;
    }
    atoms
}

fn push_wrapped_atoms(rows: &mut Vec<MappedRow>, atoms: Vec<RenderAtom>, width: usize) {
    let mut current = Vec::new();
    let mut cells = Vec::new();
    let mut current_width = 0;
    let mut emitted = false;

    for atom in atoms {
        let atom_width = terminal_cell_width_text(&atom.text);
        if current_width + atom_width > width && !current.is_empty() {
            rows.push(MappedRow {
                line: Line::from(std::mem::take(&mut current)),
                cells: std::mem::take(&mut cells),
            });
            current_width = 0;
            emitted = true;
        }
        let column_start = current_width;
        current.push(Span::styled(atom.text, atom.style));
        if atom_width > 0 {
            cells.push(MappedCell {
                columns: column_start..column_start + atom_width,
                source: atom.source,
            });
        }
        current_width += atom_width;
    }

    if !current.is_empty() || !emitted {
        rows.push(MappedRow {
            line: Line::from(current),
            cells,
        });
    }
}

#[derive(Clone)]
struct ExpandedUnit {
    source: SourceRange,
}

struct ExpandedLine {
    text: String,
    units: Vec<ExpandedUnit>,
}

fn expand_tabs_with_sources(text: &str, source_start: usize) -> ExpandedLine {
    let mut expanded = String::with_capacity(text.len());
    let mut units = Vec::new();
    let mut column = 0;
    for (byte, character) in text.char_indices() {
        let source = SourceRange::new(
            source_start + byte,
            source_start + byte + character.len_utf8(),
        );
        if character == '\t' {
            let spaces = TAB_WIDTH - (column % TAB_WIDTH);
            expanded.extend(std::iter::repeat_n(' ', spaces));
            units.extend((0..spaces).map(|_| ExpandedUnit {
                source: source.clone(),
            }));
            column += spaces;
        } else {
            expanded.push(character);
            units.push(ExpandedUnit { source });
            column += terminal_cell_width(character);
        }
    }
    ExpandedLine {
        text: expanded,
        units,
    }
}

fn terminal_cell_width(character: char) -> usize {
    // Ratatui applies the same Unicode-width rules when it writes a Span into
    // the terminal buffer. Reusing Span::width keeps our pre-wrap boundary in
    // sync without exposing a second width implementation.
    Span::raw(character.to_string()).width()
}

fn terminal_cell_width_text(text: &str) -> usize {
    Span::raw(text.to_owned()).width()
}

fn next_display_cluster(text: &str, start: usize) -> (usize, bool) {
    let first = text[start..]
        .chars()
        .next()
        .expect("valid character boundary");
    let mut end = start + first.len_utf8();
    if first == '\t' {
        return (end, true);
    }
    let mut join_next = false;
    while end < text.len() {
        let character = text[end..]
            .chars()
            .next()
            .expect("valid character boundary");
        if is_grapheme_extend(character) {
            end += character.len_utf8();
        } else if character == '\u{200d}' {
            end += character.len_utf8();
            join_next = true;
        } else if join_next {
            end += character.len_utf8();
            join_next = false;
        } else {
            break;
        }
    }
    (end, false)
}

fn is_grapheme_extend(character: char) -> bool {
    matches!(
        character as u32,
        0x0300..=0x036f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe00..=0xfe0f
            | 0xfe20..=0xfe2f
            | 0x1f3fb..=0x1f3ff
            | 0xe0100..=0xe01ef
    )
}

fn coalesce_display_clusters(atoms: Vec<RenderAtom>) -> Vec<RenderAtom> {
    let mut merged: Vec<RenderAtom> = Vec::new();
    let mut join_next = false;
    for atom in atoms {
        let first = atom.text.chars().next();
        let attach = first
            .is_some_and(|character| is_grapheme_extend(character) || character == '\u{200d}')
            || join_next
            || merged
                .last()
                .is_some_and(|previous| previous.text.ends_with('\u{200d}'));
        if attach {
            if let Some(previous) = merged.last_mut() {
                previous.text.push_str(&atom.text);
                if let (CellSource::Text(left), CellSource::Text(right)) =
                    (&previous.source, &atom.source)
                {
                    if let Some(joined) = left.join(right) {
                        previous.source = CellSource::Text(joined);
                    }
                }
                join_next = previous.text.ends_with('\u{200d}');
                continue;
            }
        }
        join_next = atom.text.ends_with('\u{200d}');
        merged.push(atom);
    }
    merged
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use ratatui::style::{Color, Modifier};

    use super::{render_markdown, render_markdown_mapped, CellSource, SourceRange};
    use crate::highlight::{Highlighter, StyledSegment};

    #[derive(Default)]
    struct RecordingHighlighter {
        calls: Vec<(PathBuf, usize, String)>,
    }

    impl Highlighter for RecordingHighlighter {
        fn highlight_line(
            &mut self,
            path: &Path,
            line_number: usize,
            text: &str,
        ) -> Result<Vec<StyledSegment>> {
            self.calls
                .push((path.to_owned(), line_number, text.to_owned()));
            Ok(vec![StyledSegment {
                text: text.to_owned(),
                foreground: (1, 2, 3),
                bold: false,
                italic: false,
            }])
        }
    }

    fn text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn mapped_text(mapped: &super::MappedMarkdown) -> String {
        mapped
            .rows
            .iter()
            .map(|row| text(&row.line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn source_ranges(mapped: &super::MappedMarkdown) -> Vec<SourceRange> {
        mapped
            .rows
            .iter()
            .flat_map(|row| row.cells.iter())
            .filter_map(|cell| match &cell.source {
                CellSource::Text(range) | CellSource::Decoration(range) => Some(range.clone()),
                CellSource::Synthetic => None,
            })
            .collect()
    }

    #[test]
    fn renders_block_markdown_and_preserves_blank_lines() {
        let mut highlighter = RecordingHighlighter::default();
        let lines = render_markdown(
            "# Heading\n\n- [x] done\n2. next\n> quoted\n**bold** *italic* `code`",
            80,
            &mut highlighter,
        );
        let rendered: Vec<_> = lines.iter().map(text).collect();
        assert_eq!(
            rendered,
            [
                "# Heading",
                "",
                "☑ done",
                "2. next",
                "│ quoted",
                "bold italic code"
            ]
        );
        assert!(lines[5]
            .spans
            .iter()
            .any(|span| { span.style.add_modifier.contains(Modifier::BOLD) }));
        assert!(lines[5]
            .spans
            .iter()
            .any(|span| { span.style.add_modifier.contains(Modifier::ITALIC) }));
        assert!(lines[5]
            .spans
            .iter()
            .any(|span| span.style.bg == Some(Color::Rgb(40, 45, 55))));
    }

    #[test]
    fn highlights_fenced_code_with_a_language_specific_synthetic_path() {
        let mut highlighter = RecordingHighlighter::default();
        let lines = render_markdown("```python\nprint('hi')\n```", 80, &mut highlighter);
        assert_eq!(highlighter.calls.len(), 1);
        assert_eq!(highlighter.calls[0].0, Path::new("chat.py"));
        assert_eq!(highlighter.calls[0].1, 1);
        assert_eq!(highlighter.calls[0].2, "print('hi')");
        assert_eq!(text(&lines[0]), "  print('hi')");
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Rgb(25, 29, 35)));
    }

    #[test]
    fn malformed_fences_are_safe_and_tabs_and_long_text_wrap() {
        let mut highlighter = RecordingHighlighter::default();
        let lines = render_markdown("```rs\n\t0123456789\nwithout a close", 6, &mut highlighter);
        assert_eq!(highlighter.calls.len(), 2);
        assert!(highlighter
            .calls
            .iter()
            .all(|(_, _, line)| !line.contains('\t')));
        assert!(lines.iter().all(|line| text(line).chars().count() <= 6));
        assert!(text(&lines[0]).starts_with("  "));
        let rendered: Vec<_> = lines.iter().map(text).collect();
        assert!(rendered.join("").contains("without a close"));
    }

    #[test]
    fn wide_unicode_wraps_by_terminal_cells_without_clipping() {
        let mut highlighter = RecordingHighlighter::default();
        let source = "这是一个很长的中文响应，包含 🎉🚀 text";
        let lines = render_markdown(source, 10, &mut highlighter);
        assert!(lines.iter().all(|line| line.width() <= 10));
        assert_eq!(lines.iter().map(text).collect::<String>(), source);
    }

    #[test]
    fn empty_input_still_has_a_row() {
        let mut highlighter = RecordingHighlighter::default();
        let lines = render_markdown("", 80, &mut highlighter);
        assert_eq!(lines.len(), 1);
        assert_eq!(text(&lines[0]), "");
    }

    #[test]
    fn mapped_rows_preserve_text_and_markdown_decoration_ranges() {
        let source = "- [x] **bold** `code`";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(source, 80, &mut highlighter);

        assert_eq!(mapped_text(&mapped), "☑ bold code");
        let first = mapped.rows.first().expect("one rendered row");
        assert!(matches!(
            first.cells.first().map(|cell| &cell.source),
            Some(CellSource::Decoration(SourceRange { start: 0, end: 6 }))
        ));
        assert!(source_ranges(&mapped).contains(&SourceRange::new(8, 9)));
        assert!(source_ranges(&mapped).contains(&SourceRange::new(16, 17)));
        for delimiter in [
            SourceRange::new(6, 8),
            SourceRange::new(12, 14),
            SourceRange::new(15, 16),
            SourceRange::new(20, 21),
        ] {
            assert!(mapped.elided.contains(&delimiter), "missing {delimiter:?}");
        }
    }

    #[test]
    fn links_render_a_safe_affordance_and_keep_exact_label_mapping() {
        let source = "See [**docs**](https://example.invalid/review) now";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(source, 80, &mut highlighter);

        assert_eq!(mapped_text(&mapped), "See docs↗ now");
        let row = mapped.rows.first().expect("one link row");
        assert!(row.line.spans[4..8].iter().all(|span| {
            span.style.add_modifier.contains(Modifier::BOLD)
                && span.style.add_modifier.contains(Modifier::UNDERLINED)
        }));
        let label_start = source.find("docs").unwrap();
        for offset in label_start..label_start + "docs".len() {
            assert!(
                source_ranges(&mapped).contains(&SourceRange::new(offset, offset + 1)),
                "missing label byte {offset}"
            );
        }
        assert!(mapped.elided.contains(&SourceRange::new(4, 5)));
        assert!(mapped.elided.contains(&SourceRange::new(
            label_start + 6,
            source.find(" now").unwrap()
        )));
        assert!(matches!(
            row.cells
                .iter()
                .find(|cell| cell.columns.start == 8)
                .map(|cell| &cell.source),
            Some(CellSource::Synthetic)
        ));
    }

    #[test]
    fn malformed_links_remain_literal_text() {
        let source = "[label]() and [unfinished](https://example.invalid";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(source, 80, &mut highlighter);

        assert_eq!(mapped_text(&mapped), source);
        assert!(mapped.elided.is_empty());
    }

    #[test]
    fn unsafe_links_are_visibly_warned_instead_of_presented_as_safe() {
        let source = "[run](javascript:alert(1))";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(source, 80, &mut highlighter);

        assert_eq!(mapped_text(&mapped), "run⚠");
        assert_eq!(
            mapped
                .rows
                .first()
                .unwrap()
                .line
                .spans
                .last()
                .unwrap()
                .style
                .fg,
            Some(Color::Yellow)
        );
    }

    #[test]
    fn unicode_links_and_tables_reflow_without_losing_semantic_ownership() {
        let link = "[文档](https://example.invalid/review)";
        for width in [2, 3, 10, 80] {
            let mut highlighter = RecordingHighlighter::default();
            let mapped = render_markdown_mapped(link, width, &mut highlighter);
            assert_eq!(
                mapped
                    .rows
                    .iter()
                    .map(|row| text(&row.line))
                    .collect::<String>(),
                "文档↗"
            );
            assert!(mapped.rows.iter().all(|row| row.line.width() <= width));
            let label_start = link.find('文').unwrap();
            assert!(source_ranges(&mapped).contains(&SourceRange::new(
                label_start,
                label_start + '文'.len_utf8()
            )));
            assert!(mapped
                .rows
                .iter()
                .flat_map(|row| &row.cells)
                .any(|cell| cell.source == CellSource::Synthetic));
        }

        let table = "| A | B |\n| --- | --- |\n| 文 | 档 |";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(table, 3, &mut highlighter);
        assert!(mapped.rows.iter().all(|row| row.line.width() <= 3));
        for needle in ['A', 'B', '文', '档'] {
            let start = table.find(needle).unwrap();
            assert!(source_ranges(&mapped)
                .iter()
                .any(|range| range.start <= start && range.end > start));
        }
    }

    #[test]
    fn nested_inline_markup_combines_styles_without_exposing_delimiters() {
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped("**bold _nested_**", 80, &mut highlighter);

        assert_eq!(mapped_text(&mapped), "bold nested");
        let row = mapped.rows.first().unwrap();
        assert!(row.line.spans[..5]
            .iter()
            .all(|span| span.style.add_modifier.contains(Modifier::BOLD)));
        assert!(row.line.spans[5..].iter().all(|span| span
            .style
            .add_modifier
            .contains(Modifier::BOLD)
            && span.style.add_modifier.contains(Modifier::ITALIC)));
    }

    #[test]
    fn tables_and_nested_quotes_render_semantically_without_losing_source_ranges() {
        let source = "| Name | Result |\n| :--- | ---: |\n| `api` | **safe** |\n| a\\|b | exact |\n> > nested quote";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(source, 80, &mut highlighter);
        let rendered = mapped_text(&mapped);

        assert!(rendered.contains("│ Name │ Result │"));
        assert!(rendered.contains("┼ ·─── ┼ ───· ┼"));
        assert!(rendered.contains("│ api │ safe │"));
        assert!(rendered.contains("│ a|b │ exact │"));
        assert!(rendered.contains("│ │ nested quote"));
        for needle in ["Name", "Result", "api", "safe", "exact", "nested quote"] {
            let start = source.find(needle).unwrap();
            assert!(
                source_ranges(&mapped)
                    .iter()
                    .any(|range| range.start <= start && range.end > start),
                "missing mapping for {needle}"
            );
        }
        let escape = source.find("\\|").unwrap();
        assert!(mapped
            .elided
            .contains(&SourceRange::new(escape, escape + 1)));
    }

    #[test]
    fn pipe_prose_without_a_separator_is_not_misclassified_as_a_table() {
        let source = "| ordinary | pipe prose |";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(source, 80, &mut highlighter);

        assert_eq!(mapped_text(&mapped), source);
        assert!(mapped
            .rows
            .first()
            .unwrap()
            .cells
            .iter()
            .all(|cell| matches!(cell.source, CellSource::Text(_))));
    }

    #[test]
    fn tables_accept_optional_outer_pipes_and_reject_column_mismatches() {
        assert_eq!(super::table_cell_texts(r"| x\|y | z |").unwrap().len(), 2);
        assert_eq!(super::table_cell_texts(r"| x\\| y | z |").unwrap().len(), 3);

        let source = "A | B\n--- | ---\n文 | 档";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(source, 80, &mut highlighter);
        assert_eq!(mapped_text(&mapped), "A │ B\n─── ┼ ───\n文 │ 档");

        let mismatch = "| A | B |\n| --- | --- | --- |\n| one | two |";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(mismatch, 80, &mut highlighter);
        assert_eq!(mapped_text(&mapped), mismatch);

        let indented = "  | A | B |\n  | --- | --- |\n  | one | two |";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(indented, 80, &mut highlighter);
        assert!(mapped_text(&mapped).contains("\n  ┼ ─── ┼ ─── ┼\n"));
        assert!(source_ranges(&mapped).contains(&SourceRange::new(12, 13)));
        assert!(source_ranges(&mapped).contains(&SourceRange::new(13, 14)));
    }

    #[test]
    fn mapped_tabs_follow_original_columns_after_a_markdown_prefix() {
        let source = "- \tvalue";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(source, 80, &mut highlighter);
        let row = mapped.rows.first().expect("one row");
        let tab = SourceRange::new(2, 3);

        // The original tab starts at source column two, so it reaches the
        // four-column stop with two display spaces rather than four.
        assert_eq!(text(&row.line), "•  value");
        let tab_cells = row
            .cells
            .iter()
            .filter(|cell| cell.columns.start >= 2 && cell.columns.end <= 4)
            .filter(|cell| cell.source == CellSource::Text(tab.clone()))
            .count();
        assert_eq!(tab_cells, 1);
    }

    #[test]
    fn mapped_fence_tabs_and_emoji_keep_original_source_ownership() {
        let source = "```rust\n\tlet worker = \"👩\u{200d}💻\";\n```";
        let mut highlighter = RecordingHighlighter::default();
        let mapped = render_markdown_mapped(source, 80, &mut highlighter);
        let line_start = source.find('\n').expect("opening fence newline") + 1;
        let tab = SourceRange::new(line_start, line_start + 1);
        let code = mapped.rows.first().expect("code row");

        assert_eq!(text(&code.line), "      let worker = \"👩\u{200d}💻\";");
        assert!(matches!(code.cells[0].source, CellSource::Synthetic));
        assert!(code
            .cells
            .iter()
            .filter(|cell| cell.columns.start >= 2 && cell.columns.end <= 6)
            .all(|cell| cell.source == CellSource::Text(tab.clone())));
        let emoji_start = source.find('👩').expect("emoji source offset");
        let emoji = SourceRange::new(emoji_start, emoji_start + "👩\u{200d}💻".len());
        let emoji_cell = code
            .cells
            .iter()
            .find(|cell| cell.source == CellSource::Text(emoji.clone()))
            .expect("emoji cell mapping");
        assert!(
            emoji_cell
                .columns
                .end
                .saturating_sub(emoji_cell.columns.start)
                >= 2
        );
        assert!(mapped.elided.contains(&SourceRange::new(0, 7)));
        let closing_start = source.rfind("```").expect("closing fence");
        assert!(mapped
            .elided
            .contains(&SourceRange::new(closing_start, source.len())));
    }

    #[test]
    fn mapped_unicode_source_ranges_survive_rewrap() {
        let source = "A你e\u{301}👩\u{200d}💻Z";
        let mut narrow_highlighter = RecordingHighlighter::default();
        let narrow = render_markdown_mapped(source, 3, &mut narrow_highlighter);
        let mut wide_highlighter = RecordingHighlighter::default();
        let wide = render_markdown_mapped(source, 40, &mut wide_highlighter);

        assert_eq!(
            narrow
                .rows
                .iter()
                .map(|row| text(&row.line))
                .collect::<String>(),
            source
        );
        assert_eq!(mapped_text(&wide), source);
        assert!(narrow.rows.len() > wide.rows.len());

        let accent_start = source.find('e').expect("accent base");
        let accent = SourceRange::new(accent_start, accent_start + "e\u{301}".len());
        let emoji_start = source.find('👩').expect("emoji");
        let emoji = SourceRange::new(emoji_start, emoji_start + "👩\u{200d}💻".len());
        for mapped in [&narrow, &wide] {
            let ranges = source_ranges(mapped);
            assert!(ranges.contains(&accent));
            assert!(ranges.contains(&emoji));
            let width = if std::ptr::eq(mapped, &narrow) { 3 } else { 40 };
            assert!(mapped.rows.iter().all(|row| row.line.width() <= width));
        }
    }
}

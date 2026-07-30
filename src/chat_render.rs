use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::highlight::{Highlighter, StyledSegment};

const DEFAULT_WIDTH: usize = 1;
const TAB_WIDTH: usize = 4;

/// Render a chat message into terminal rows.
///
/// This is intentionally a small, layout-oriented Markdown renderer. It keeps
/// the returned rows owned so a caller can cache them and scroll by rendered
/// row rather than by message. Fenced code is handed to the application's
/// existing syntax highlighter one source line at a time.
pub(crate) fn render_markdown(
    text: &str,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> Vec<Line<'static>> {
    let width = width.max(DEFAULT_WIDTH);
    let mut rows = Vec::new();
    let mut fence: Option<Fence> = None;

    // `split` (rather than `lines`) deliberately retains a final empty item,
    // which makes explicit trailing blank lines visible in the transcript.
    for source_line in text.split('\n') {
        if fence
            .as_ref()
            .is_some_and(|active_fence| is_fence(source_line, active_fence.marker))
        {
            fence = None;
            continue;
        }
        if let Some(active_fence) = fence.as_mut() {
            let source_line = expand_tabs(source_line);
            let segments = highlighter
                .highlight_line(&active_fence.path, active_fence.line_number, &source_line)
                .unwrap_or_else(|_| plain_segment(&source_line));
            active_fence.line_number += 1;
            push_wrapped_segments(&mut rows, code_prefix(), segments, width);
            continue;
        }

        if let Some((marker, info)) = opening_fence(source_line) {
            fence = Some(Fence {
                marker,
                path: synthetic_path(info),
                line_number: 1,
            });
            continue;
        }

        render_text_line(source_line, width, &mut rows);
    }

    if rows.is_empty() {
        rows.push(Line::from(""));
    }
    rows
}

#[derive(Clone)]
struct Fence {
    marker: char,
    path: PathBuf,
    line_number: usize,
}

fn render_text_line(source_line: &str, width: usize, rows: &mut Vec<Line<'static>>) {
    if source_line.is_empty() {
        rows.push(Line::from(""));
        return;
    }

    let expanded = expand_tabs(source_line);
    let (prefix, content, prefix_style, content_style) = markdown_prefix(&expanded);
    let mut parts = Vec::new();
    if !prefix.is_empty() {
        parts.push(StyledPart::new(prefix, prefix_style));
    }
    parts.extend(inline_parts(content, content_style));
    push_wrapped_parts(rows, parts, width);
}

fn markdown_prefix(line: &str) -> (String, &str, Style, Style) {
    let leading = line.len() - line.trim_start_matches(' ').len();
    let (indent, rest) = line.split_at(leading);
    let normal = Style::default().fg(Color::Rgb(210, 210, 210));

    if let Some((level, content)) = heading(rest) {
        let prefix = format!("{}{} ", indent, "#".repeat(level));
        return (
            prefix,
            content,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }

    if let Some(content) = rest.strip_prefix('>').and_then(strip_optional_space) {
        return (
            format!("{indent}│ "),
            content,
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::Rgb(185, 195, 205)),
        );
    }

    if let Some((marker, content)) = unordered_item(rest) {
        return (
            format!("{indent}{marker} "),
            content,
            Style::default().fg(Color::Cyan),
            normal,
        );
    }

    if let Some((marker, content)) = ordered_item(rest) {
        return (
            format!("{indent}{marker} "),
            content,
            Style::default().fg(Color::Cyan),
            normal,
        );
    }

    (indent.to_owned(), rest, normal, normal)
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

fn code_prefix() -> StyledPart {
    StyledPart::new(
        "  ",
        Style::default()
            .fg(Color::DarkGray)
            .bg(Color::Rgb(25, 29, 35)),
    )
}

fn inline_parts(text: &str, base_style: Style) -> Vec<StyledPart> {
    let mut parts = Vec::new();
    let mut plain_start = 0;
    let mut cursor = 0;

    while cursor < text.len() {
        let rest = &text[cursor..];
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
            parts.push(StyledPart::new(
                text[plain_start..cursor].to_owned(),
                base_style,
            ));
        }
        parts.push(StyledPart::new(
            text[content_start..content_end].to_owned(),
            style,
        ));
        cursor = content_end + delimiter.len();
        plain_start = cursor;
    }

    if plain_start < text.len() {
        parts.push(StyledPart::new(text[plain_start..].to_owned(), base_style));
    }
    if parts.is_empty() {
        parts.push(StyledPart::new(text.to_owned(), base_style));
    }
    parts
}

fn is_word_underscore(text: &str, position: usize) -> bool {
    let before = text[..position].chars().next_back();
    let after = text[position + 1..].chars().next();
    before.is_some_and(|character| character.is_alphanumeric())
        && after.is_some_and(|character| character.is_alphanumeric())
}

#[derive(Clone)]
struct StyledPart {
    text: String,
    style: Style,
}

impl StyledPart {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
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

fn push_wrapped_segments(
    rows: &mut Vec<Line<'static>>,
    prefix: StyledPart,
    segments: Vec<StyledSegment>,
    width: usize,
) {
    let mut parts = vec![prefix];
    parts.extend(segments.into_iter().map(|segment| {
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
        StyledPart::new(segment.text, style.bg(Color::Rgb(25, 29, 35)))
    }));
    push_wrapped_parts(rows, parts, width);
}

fn push_wrapped_parts(rows: &mut Vec<Line<'static>>, parts: Vec<StyledPart>, width: usize) {
    let mut current = Vec::new();
    let mut current_width = 0;
    let mut emitted = false;

    for part in parts {
        let mut chunk = String::new();
        for character in part.text.chars() {
            let character_width = 1;
            if current_width + character_width > width && (!chunk.is_empty() || !current.is_empty())
            {
                if !chunk.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut chunk), part.style));
                }
                rows.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
                emitted = true;
            }
            chunk.push(character);
            current_width += character_width;
        }
        if !chunk.is_empty() {
            current.push(Span::styled(chunk, part.style));
        }
    }

    if !current.is_empty() || !emitted {
        rows.push(Line::from(current));
    }
}

fn expand_tabs(text: &str) -> String {
    let mut expanded = String::with_capacity(text.len());
    let mut column = 0;
    for character in text.chars() {
        if character == '\t' {
            let spaces = TAB_WIDTH - (column % TAB_WIDTH);
            expanded.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            expanded.push(character);
            column += 1;
        }
    }
    expanded
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use ratatui::style::{Color, Modifier};

    use super::render_markdown;
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
    fn empty_input_still_has_a_row() {
        let mut highlighter = RecordingHighlighter::default();
        let lines = render_markdown("", 80, &mut highlighter);
        assert_eq!(lines.len(), 1);
        assert_eq!(text(&lines[0]), "");
    }
}

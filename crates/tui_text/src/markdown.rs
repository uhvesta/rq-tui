use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InlineLink {
    pub(crate) label: Range<usize>,
    pub(crate) destination: Range<usize>,
    pub(crate) consumed: usize,
}

pub(crate) fn inline_link(text: &str) -> Option<InlineLink> {
    if !text.starts_with('[') {
        return None;
    }
    let label_end = text[1..].find("](")? + 1;
    if label_end == 1 {
        return None;
    }
    let destination_start = label_end + 2;
    let mut depth = 0usize;
    let mut escaped = false;
    let mut destination_end = None;
    for (offset, character) in text[destination_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '(' => depth += 1,
            ')' if depth == 0 => {
                destination_end = Some(destination_start + offset);
                break;
            }
            ')' => depth -= 1,
            _ => {}
        }
    }
    let destination_end = destination_end?;
    if destination_end == destination_start {
        return None;
    }
    Some(InlineLink {
        label: 1..label_end,
        destination: destination_start..destination_end,
        consumed: destination_end + 1,
    })
}

pub fn render_inline_html(text: &str) -> String {
    render_inline_html_inner(text, 0)
}

fn render_inline_html_inner(text: &str, depth: usize) -> String {
    if depth >= 16 {
        return escape_html(text);
    }
    let mut output = String::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let rest = &text[cursor..];
        if let Some(link) = inline_link(rest) {
            let label = render_inline_html_inner(&rest[link.label.clone()], depth + 1);
            let destination = normalized_link_destination(&rest[link.destination.clone()]);
            if safe_link_destination(&destination) {
                output.push_str("<a href=\"");
                output.push_str(&escape_html_attribute(&destination));
                output.push_str("\">");
                output.push_str(&label);
                output.push_str("</a>");
            } else {
                output.push_str("<span class=\"unsafe-link\" title=\"unsafe link omitted\">");
                output.push_str(&label);
                output.push_str("</span>");
            }
            cursor += link.consumed;
            continue;
        }
        if let Some(code) = rest.strip_prefix('`') {
            if let Some(end) = code.find('`') {
                output.push_str("<code>");
                output.push_str(&escape_html(&code[..end]));
                output.push_str("</code>");
                cursor += end + 2;
                continue;
            }
        }
        let delimiter = if rest.starts_with("***") {
            Some(("***", "<strong><em>", "</em></strong>"))
        } else if rest.starts_with("**") {
            Some(("**", "<strong>", "</strong>"))
        } else if rest.starts_with('*') {
            Some(("*", "<em>", "</em>"))
        } else if rest.starts_with('_') {
            Some(("_", "<em>", "</em>"))
        } else {
            None
        };
        if let Some((delimiter, opening, closing)) = delimiter {
            if let Some(end) = rest[delimiter.len()..].find(delimiter) {
                let content_start = delimiter.len();
                let content_end = content_start + end;
                if content_end > content_start {
                    output.push_str(opening);
                    output.push_str(&render_inline_html_inner(
                        &rest[content_start..content_end],
                        depth + 1,
                    ));
                    output.push_str(closing);
                    cursor += content_end + delimiter.len();
                    continue;
                }
            }
        }
        if let Some(after_slash) = rest.strip_prefix('\\') {
            if let Some(character) = after_slash
                .chars()
                .next()
                .filter(|character| markdown_escapable(*character))
            {
                push_escaped_char(&mut output, character);
                cursor += 1 + character.len_utf8();
                continue;
            }
        }
        let character = rest.chars().next().expect("cursor is in bounds");
        push_escaped_char(&mut output, character);
        cursor += character.len_utf8();
    }
    output
}

pub fn escape_html(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        push_escaped_char(&mut output, character);
    }
    output
}

fn escape_html_attribute(text: &str) -> String {
    escape_html(text)
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn push_escaped_char(output: &mut String, character: char) {
    match character {
        '&' => output.push_str("&amp;"),
        '<' => output.push_str("&lt;"),
        '>' => output.push_str("&gt;"),
        '"' => output.push_str("&quot;"),
        '\'' => output.push_str("&#39;"),
        _ => output.push(character),
    }
}

pub(crate) fn safe_link_destination(destination: &str) -> bool {
    let destination = normalized_link_destination(destination);
    if destination.is_empty()
        || destination.contains('\\')
        || destination
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return false;
    }
    let lower = destination.to_ascii_lowercase();
    lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("mailto:")
        || lower.starts_with('#')
        || (lower.starts_with('/') && !lower.starts_with("//"))
        || (!lower.contains(':') && !lower.starts_with("//") && !lower.starts_with('\\'))
}

fn normalized_link_destination(destination: &str) -> String {
    let mut output = String::with_capacity(destination.len());
    let mut characters = destination.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\'
            && characters
                .peek()
                .is_some_and(|next| matches!(next, '(' | ')' | '\\'))
        {
            output.push(characters.next().expect("peeked destination character"));
        } else {
            output.push(character);
        }
    }
    output
}

fn markdown_escapable(character: char) -> bool {
    matches!(
        character,
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
}

#[cfg(test)]
mod tests {
    use super::{inline_link, render_inline_html};

    #[test]
    fn parses_link_ranges_without_losing_unicode_offsets() {
        let source = "[文档](https://example.invalid/path)";
        let link = inline_link(source).unwrap();
        assert_eq!(&source[link.label], "文档");
        assert_eq!(&source[link.destination], "https://example.invalid/path");
        assert_eq!(link.consumed, source.len());
    }

    #[test]
    fn parses_balanced_and_escaped_destination_parentheses() {
        let source = "[docs](https://example.invalid/Function_(math\\)))";
        let link = inline_link(source).unwrap();
        assert_eq!(
            &source[link.destination],
            "https://example.invalid/Function_(math\\))"
        );
        assert_eq!(link.consumed, source.len());
        let html = render_inline_html(source);
        assert!(html.contains("href=\"https://example.invalid/Function_(math))\""));
        assert!(!html.ends_with(')'));
    }

    #[test]
    fn renders_inline_markup_and_rejects_executable_links() {
        let html = render_inline_html(
            "**safe** `code` [docs](https://example.invalid) \
             [bad](JaVaScRiPt:alert(1)) <script>",
        );
        assert!(html.contains("<strong>safe</strong>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<a href=\"https://example.invalid\">docs</a>"));
        assert!(html.contains("class=\"unsafe-link\""));
        assert!(!html.contains("href=\"JaVaScRiPt:"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn malformed_markup_stays_escaped_and_bounded() {
        let html = render_inline_html("[missing]( **open <tag>");
        assert_eq!(html, "[missing]( **open &lt;tag&gt;");
    }

    #[test]
    fn preserves_literal_backslashes_and_rejects_unsafe_url_forms() {
        let html = render_inline_html(
            r#"C:\temp [data](data:text/html,x) [file](file:///tmp/x) \
               [vb](vbscript:msgbox(1)) [network](//evil.invalid/x) \
               [backslash](/\evil.invalid/x) \
               [quoted](https://example.invalid/"x")"#,
        );
        assert!(html.contains(r"C:\temp"));
        assert_eq!(html.matches("class=\"unsafe-link\"").count(), 5);
        assert!(!html.contains("href=\"data:"));
        assert!(!html.contains("href=\"file:"));
        assert!(!html.contains("href=\"vbscript:"));
        assert!(!html.contains("href=\"//evil"));
        assert!(!html.contains("href=\"/\\evil"));
        assert!(html.contains("href=\"https://example.invalid/&quot;x&quot;\""));
    }
}

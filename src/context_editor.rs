//! Structured state and transport helpers for generated review context.
//!
//! The editor owns a [`ReviewContext`] rather than a serialized buffer.  The
//! label parser remains intentionally permissive because older agents emit
//! the legacy `Title: value` format, while the serializer uses Markdown
//! sections and fenced values so newlines, colons, and heading-like content
//! round-trip without being reinterpreted as fields.

use crate::domain::ReviewContext;

const FIELDS: [ContextField; 6] = [
    ContextField::Title,
    ContextField::What,
    ContextField::Why,
    ContextField::How,
    ContextField::Considerations,
    ContextField::Alternatives,
];

/// One of the six canonical fields shown by the context editor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ContextField {
    Title,
    What,
    Why,
    How,
    Considerations,
    Alternatives,
}

impl ContextField {
    pub(crate) const fn all() -> &'static [Self; 6] {
        &FIELDS
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Title => 0,
            Self::What => 1,
            Self::Why => 2,
            Self::How => 3,
            Self::Considerations => 4,
            Self::Alternatives => 5,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::What => "What",
            Self::Why => "Why",
            Self::How => "How",
            Self::Considerations => "Considerations",
            Self::Alternatives => "Other approaches",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        let normalized = normalize_label(label);
        match normalized.as_str() {
            "title" => Some(Self::Title),
            "what" => Some(Self::What),
            "why" => Some(Self::Why),
            "how" => Some(Self::How),
            "considerations" => Some(Self::Considerations),
            "other approaches" | "alternatives" => Some(Self::Alternatives),
            _ => None,
        }
    }

    fn get(self, context: &ReviewContext) -> &str {
        match self {
            Self::Title => &context.title,
            Self::What => &context.what,
            Self::Why => &context.why,
            Self::How => &context.how,
            Self::Considerations => &context.considerations,
            Self::Alternatives => &context.alternatives,
        }
    }

    fn get_mut(self, context: &mut ReviewContext) -> &mut String {
        match self {
            Self::Title => &mut context.title,
            Self::What => &mut context.what,
            Self::Why => &mut context.why,
            Self::How => &mut context.how,
            Self::Considerations => &mut context.considerations,
            Self::Alternatives => &mut context.alternatives,
        }
    }
}

/// A transient, independently navigable context draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextEditorState {
    pub(crate) base: ReviewContext,
    pub(crate) draft: ReviewContext,
    pub(crate) selected: usize,
    pub(crate) dirty: bool,
    pub(crate) raw_generation_stream: String,
    pub(crate) generation_id: Option<String>,
    pub(crate) generation_active: bool,
}

impl ContextEditorState {
    pub(crate) fn new(base: ReviewContext) -> Self {
        Self {
            draft: base.clone(),
            base,
            selected: 0,
            dirty: false,
            raw_generation_stream: String::new(),
            generation_id: None,
            generation_active: false,
        }
    }

    pub(crate) fn selected_field(&self) -> ContextField {
        FIELDS[self.selected.min(FIELDS.len() - 1)]
    }

    pub(crate) fn select_next(&mut self) -> ContextField {
        self.selected = (self.selected + 1) % FIELDS.len();
        self.selected_field()
    }

    pub(crate) fn select_previous(&mut self) -> ContextField {
        self.selected = if self.selected == 0 {
            FIELDS.len() - 1
        } else {
            self.selected - 1
        };
        self.selected_field()
    }

    pub(crate) fn select(&mut self, field: ContextField) {
        self.selected = field.index();
    }

    pub(crate) fn field(&self, field: ContextField) -> &str {
        field.get(&self.draft)
    }

    pub(crate) fn set_field(&mut self, field: ContextField, value: impl Into<String>) {
        *field.get_mut(&mut self.draft) = value.into();
        self.dirty = self.draft != self.base;
    }

    pub(crate) fn set_selected_field(&mut self, value: impl Into<String>) {
        let field = self.selected_field();
        self.set_field(field, value);
    }

    pub(crate) fn begin_generation(&mut self, generation_id: impl Into<String>) {
        self.raw_generation_stream.clear();
        self.generation_id = Some(generation_id.into());
        self.generation_active = true;
    }

    pub(crate) fn clear_fields_for_generation(&mut self) {
        self.draft.title.clear();
        self.draft.what.clear();
        self.draft.why.clear();
        self.draft.how.clear();
        self.draft.considerations.clear();
        self.draft.alternatives.clear();
        self.draft.source = "generated".into();
        self.draft.attached_to_session = false;
        self.draft.delivery_state = crate::domain::DeliveryState::Draft;
        self.dirty = false;
    }

    pub(crate) fn has_content(&self) -> bool {
        FIELDS
            .iter()
            .any(|field| !field.get(&self.draft).trim().is_empty())
    }

    pub(crate) fn apply_generation_partial(&mut self, generation_id: &str) -> bool {
        if !self.generation_active || self.generation_id.as_deref() != Some(generation_id) {
            return false;
        }
        let parsed = parse_context(&self.draft.work_item_id, &self.raw_generation_stream);
        if !self.dirty {
            self.draft = parsed.context;
            self.dirty = false;
        }
        true
    }

    pub(crate) fn fail_generation(&mut self, generation_id: &str) -> bool {
        self.finish_generation(generation_id)
    }

    pub(crate) fn append_generation_delta(&mut self, generation_id: &str, delta: &str) -> bool {
        if self.generation_active && self.generation_id.as_deref() == Some(generation_id) {
            self.raw_generation_stream.push_str(delta);
            true
        } else {
            false
        }
    }

    pub(crate) fn finish_generation(&mut self, generation_id: &str) -> bool {
        if self.generation_id.as_deref() == Some(generation_id) {
            self.generation_active = false;
            self.generation_id = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn replace_from_generation(
        &mut self,
        generation_id: &str,
    ) -> Option<ContextParseResult> {
        if !self.finish_generation(generation_id) {
            return None;
        }
        let parsed = parse_context(&self.draft.work_item_id, &self.raw_generation_stream);
        if !self.dirty {
            self.draft = parsed.context.clone();
            self.dirty = self.draft != self.base;
        }
        Some(parsed)
    }

    pub(crate) fn accept(&mut self) -> ReviewContext {
        self.base = self.draft.clone();
        self.dirty = false;
        self.draft.clone()
    }

    pub(crate) fn discard(&mut self) {
        self.draft = self.base.clone();
        self.dirty = false;
    }
}

/// A parser diagnostic.  Unknown labels and duplicate fields are warnings so
/// a partially useful agent response can still be edited and accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ContextWarning {
    DuplicateField { field: ContextField, line: usize },
    UnknownLabel { label: String, line: usize },
    ContinuationWithoutField { line: usize },
}

/// Parsed context plus non-fatal diagnostics from the agent transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContextParseResult {
    pub(crate) context: ReviewContext,
    pub(crate) warnings: Vec<ContextWarning>,
}

/// Parse both the legacy label format and the Markdown emitted by
/// [`serialize_context_markdown`].
pub(crate) fn parse_context(work_item_id: &str, source: &str) -> ContextParseResult {
    let mut context = ReviewContext {
        work_item_id: work_item_id.to_owned(),
        source: "generated".to_owned(),
        ..ReviewContext::default()
    };
    let mut warnings = Vec::new();
    let mut current = None;
    let mut value_lines = Vec::new();
    let mut seen = [false; 6];
    let mut fenced = false;
    let mut canonical_section = false;

    let finish = |context: &mut ReviewContext,
                  current: &mut Option<ContextField>,
                  value_lines: &mut Vec<String>| {
        if let Some(field) = current.take() {
            *field.get_mut(context) = value_lines.join("\n");
            value_lines.clear();
        }
    };

    let trailing_newline = source.ends_with('\n');
    let line_count = source.split('\n').count();
    for (line_index, raw_line) in source.split('\n').enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        if trailing_newline && line_index + 1 == line_count && line.is_empty() {
            continue;
        }

        if fenced {
            if line.trim() == "```" {
                fenced = false;
                continue;
            }
            value_lines.push(unescape_fence_line(line));
            continue;
        }

        if canonical_section && line.trim().is_empty() && current.is_some() {
            // Markdown's blank line between a heading and its fenced value is
            // presentation, not part of the field.
            continue;
        }

        if current.is_some() && line.trim_start().starts_with("```") {
            fenced = true;
            canonical_section = true;
            continue;
        }

        if let Some(heading) = markdown_heading(line) {
            finish(&mut context, &mut current, &mut value_lines);
            canonical_section = true;
            if let Some(field) = ContextField::from_label(heading) {
                if seen[field.index()] {
                    warnings.push(ContextWarning::DuplicateField {
                        field,
                        line: line_index + 1,
                    });
                }
                seen[field.index()] = true;
                current = Some(field);
            } else if !heading.trim().is_empty() && heading != "Review Context" {
                warnings.push(ContextWarning::UnknownLabel {
                    label: heading.trim().to_owned(),
                    line: line_index + 1,
                });
            }
            continue;
        }

        if let Some((label, value)) = legacy_label(line) {
            finish(&mut context, &mut current, &mut value_lines);
            let Some(field) = ContextField::from_label(label) else {
                warnings.push(ContextWarning::UnknownLabel {
                    label: normalize_label(label),
                    line: line_index + 1,
                });
                current = None;
                continue;
            };
            if seen[field.index()] {
                warnings.push(ContextWarning::DuplicateField {
                    field,
                    line: line_index + 1,
                });
            }
            seen[field.index()] = true;
            current = Some(field);
            value_lines.push(value.trim().to_owned());
            canonical_section = false;
            continue;
        }

        if current.is_some() {
            value_lines.push(continuation_line(line));
        } else if !line.trim().is_empty() && !is_markdown_boilerplate(line) {
            // Agent preamble/prose is intentionally ignored.  A continuation
            // is only meaningful after a recognized field.
            warnings.push(ContextWarning::ContinuationWithoutField {
                line: line_index + 1,
            });
        }
    }
    finish(&mut context, &mut current, &mut value_lines);

    ContextParseResult { context, warnings }
}

/// Compatibility name for callers that specifically consume agent labels.
pub(crate) fn parse_legacy_context(work_item_id: &str, source: &str) -> ContextParseResult {
    parse_context(work_item_id, source)
}

/// Serialize a context as Markdown while retaining every field's contents.
/// Fenced sections prevent colons, newlines, and Markdown-looking content in
/// a value from being mistaken for another field on a later parse.
pub(crate) fn serialize_context_markdown(context: &ReviewContext) -> String {
    let mut output = String::from("# Review Context\n\n");
    for field in FIELDS {
        output.push_str("## ");
        output.push_str(field.label());
        output.push_str("\n```text\n");
        for line in field.get(context).split('\n') {
            output.push_str(&escape_fence_line(line));
            output.push('\n');
        }
        output.push_str("```\n\n");
    }
    output
}

/// Compatibility name for storage-facing callers.
pub(crate) fn render_context_markdown(context: &ReviewContext) -> String {
    serialize_context_markdown(context)
}

fn markdown_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let hashes = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if (2..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
        Some(trimmed[hashes + 1..].trim())
    } else {
        None
    }
}

fn legacy_label(line: &str) -> Option<(&str, &str)> {
    let mut candidate = line.trim_start();
    if let Some(rest) = candidate.strip_prefix("- ") {
        candidate = rest.trim_start();
    }
    if let Some(rest) = candidate.strip_prefix("> ") {
        candidate = rest.trim_start();
    }
    if candidate.starts_with("**") && candidate.ends_with("**") {
        candidate = &candidate[2..candidate.len() - 2];
    }
    let (label, value) = candidate.split_once(':')?;
    let label = label.trim().trim_matches('*').trim();
    if label.is_empty()
        || !label.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == ' '
                || character == '_'
                || character == '-'
        })
    {
        return None;
    }
    Some((label, value))
}

fn normalize_label(label: &str) -> String {
    label
        .trim()
        .trim_matches('*')
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn continuation_line(line: &str) -> String {
    line.strip_prefix("  ")
        .or_else(|| line.strip_prefix("\t"))
        .unwrap_or(line)
        .to_owned()
}

fn is_markdown_boilerplate(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed == "---"
        || trimmed.starts_with("<!--")
        || trimmed.starts_with("# Review Context")
}

fn escape_fence_line(line: &str) -> String {
    let whitespace_len = line.len() - line.trim_start().len();
    let (whitespace, rest) = line.split_at(whitespace_len);
    if rest.starts_with("```") {
        format!("{whitespace}\\{rest}")
    } else {
        line.to_owned()
    }
}

fn unescape_fence_line(line: &str) -> String {
    let whitespace_len = line.len() - line.trim_start().len();
    let (whitespace, rest) = line.split_at(whitespace_len);
    if let Some(unescaped) = rest.strip_prefix("\\```") {
        format!("{whitespace}```{unescaped}")
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DeliveryState;

    fn context() -> ReviewContext {
        ReviewContext {
            work_item_id: "wi-1".into(),
            title: "Refresh: 15 minutes".into(),
            what: "Adds a check\nwith a continuation".into(),
            why: "Avoids logout: especially during review".into(),
            how: "validate() → refresh()".into(),
            considerations: "A heading-like line:\n## What\nKeep it visible".into(),
            alternatives: "Sliding-window cookie\nClient polling".into(),
            source: "manual".into(),
            attached_to_session: true,
            delivery_state: DeliveryState::Sent,
        }
    }

    #[test]
    fn fields_are_stable_and_navigation_wraps() {
        assert_eq!(ContextField::all().len(), 6);
        assert_eq!(ContextField::Alternatives.label(), "Other approaches");
        let mut editor = ContextEditorState::new(ReviewContext::default());
        assert_eq!(editor.selected_field(), ContextField::Title);
        editor.select_previous();
        assert_eq!(editor.selected_field(), ContextField::Alternatives);
        editor.select_next();
        assert_eq!(editor.selected_field(), ContextField::Title);
        editor.select(ContextField::How);
        assert_eq!(editor.selected, 3);
    }

    #[test]
    fn field_edits_and_accept_discard_track_dirty_state() {
        let base = ReviewContext::default();
        let mut editor = ContextEditorState::new(base.clone());
        editor.set_field(ContextField::Why, "because");
        assert!(editor.dirty);
        assert_eq!(editor.field(ContextField::Why), "because");
        editor.discard();
        assert!(!editor.dirty);
        assert_eq!(editor.field(ContextField::Why), "");
        editor.select(ContextField::Why);
        editor.set_selected_field("accepted");
        let accepted = editor.accept();
        assert!(!editor.dirty);
        assert_eq!(accepted.why, "accepted");
        assert_eq!(editor.base, accepted);
    }

    #[test]
    fn generation_stream_is_correlated_and_replaces_clean_draft() {
        let mut editor = ContextEditorState::new(ReviewContext::default());
        editor.begin_generation("g1");
        assert!(!editor.append_generation_delta("stale", "Title: bad"));
        assert!(editor.append_generation_delta("g1", "Title: Good\nWhat: Useful"));
        assert!(editor.generation_active);
        let parsed = editor
            .replace_from_generation("g1")
            .expect("matching generation");
        assert_eq!(parsed.context.title, "Good");
        assert_eq!(editor.draft.what, "Useful");
        assert!(!editor.generation_active);
        assert!(editor.replace_from_generation("g1").is_none());
    }

    #[test]
    fn dirty_generation_does_not_overwrite_manual_draft() {
        let mut editor = ContextEditorState::new(ReviewContext::default());
        editor.set_field(ContextField::Title, "Keep mine");
        editor.begin_generation("g1");
        editor.append_generation_delta("g1", "Title: Agent\nWhy: New reason");
        let parsed = editor
            .replace_from_generation("g1")
            .expect("matching generation");
        assert_eq!(parsed.context.title, "Agent");
        assert_eq!(editor.draft.title, "Keep mine");
        assert!(editor.dirty);
    }

    #[test]
    fn legacy_parser_preserves_colons_and_continuations() {
        let result = parse_legacy_context(
            "wi-2",
            "Title: A: useful title\n  second line\nWhat: First\n second line\nOther approaches: A: B",
        );
        assert_eq!(result.context.work_item_id, "wi-2");
        assert_eq!(result.context.title, "A: useful title\nsecond line");
        assert_eq!(result.context.what, "First\n second line");
        assert_eq!(result.context.alternatives, "A: B");
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn parser_reports_duplicate_and_unknown_labels() {
        let result = parse_context(
            "wi-3",
            "Title: first\nMood: speculative\nTitle: second\nWhy: useful",
        );
        assert_eq!(result.context.title, "second");
        assert!(result.warnings.iter().any(|warning| matches!(
            warning,
            ContextWarning::UnknownLabel { label, line: 2 } if label == "mood"
        )));
        assert!(result.warnings.iter().any(|warning| matches!(
            warning,
            ContextWarning::DuplicateField {
                field: ContextField::Title,
                line: 3
            }
        )));
    }

    #[test]
    fn markdown_serializer_is_lossless_for_multiline_markdown_values() {
        let original = context();
        let markdown = render_context_markdown(&original);
        assert!(markdown.starts_with("# Review Context\n"));
        assert!(markdown.contains("## Other approaches\n```text\n"));
        let parsed = parse_context(&original.work_item_id, &markdown);
        assert_eq!(parsed.context.title, original.title);
        assert_eq!(parsed.context.what, original.what);
        assert_eq!(parsed.context.why, original.why);
        assert_eq!(parsed.context.how, original.how);
        assert_eq!(parsed.context.considerations, original.considerations);
        assert_eq!(parsed.context.alternatives, original.alternatives);
        assert_eq!(parsed.context.source, "generated");
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn markdown_serializer_escapes_fences_and_empty_fields() {
        let original = ReviewContext {
            title: "before\n```\nafter".into(),
            ..ReviewContext::default()
        };
        let markdown = serialize_context_markdown(&original);
        assert!(markdown.contains("\\```"));
        let parsed = parse_context("wi-4", &markdown);
        assert_eq!(parsed.context.title, original.title);
        assert_eq!(parsed.context.what, "");
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn parser_accepts_markdown_and_legacy_aliases() {
        let result = parse_context(
            "wi-5",
            "## Title\n```text\nReadable\n```\n\nAlternatives: old path\n",
        );
        assert_eq!(result.context.title, "Readable");
        assert_eq!(result.context.alternatives, "old path");
        assert!(result.warnings.is_empty());
    }
}

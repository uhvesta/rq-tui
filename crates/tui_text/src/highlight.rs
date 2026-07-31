use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use anyhow::Result;
use lru::LruCache;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, HighlightState, Theme, ThemeSet};
use syntect::parsing::{ParseState, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSegment {
    pub text: String,
    pub foreground: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
}

pub trait Highlighter {
    fn highlight_line(
        &mut self,
        path: &Path,
        line_number: usize,
        text: &str,
    ) -> Result<Vec<StyledSegment>>;
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    path: PathBuf,
    line_number: usize,
    content_hash: u64,
}

pub struct SyntectHighlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
    cache: LruCache<CacheKey, Vec<StyledSegment>>,
    states: LruCache<CacheKey, (HighlightState, ParseState)>,
}

impl SyntectHighlighter {
    pub fn new(capacity: usize) -> Self {
        let syntaxes = two_face::syntax::extra_newlines();
        let themes = ThemeSet::load_defaults();
        let requested_theme = std::env::var("REV_SYNTAX_THEME").ok();
        let theme_name = requested_theme
            .as_deref()
            .filter(|name| themes.themes.contains_key(*name))
            .unwrap_or("base16-eighties.dark");
        let theme = themes.themes.get(theme_name).cloned().unwrap_or_default();
        Self {
            syntaxes,
            theme,
            cache: LruCache::new(
                NonZeroUsize::new(capacity.max(1)).expect("positive cache capacity"),
            ),
            states: LruCache::new(
                NonZeroUsize::new(capacity.max(1)).expect("positive state cache capacity"),
            ),
        }
    }

    fn syntax_for_path(&self, path: &Path) -> &SyntaxReference {
        if let Ok(Some(syntax)) = self.syntaxes.find_syntax_for_file(path) {
            return syntax;
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if matches!(
            file_name,
            "BUILD" | "BUILD.bazel" | "MODULE.bazel" | "WORKSPACE" | "WORKSPACE.bazel" | ".bazelrc"
        ) {
            return self
                .syntaxes
                .find_syntax_by_extension("bzl")
                .or_else(|| self.syntaxes.find_syntax_by_extension("py"))
                .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        }
        self.syntaxes.find_syntax_plain_text()
    }

    #[cfg(test)]
    pub fn syntax_name_for_path(&self, path: &Path) -> &str {
        self.syntax_for_path(path).name.as_str()
    }
}

impl Default for SyntectHighlighter {
    fn default() -> Self {
        Self::new(2_048)
    }
}

impl Highlighter for SyntectHighlighter {
    fn highlight_line(
        &mut self,
        path: &Path,
        line_number: usize,
        text: &str,
    ) -> Result<Vec<StyledSegment>> {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let key = CacheKey {
            path: path.to_path_buf(),
            line_number,
            content_hash: hasher.finish(),
        };
        if let Some(cached) = self.cache.get(&key) {
            return Ok(cached.clone());
        }

        let syntax = self.syntax_for_path(path);
        let previous_state = line_number.checked_sub(1).and_then(|previous_line| {
            self.states
                .iter()
                .find(|(candidate, _)| {
                    candidate.path == path && candidate.line_number == previous_line
                })
                .map(|(_, state)| state.clone())
        });
        let mut highlighter = if let Some((highlight_state, parse_state)) = previous_state {
            HighlightLines::from_state(&self.theme, highlight_state, parse_state)
        } else {
            HighlightLines::new(syntax, &self.theme)
        };
        let source = format!("{text}\n");
        let mut segments = Vec::new();
        for line in LinesWithEndings::from(&source) {
            for (style, piece) in highlighter.highlight_line(line, &self.syntaxes)? {
                segments.push(StyledSegment {
                    text: piece.trim_end_matches('\n').to_owned(),
                    foreground: (style.foreground.r, style.foreground.g, style.foreground.b),
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                });
            }
        }
        self.states.put(key.clone(), highlighter.state());
        self.cache.put(key, segments.clone());
        Ok(segments)
    }
}

#[derive(Default)]
pub struct PlainHighlighter;

impl Highlighter for PlainHighlighter {
    fn highlight_line(
        &mut self,
        _path: &Path,
        _line_number: usize,
        text: &str,
    ) -> Result<Vec<StyledSegment>> {
        Ok(vec![StyledSegment {
            text: text.to_owned(),
            foreground: (210, 210, 210),
            bold: false,
            italic: false,
        }])
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Highlighter, SyntectHighlighter};

    #[test]
    fn recognizes_common_languages() {
        let highlighter = SyntectHighlighter::new(32);
        for (file, expected) in [
            ("main.rs", "Rust"),
            ("main.py", "Python"),
            ("app.ts", "TypeScript"),
            ("app.js", "JavaScript"),
            ("main.go", "Go"),
            ("config.json", "JSON"),
            ("config.yml", "YAML"),
            ("Cargo.toml", "TOML"),
            ("README.md", "Markdown"),
            ("script.sh", "Bourne Again Shell (bash)"),
            ("main.c", "C"),
            ("main.cpp", "C++"),
            ("Main.java", "Java"),
            ("Program.cs", "C#"),
            ("app.rb", "Ruby"),
            ("index.php", "PHP"),
            ("query.sql", "SQL"),
            ("index.html", "HTML"),
            ("style.css", "CSS"),
            ("App.swift", "Swift"),
        ] {
            assert!(
                highlighter
                    .syntax_name_for_path(Path::new(file))
                    .contains(expected),
                "{file} was detected as {}",
                highlighter.syntax_name_for_path(Path::new(file))
            );
        }
    }

    #[test]
    fn highlights_only_requested_lines_and_reuses_cache() {
        let mut highlighter = SyntectHighlighter::new(2);
        let first = highlighter
            .highlight_line(Path::new("main.rs"), 1, "fn main() {}")
            .unwrap();
        let second = highlighter
            .highlight_line(Path::new("main.rs"), 1, "fn main() {}")
            .unwrap();
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn rust_uses_a_high_contrast_multi_scope_palette() {
        let mut highlighter = SyntectHighlighter::new(8);
        let segments = highlighter
            .highlight_line(
                Path::new("main.rs"),
                1,
                "pub fn render(value: Option<bool>) -> String { format!(\"{value:?}\") }",
            )
            .unwrap();
        let colors = segments
            .iter()
            .map(|segment| segment.foreground)
            .collect::<std::collections::HashSet<_>>();
        assert!(
            colors.len() >= 4,
            "Rust keywords, types, macros, strings, and punctuation should not collapse into one color: {segments:?}"
        );
    }

    #[test]
    fn bazel_entrypoints_use_starlark_or_python_instead_of_plain_text() {
        let highlighter = SyntectHighlighter::new(8);
        for file in ["BUILD", "BUILD.bazel", "MODULE.bazel", "WORKSPACE.bazel"] {
            assert_ne!(
                highlighter.syntax_name_for_path(Path::new(file)),
                "Plain Text",
                "{file} should receive Starlark-compatible highlighting"
            );
        }
    }

    #[test]
    fn contiguous_lines_preserve_multiline_lexical_state() {
        let mut sequential = SyntectHighlighter::new(8);
        sequential
            .highlight_line(Path::new("main.rs"), 1, "/* comment starts")
            .unwrap();
        let continued = sequential
            .highlight_line(
                Path::new("main.rs"),
                2,
                "still a comment */ let value = true;",
            )
            .unwrap();

        let mut isolated = SyntectHighlighter::new(8);
        let fresh = isolated
            .highlight_line(
                Path::new("other.rs"),
                2,
                "still a comment */ let value = true;",
            )
            .unwrap();
        assert_ne!(
            continued, fresh,
            "the second line should inherit the open block-comment scope"
        );
    }
}

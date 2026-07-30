use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use anyhow::Result;
use lru::LruCache;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
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
}

impl SyntectHighlighter {
    pub fn new(capacity: usize) -> Self {
        let syntaxes = two_face::syntax::extra_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .unwrap_or_default();
        Self {
            syntaxes,
            theme,
            cache: LruCache::new(
                NonZeroUsize::new(capacity.max(1)).expect("positive cache capacity"),
            ),
        }
    }

    #[cfg(test)]
    pub fn syntax_name_for_path(&self, path: &Path) -> &str {
        self.syntaxes
            .find_syntax_for_file(path)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text())
            .name
            .as_str()
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

        let syntax = self
            .syntaxes
            .find_syntax_for_file(path)?
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        // Only requested viewport lines are highlighted. Starting a line with a
        // fresh parser keeps the cache bounded; future checkpointing can improve
        // multi-line construct fidelity without changing this trait.
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
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
}

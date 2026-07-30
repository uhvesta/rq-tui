use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffSet {
    pub files: Vec<DiffFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFile {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub display_path: PathBuf,
    pub status: FileStatus,
    /// Cached once while parsing so repainting the file picker is O(files),
    /// not O(every changed line in the work item).
    pub additions: usize,
    pub deletions: usize,
    pub hunks: Vec<Hunk>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Renamed,
    #[default]
    Modified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Addition,
    Deletion,
    Meta,
}

impl DiffFile {
    pub fn path(&self) -> &Path {
        &self.display_path
    }

    pub fn visible_lines(&self) -> impl Iterator<Item = &DiffLine> {
        self.hunks.iter().flat_map(|hunk| hunk.lines.iter())
    }

    pub fn visible_line_count(&self) -> usize {
        self.hunks.iter().map(|hunk| hunk.lines.len()).sum()
    }

    pub fn change_counts(&self) -> (usize, usize) {
        (self.additions, self.deletions)
    }
}

pub fn parse_unified(input: &str) -> Result<DiffSet> {
    let mut files = Vec::new();
    let mut current_file: Option<DiffFile> = None;
    let mut current_hunk: Option<Hunk> = None;
    let mut old_line = 0;
    let mut new_line = 0;

    for raw in input.lines() {
        if raw.starts_with("diff --git ") {
            finish_hunk(&mut current_file, &mut current_hunk);
            if let Some(file) = current_file.take() {
                files.push(file);
            }
            let (old_path, new_path) = parse_diff_header(raw)?;
            current_file = Some(DiffFile {
                display_path: new_path.clone(),
                old_path: Some(old_path),
                new_path: Some(new_path),
                status: FileStatus::Modified,
                additions: 0,
                deletions: 0,
                hunks: Vec::new(),
            });
        } else if let Some(path) = raw.strip_prefix("rename from ") {
            if let Some(file) = current_file.as_mut() {
                file.status = FileStatus::Renamed;
                file.old_path = Some(parse_extended_path(path)?);
            }
        } else if let Some(path) = raw.strip_prefix("rename to ") {
            if let Some(file) = current_file.as_mut() {
                file.status = FileStatus::Renamed;
                let path = parse_extended_path(path)?;
                file.new_path = Some(path.clone());
                file.display_path = path;
            }
        } else if raw.starts_with("new file mode ") {
            if let Some(file) = current_file.as_mut() {
                file.status = FileStatus::Added;
            }
        } else if raw.starts_with("deleted file mode ") {
            if let Some(file) = current_file.as_mut() {
                file.status = FileStatus::Deleted;
                if let Some(old_path) = file.old_path.clone() {
                    file.display_path = old_path;
                }
            }
        } else if raw.starts_with("@@ ") {
            if let Some(previous) = current_hunk.as_mut() {
                let next_new_start = parse_hunk_header(raw)?.2;
                let previous_end = previous.new_start.saturating_add(previous.new_count);
                let gap = next_new_start.saturating_sub(previous_end);
                if gap > 0 {
                    previous.lines.push(DiffLine {
                        kind: LineKind::Meta,
                        old_line: None,
                        new_line: None,
                        content: format!(
                            "··· {gap} unchanged lines ··· (o expand 10 · O expand all)"
                        ),
                    });
                }
            }
            finish_hunk(&mut current_file, &mut current_hunk);
            let (old_start, old_count, new_start, new_count) = parse_hunk_header(raw)?;
            old_line = old_start;
            new_line = new_start;
            current_hunk = Some(Hunk {
                header: raw.to_owned(),
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
        } else if let Some(hunk) = current_hunk.as_mut() {
            let (kind, content, old, new) = if let Some(content) = raw.strip_prefix('+') {
                let line = new_line;
                new_line += 1;
                (LineKind::Addition, content, None, Some(line))
            } else if let Some(content) = raw.strip_prefix('-') {
                let line = old_line;
                old_line += 1;
                (LineKind::Deletion, content, Some(line), None)
            } else if let Some(content) = raw.strip_prefix(' ') {
                let old = old_line;
                let new = new_line;
                old_line += 1;
                new_line += 1;
                (LineKind::Context, content, Some(old), Some(new))
            } else {
                (LineKind::Meta, raw, None, None)
            };
            hunk.lines.push(DiffLine {
                kind,
                old_line: old,
                new_line: new,
                content: content.to_owned(),
            });
        }
    }

    finish_hunk(&mut current_file, &mut current_hunk);
    if let Some(file) = current_file {
        files.push(file);
    }
    Ok(DiffSet { files })
}

fn parse_extended_path(path: &str) -> Result<PathBuf> {
    if path.starts_with('"') {
        let tokens = split_git_tokens(path)?;
        return tokens
            .into_iter()
            .next()
            .map(PathBuf::from)
            .context("missing extended diff path");
    }
    Ok(PathBuf::from(path))
}

fn finish_hunk(file: &mut Option<DiffFile>, hunk: &mut Option<Hunk>) {
    if let (Some(file), Some(hunk)) = (file.as_mut(), hunk.take()) {
        for line in &hunk.lines {
            match line.kind {
                LineKind::Addition => file.additions += 1,
                LineKind::Deletion => file.deletions += 1,
                LineKind::Context | LineKind::Meta => {}
            }
        }
        file.hunks.push(hunk);
    }
}

fn parse_diff_header(line: &str) -> Result<(PathBuf, PathBuf)> {
    let rest = line
        .strip_prefix("diff --git ")
        .context("invalid diff header")?;
    let paths = split_git_tokens(rest)?;
    if paths.len() != 2 {
        anyhow::bail!("invalid diff paths");
    }
    let old = paths[0].strip_prefix("a/").unwrap_or(&paths[0]);
    let new = paths[1].strip_prefix("b/").unwrap_or(&paths[1]);
    Ok((PathBuf::from(old), PathBuf::from(new)))
}

fn split_git_tokens(input: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut characters = input.chars().peekable();
    let mut quoted = false;
    while let Some(character) = characters.next() {
        match character {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            '\\' if quoted => {
                let escaped = characters.next().context("trailing path escape")?;
                match escaped {
                    'n' => token.push('\n'),
                    't' => token.push('\t'),
                    'r' => token.push('\r'),
                    '\\' | '"' => token.push(escaped),
                    digit @ '0'..='7' => {
                        let mut octal = String::from(digit);
                        for _ in 0..2 {
                            if characters
                                .peek()
                                .is_some_and(|next| matches!(next, '0'..='7'))
                            {
                                octal.push(characters.next().expect("peeked"));
                            }
                        }
                        let byte = u8::from_str_radix(&octal, 8)?;
                        token.push(char::from(byte));
                    }
                    other => token.push(other),
                }
            }
            other => token.push(other),
        }
    }
    if quoted {
        anyhow::bail!("unterminated quoted diff path");
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Ok(tokens)
}

fn parse_hunk_header(line: &str) -> Result<(usize, usize, usize, usize)> {
    let body = line
        .strip_prefix("@@ -")
        .and_then(|value| value.split_once(" @@").map(|pair| pair.0))
        .context("invalid hunk header")?;
    let (old, new) = body.split_once(" +").context("invalid hunk line ranges")?;
    let (old_start, old_count) = parse_range(old)?;
    let (new_start, new_count) = parse_range(new)?;
    Ok((old_start, old_count, new_start, new_count))
}

fn parse_range(range: &str) -> Result<(usize, usize)> {
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    Ok((start.parse()?, count.parse()?))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{parse_unified, FileStatus, LineKind};

    #[test]
    fn parses_files_hunks_and_line_numbers() {
        let diff = "\\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
 one
-two
+two changed
+three
";
        let parsed = parse_unified(diff).unwrap();
        assert_eq!(parsed.files.len(), 1);
        let file = &parsed.files[0];
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!(file.change_counts(), (2, 1));
        assert_eq!(file.visible_line_count(), 4);
        assert_eq!(file.hunks[0].lines[1].kind, LineKind::Deletion);
        assert_eq!(file.hunks[0].lines[1].old_line, Some(2));
        assert_eq!(file.hunks[0].lines[2].new_line, Some(2));
    }

    #[test]
    fn follows_rename_metadata() {
        let diff = "\\
diff --git a/old.rs b/new.rs
similarity index 100%
rename from old.rs
rename to new.rs
";
        let parsed = parse_unified(diff).unwrap();
        assert_eq!(parsed.files[0].status, FileStatus::Renamed);
        assert_eq!(parsed.files[0].display_path.to_string_lossy(), "new.rs");
    }

    #[test]
    fn inserts_expandable_fold_rows_between_hunks() {
        let diff = "\\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-old
+new
@@ -20 +20 @@
-later
+changed
";
        let parsed = parse_unified(diff).unwrap();
        assert!(parsed.files[0].hunks[0]
            .lines
            .iter()
            .any(|line| line.kind == LineKind::Meta && line.content.contains("unchanged lines")));
    }

    #[test]
    fn parses_quoted_paths_with_spaces() {
        let parsed = parse_unified(
            "diff --git \"a/src/my file.rs\" \"b/src/my file.rs\"\n\\
             --- \"a/src/my file.rs\"\n\\
             +++ \"b/src/my file.rs\"\n\\
             @@ -1 +1 @@\n\\
             -old\n\\
             +new\n",
        )
        .unwrap();
        assert_eq!(
            parsed.files[0].display_path,
            PathBuf::from("src/my file.rs")
        );
    }
}

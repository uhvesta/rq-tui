use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::domain::{Annotation, AnnotationKind, AskMessage, Placement, Repo, WorkItem};
use crate::storage::{now, Storage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExportFormat {
    Markdown,
    Json,
}

impl ExportFormat {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "md" | "markdown" => Ok(Self::Markdown),
            "json" => Ok(Self::Json),
            other => bail!("unsupported export format: {other}"),
        }
    }

    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Json => "json",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExportedComment {
    pub(crate) annotation_id: String,
    pub(crate) repo: String,
    pub(crate) file: PathBuf,
    pub(crate) line_start: i64,
    pub(crate) line_end: i64,
    pub(crate) outdated: bool,
    pub(crate) ambiguous: bool,
    pub(crate) text: String,
    pub(crate) anchor_snippet: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommentExport {
    pub(crate) work_item_id: String,
    pub(crate) work_item_name: String,
    pub(crate) generated_at: String,
    pub(crate) comments: Vec<ExportedComment>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ArchivedAnnotation {
    pub(crate) annotation: Annotation,
    pub(crate) repo: String,
    pub(crate) placement: Placement,
    pub(crate) thread: Vec<AskMessage>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReviewArchive {
    pub(crate) work_item_id: String,
    pub(crate) work_item_name: String,
    pub(crate) generated_at: String,
    pub(crate) annotations: Vec<ArchivedAnnotation>,
}

impl CommentExport {
    pub(crate) fn load(storage: &Storage, item: &WorkItem) -> Result<Self> {
        let comments = storage
            .pending_comments_for_work_item(&item.id)?
            .into_iter()
            .map(|(annotation, repo)| exported_comment(storage, annotation, repo))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            work_item_id: item.id.clone(),
            work_item_name: item.name.clone(),
            generated_at: now(),
            comments,
        })
    }

    pub(crate) fn structured_session_message(&self) -> String {
        let mut message = format!(
            "Code review comments submitted for {} ({} items)\n",
            self.work_item_name,
            self.comments.len()
        );
        for (index, comment) in self.comments.iter().enumerate() {
            let marker = if comment.outdated {
                " [outdated]"
            } else if comment.ambiguous {
                " [re-anchored; verify]"
            } else {
                ""
            };
            let _ = writeln!(
                message,
                "{}. {}/{}:{}-{}{} — {}",
                index + 1,
                comment.repo,
                comment.file.display(),
                comment.line_start,
                comment.line_end,
                marker,
                comment.text
            );
        }
        message
    }

    pub(crate) fn render(&self, format: ExportFormat) -> Result<String> {
        match format {
            ExportFormat::Json => Ok(serde_json::to_string_pretty(self)?),
            ExportFormat::Markdown => Ok(self.markdown()),
        }
    }

    pub(crate) fn write(&self, format: ExportFormat, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.render(format)?)
            .with_context(|| format!("cannot write {}", path.display()))
    }

    fn markdown(&self) -> String {
        let mut output = format!(
            "# Code review: {}\n\nGenerated: {}\n",
            self.work_item_name, self.generated_at
        );
        let mut current_repo = "";
        let mut current_file = Path::new("");
        for comment in &self.comments {
            if current_repo != comment.repo {
                current_repo = &comment.repo;
                current_file = Path::new("");
                let _ = write!(output, "\n## {}\n", comment.repo);
            }
            if current_file != comment.file {
                current_file = &comment.file;
                let _ = write!(output, "\n### `{}`\n", comment.file.display());
            }
            let flags = match (comment.outdated, comment.ambiguous) {
                (true, _) => " — **outdated**",
                (false, true) => " — **re-anchored; verify**",
                _ => "",
            };
            let _ = write!(
                output,
                "\n- Lines {}–{}{}: {}\n\n  ```\n{}\n  ```\n",
                comment.line_start,
                comment.line_end,
                flags,
                comment.text,
                comment.anchor_snippet.replace('\n', "\n  ")
            );
        }
        output
    }
}

impl ReviewArchive {
    pub(crate) fn load(storage: &Storage, item: &WorkItem) -> Result<Self> {
        let mut unique = HashMap::<String, (Annotation, String)>::new();
        for repo in storage.repos_for_work_item(&item.id)? {
            for version in storage.versions_for_repo(&repo.id)? {
                for (annotation, _) in storage.annotations_for_version(&version.id)? {
                    unique
                        .entry(annotation.id.clone())
                        .or_insert_with(|| (annotation, repo.name.clone()));
                }
            }
        }
        let mut annotations = unique
            .into_values()
            .map(|(annotation, repo)| {
                let placement = storage
                    .latest_placement_for_annotation(&annotation.id)?
                    .with_context(|| format!("annotation {} has no placement", annotation.id))?;
                let thread = if annotation.kind == AnnotationKind::Ask {
                    storage.ask_messages_for_annotation(&annotation.id)?
                } else {
                    Vec::new()
                };
                Ok(ArchivedAnnotation {
                    annotation,
                    repo,
                    placement,
                    thread,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        annotations.sort_by(|left, right| {
            (
                &left.repo,
                &left.annotation.file_path,
                left.placement.line_start,
                &left.annotation.created_at,
            )
                .cmp(&(
                    &right.repo,
                    &right.annotation.file_path,
                    right.placement.line_start,
                    &right.annotation.created_at,
                ))
        });
        Ok(Self {
            work_item_id: item.id.clone(),
            work_item_name: item.name.clone(),
            generated_at: now(),
            annotations,
        })
    }

    pub(crate) fn write_markdown(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.markdown())
            .with_context(|| format!("cannot write {}", path.display()))
    }

    fn markdown(&self) -> String {
        let mut output = format!(
            "# Review archive: {}\n\nGenerated: {}\n",
            self.work_item_name, self.generated_at
        );
        let mut current_repo = "";
        let mut current_file = Path::new("");
        for entry in &self.annotations {
            if current_repo != entry.repo {
                current_repo = &entry.repo;
                current_file = Path::new("");
                let _ = write!(output, "\n## {}\n", entry.repo);
            }
            if current_file != entry.annotation.file_path {
                current_file = &entry.annotation.file_path;
                let _ = write!(output, "\n### `{}`\n", current_file.display());
            }
            let flags = match (entry.placement.outdated, entry.placement.ambiguous) {
                (true, _) => " — **outdated**",
                (false, true) => " — **re-anchored; verify**",
                _ => "",
            };
            let _ = writeln!(
                output,
                "\n#### {} at lines {}–{}{}",
                entry.annotation.kind.as_str(),
                entry.placement.line_start,
                entry.placement.line_end,
                flags
            );
            if let Some(text) = &entry.annotation.text {
                let _ = writeln!(output, "\n{text}");
            }
            for message in &entry.thread {
                let _ = writeln!(output, "\n- **{}:** {}", message.role, message.text);
            }
            let _ = write!(
                output,
                "\n```text\n{}\n```\n",
                entry.annotation.anchor_snippet
            );
        }
        output
    }
}

fn exported_comment(
    storage: &Storage,
    annotation: Annotation,
    repo: Repo,
) -> Result<ExportedComment> {
    let placement = storage
        .latest_placement_for_annotation(&annotation.id)?
        .with_context(|| format!("annotation {} has no placement", annotation.id))?;
    Ok(from_parts(annotation, repo, placement))
}

fn from_parts(annotation: Annotation, repo: Repo, placement: Placement) -> ExportedComment {
    ExportedComment {
        annotation_id: annotation.id,
        repo: repo.name,
        file: annotation.file_path,
        line_start: placement.line_start,
        line_end: placement.line_end,
        outdated: placement.outdated,
        ambiguous: placement.ambiguous,
        text: annotation.text.unwrap_or_default(),
        anchor_snippet: annotation.anchor_snippet,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{ArchivedAnnotation, CommentExport, ExportFormat, ExportedComment, ReviewArchive};
    use crate::domain::{Annotation, AnnotationKind, AskMessage, DeliveryState, Placement};

    fn export() -> CommentExport {
        CommentExport {
            work_item_id: "w".into(),
            work_item_name: "demo".into(),
            generated_at: "2026-01-01T00:00:00Z".into(),
            comments: vec![ExportedComment {
                annotation_id: "a".into(),
                repo: "api".into(),
                file: PathBuf::from("src/lib.rs"),
                line_start: 10,
                line_end: 12,
                outdated: false,
                ambiguous: true,
                text: "extract this".into(),
                anchor_snippet: "fn demo() {}".into(),
            }],
        }
    }

    #[test]
    fn markdown_groups_and_marks_ambiguous_comments() {
        let markdown = export().render(ExportFormat::Markdown).unwrap();
        assert!(markdown.contains("## api"));
        assert!(markdown.contains("### `src/lib.rs`"));
        assert!(markdown.contains("re-anchored; verify"));
    }

    #[test]
    fn json_is_structured() {
        let json = export().render(ExportFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["comments"][0]["line_start"], 10);
    }

    #[test]
    fn prune_archive_keeps_asks_and_their_full_thread() {
        let archive = ReviewArchive {
            work_item_id: "w".into(),
            work_item_name: "demo".into(),
            generated_at: "now".into(),
            annotations: vec![ArchivedAnnotation {
                annotation: Annotation {
                    id: "ask".into(),
                    repo_id: "repo".into(),
                    kind: AnnotationKind::Ask,
                    file_path: "src/lib.rs".into(),
                    anchor_snippet: "fn demo() {}".into(),
                    anchor_hash: "hash".into(),
                    anchor_start_offset: 0,
                    anchor_line_count: 1,
                    text: None,
                    submitted: false,
                    delivery_state: DeliveryState::Sent,
                    created_at: "now".into(),
                },
                repo: "api".into(),
                placement: Placement {
                    annotation_id: "ask".into(),
                    version_id: "v1".into(),
                    side: crate::domain::AnchorSide::New,
                    line_start: 3,
                    line_end: 3,
                    outdated: false,
                    ambiguous: false,
                },
                thread: vec![
                    AskMessage {
                        id: "question".into(),
                        annotation_id: "ask".into(),
                        seq: 0,
                        role: "user".into(),
                        text: "Why?".into(),
                        sent: true,
                        delivery_state: DeliveryState::Sent,
                        ts: "now".into(),
                    },
                    AskMessage {
                        id: "answer".into(),
                        annotation_id: "ask".into(),
                        seq: 1,
                        role: "assistant".into(),
                        text: "Because.".into(),
                        sent: true,
                        delivery_state: DeliveryState::Sent,
                        ts: "now".into(),
                    },
                ],
            }],
        };
        let markdown = archive.markdown();
        assert!(markdown.contains("#### ask at lines 3–3"));
        assert!(markdown.contains("**user:** Why?"));
        assert!(markdown.contains("**assistant:** Because."));
    }
}

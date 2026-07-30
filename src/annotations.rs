use std::cmp::Ordering;
use std::path::Path;

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use similar::TextDiff;
use uuid::Uuid;

use crate::diff::{DiffFile, DiffLine, LineKind};
use crate::domain::{
    AnchorSide, Annotation, AnnotationKind, AskMessage, DeliveryState, Placement, Repo, Version,
    VersionKind,
};
use crate::git::Git;
use crate::storage::{now, Storage};

const ANCHOR_CONTEXT: usize = 3;
const FUZZY_THRESHOLD: f32 = 0.62;

#[derive(Clone, Debug)]
pub(crate) struct AnchorData {
    pub(crate) snippet: String,
    pub(crate) hash: String,
    pub(crate) side: AnchorSide,
    pub(crate) start_offset: usize,
    pub(crate) selected_count: usize,
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct CreatedAnnotation {
    pub(crate) annotation: Annotation,
    pub(crate) placement: Placement,
    pub(crate) snapshot: Option<Version>,
    pub(crate) ask_message: Option<AskMessage>,
}

pub(crate) struct AnnotationRequest<'a> {
    pub(crate) repo: &'a Repo,
    pub(crate) current_version: &'a Version,
    pub(crate) file: &'a DiffFile,
    pub(crate) selection_start: usize,
    pub(crate) selection_end: usize,
    pub(crate) kind: AnnotationKind,
    pub(crate) text: String,
}

pub(crate) fn anchor_from_diff(
    file: &DiffFile,
    selection_start: usize,
    selection_end: usize,
) -> Result<AnchorData> {
    let lines = file.visible_lines().collect::<Vec<_>>();
    if lines.is_empty() {
        bail!("cannot annotate an empty diff");
    }
    let start = selection_start.min(selection_end).min(lines.len() - 1);
    let end = selection_start.max(selection_end).min(lines.len() - 1);
    let selected = &lines[start..=end];
    if selected.iter().any(|line| line.kind == LineKind::Meta) {
        bail!("fold and metadata rows cannot be annotation anchors");
    }
    let has_additions = selected.iter().any(|line| line.kind == LineKind::Addition);
    let has_deletions = selected.iter().any(|line| line.kind == LineKind::Deletion);
    if has_additions && has_deletions {
        bail!("select either old/deleted lines or new/added lines, not both");
    }
    let side = if has_deletions {
        AnchorSide::Old
    } else {
        AnchorSide::New
    };
    let source_lines = selected
        .iter()
        .map(|line| source_line(line, side))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| anyhow::anyhow!("selection has no source lines on the {side:?} side"))?;
    let snippet_start = start.saturating_sub(ANCHOR_CONTEXT);
    let snippet_end = (end + ANCHOR_CONTEXT + 1).min(lines.len());
    let snippet = lines[snippet_start..snippet_end]
        .iter()
        .filter(|line| line.kind != LineKind::Meta)
        .map(|line| line.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let start_offset = lines[snippet_start..start]
        .iter()
        .filter(|line| line.kind != LineKind::Meta)
        .count();
    let line_start = *source_lines.first().expect("selection is not empty");
    let line_end = *source_lines.last().expect("selection is not empty");
    let hash_source = containing_hunk(file, start)
        .map(|lines| {
            lines
                .iter()
                .filter(|line| line.kind != LineKind::Meta)
                .map(|line| line.content.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_else(|| snippet.clone());
    Ok(AnchorData {
        snippet,
        hash: sha256(&hash_source),
        side,
        start_offset,
        selected_count: end - start + 1,
        line_start,
        line_end,
    })
}

pub(crate) fn create_local_annotation(
    storage: &Storage,
    git: &Git,
    request: AnnotationRequest<'_>,
) -> Result<CreatedAnnotation> {
    let AnnotationRequest {
        repo,
        current_version,
        file,
        selection_start,
        selection_end,
        kind,
        text,
    } = request;
    let anchor = anchor_from_diff(file, selection_start, selection_end)?;
    let annotation_id = Uuid::new_v4().to_string();
    let snapshot = if current_version.kind == VersionKind::WorkingTree {
        Some(create_snapshot(storage, git, repo)?)
    } else {
        None
    };
    let placement_version = snapshot.as_ref().unwrap_or(current_version);
    let annotation = Annotation {
        id: annotation_id.clone(),
        repo_id: repo.id.clone(),
        kind,
        file_path: file.display_path.clone(),
        anchor_snippet: anchor.snippet,
        anchor_hash: anchor.hash,
        anchor_start_offset: anchor.start_offset as i64,
        anchor_line_count: anchor.selected_count as i64,
        text: (kind == AnnotationKind::Comment).then_some(text.clone()),
        submitted: false,
        delivery_state: if kind == AnnotationKind::Ask {
            DeliveryState::Pending
        } else {
            DeliveryState::Draft
        },
        created_at: now(),
    };
    let placement = Placement {
        annotation_id: annotation_id.clone(),
        version_id: placement_version.id.clone(),
        side: anchor.side,
        line_start: anchor.line_start as i64,
        line_end: anchor.line_end as i64,
        outdated: false,
        ambiguous: false,
    };
    storage.add_annotation(&annotation, &placement)?;
    if placement.version_id != current_version.id {
        storage.upsert_placement(&Placement {
            version_id: current_version.id.clone(),
            ..placement.clone()
        })?;
    }

    let ask_message = if kind == AnnotationKind::Ask {
        let message = AskMessage {
            id: Uuid::new_v4().to_string(),
            annotation_id,
            seq: 0,
            role: "user".into(),
            text,
            sent: false,
            delivery_state: DeliveryState::Pending,
            ts: now(),
        };
        storage.append_ask_message(&message)?;
        Some(message)
    } else {
        None
    };
    Ok(CreatedAnnotation {
        annotation,
        placement,
        snapshot,
        ask_message,
    })
}

pub(crate) fn create_snapshot(storage: &Storage, git: &Git, repo: &Repo) -> Result<Version> {
    let snapshot_number = storage
        .versions_for_repo(&repo.id)?
        .iter()
        .filter(|version| version.kind == VersionKind::Snapshot)
        .map(|version| version.version_num)
        .max()
        .unwrap_or(0)
        + 1;
    let id = format!("{}:snapshot:{snapshot_number}", repo.id);
    let commit = git.snapshot(&repo.path, &id)?;
    let version = Version {
        id,
        repo_id: repo.id.clone(),
        version_num: snapshot_number,
        kind: VersionKind::Snapshot,
        created_at: now(),
        head_sha: commit,
        worktree_path: None,
        last_opened_at: Some(now()),
    };
    storage.upsert_version(&version)?;
    let working_tree_id = format!("{}:working-tree", repo.id);
    for (annotation, placement) in storage.annotations_for_version(&working_tree_id)? {
        storage.upsert_placement(&Placement {
            annotation_id: annotation.id,
            version_id: version.id.clone(),
            ..placement
        })?;
    }
    Ok(version)
}

pub(crate) fn reanchor(
    annotation: &Annotation,
    previous: &Placement,
    version_id: &str,
    new_content: &str,
) -> Placement {
    let snippet_lines = annotation.anchor_snippet.lines().collect::<Vec<_>>();
    let content_lines = new_content.lines().collect::<Vec<_>>();
    if snippet_lines.is_empty() || content_lines.is_empty() {
        return outdated_placement(annotation, previous, version_id);
    }

    let window_size = snippet_lines.len().min(content_lines.len());
    let snippet = annotation.anchor_snippet.as_str();
    let mut candidates = content_lines
        .windows(window_size)
        .enumerate()
        .map(|(index, window)| {
            let candidate = window.join("\n");
            let score = if normalize(&candidate) == normalize(snippet) {
                1.0
            } else {
                TextDiff::from_lines(snippet, &candidate).ratio()
            };
            (index, score)
        })
        .filter(|(_, score)| *score >= FUZZY_THRESHOLD)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return outdated_placement(annotation, previous, version_id);
    }
    candidates.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                distance_to_previous(left.0, annotation, previous)
                    .cmp(&distance_to_previous(right.0, annotation, previous))
            })
    });
    let best_score = candidates[0].1;
    let tied = candidates
        .iter()
        .filter(|(_, score)| (best_score - score).abs() < 0.0001)
        .count();
    let snippet_start = candidates[0].0;
    let selected_start = snippet_start + annotation.anchor_start_offset.max(0) as usize;
    let count = annotation.anchor_line_count.max(1) as usize;
    Placement {
        annotation_id: annotation.id.clone(),
        version_id: version_id.to_owned(),
        side: AnchorSide::New,
        line_start: (selected_start + 1) as i64,
        line_end: (selected_start + count) as i64,
        outdated: false,
        ambiguous: tied > 1,
    }
}

pub(crate) fn follow_rename(
    storage: &Storage,
    annotation: &mut Annotation,
    renamed_to: Option<&Path>,
) -> Result<()> {
    if let Some(path) = renamed_to {
        annotation.file_path = path.to_path_buf();
        storage.update_annotation_file_path(&annotation.id, path)?;
    }
    Ok(())
}

pub(crate) fn self_contained_ask(
    annotation: &Annotation,
    message: &AskMessage,
    thread_tail: &[AskMessage],
) -> String {
    let tail = thread_tail
        .iter()
        .map(|entry| format!("{}: {}", entry.role, entry.text))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Code review ask\n\
         Annotation: {}\n\
         File: {}\n\
         Anchor:\n```\n{}\n```\n\
         Thread tail:\n{}\n\
         Question: {}",
        annotation.id,
        annotation.file_path.display(),
        annotation.anchor_snippet,
        if tail.is_empty() { "(none)" } else { &tail },
        message.text
    )
}

fn containing_hunk(file: &DiffFile, selected_index: usize) -> Option<&[DiffLine]> {
    let mut offset = 0;
    for hunk in &file.hunks {
        let end = offset + hunk.lines.len();
        if selected_index < end {
            return Some(&hunk.lines);
        }
        offset = end;
    }
    None
}

fn source_line(line: &DiffLine, side: AnchorSide) -> Option<usize> {
    match side {
        AnchorSide::Old => line.old_line,
        AnchorSide::New => line.new_line,
    }
}

fn outdated_placement(
    annotation: &Annotation,
    previous: &Placement,
    version_id: &str,
) -> Placement {
    Placement {
        annotation_id: annotation.id.clone(),
        version_id: version_id.to_owned(),
        side: previous.side,
        line_start: previous.line_start,
        line_end: previous.line_end,
        outdated: true,
        ambiguous: false,
    }
}

fn distance_to_previous(
    snippet_start: usize,
    annotation: &Annotation,
    previous: &Placement,
) -> usize {
    let candidate =
        snippet_start.saturating_add(annotation.anchor_start_offset.max(0) as usize) + 1;
    candidate.abs_diff(previous.line_start.max(1) as usize)
}

fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sha256(value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{anchor_from_diff, reanchor};
    use crate::diff::{DiffFile, DiffLine, FileStatus, Hunk, LineKind};
    use crate::domain::{AnchorSide, Annotation, AnnotationKind, DeliveryState, Placement};

    fn annotation(snippet: &str, offset: i64, count: i64) -> Annotation {
        Annotation {
            id: "a".into(),
            repo_id: "r".into(),
            kind: AnnotationKind::Comment,
            file_path: PathBuf::from("src/lib.rs"),
            anchor_snippet: snippet.into(),
            anchor_hash: "hash".into(),
            anchor_start_offset: offset,
            anchor_line_count: count,
            text: Some("note".into()),
            submitted: false,
            delivery_state: DeliveryState::Draft,
            created_at: String::new(),
        }
    }

    #[test]
    fn anchor_contains_selection_and_context() {
        let lines = (1..=10)
            .map(|number| DiffLine {
                kind: LineKind::Context,
                old_line: Some(number),
                new_line: Some(number),
                content: format!("line {number}"),
            })
            .collect();
        let file = DiffFile {
            old_path: Some("x".into()),
            new_path: Some("x".into()),
            display_path: "x".into(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                header: String::new(),
                old_start: 1,
                old_count: 10,
                new_start: 1,
                new_count: 10,
                lines,
            }],
        };
        let anchor = anchor_from_diff(&file, 4, 5).unwrap();
        assert_eq!(anchor.start_offset, 3);
        assert_eq!(anchor.selected_count, 2);
        assert!(anchor.snippet.starts_with("line 2"));
        assert!(anchor.snippet.ends_with("line 9"));
    }

    #[test]
    fn exact_reanchor_follows_moved_content() {
        let annotation = annotation("before\ntarget\nafter", 1, 1);
        let previous = Placement {
            annotation_id: "a".into(),
            version_id: "v1".into(),
            side: AnchorSide::New,
            line_start: 2,
            line_end: 2,
            outdated: false,
            ambiguous: false,
        };
        let placement = reanchor(
            &annotation,
            &previous,
            "v2",
            "new\nnewer\nbefore\ntarget\nafter\nend",
        );
        assert_eq!(placement.line_start, 4);
        assert!(!placement.outdated);
    }

    #[test]
    fn ambiguous_matches_choose_nearest_previous_location() {
        let annotation = annotation("target", 0, 1);
        let previous = Placement {
            annotation_id: "a".into(),
            version_id: "v1".into(),
            side: AnchorSide::New,
            line_start: 5,
            line_end: 5,
            outdated: false,
            ambiguous: false,
        };
        let placement = reanchor(&annotation, &previous, "v2", "target\nx\nx\nx\ntarget\nx");
        assert_eq!(placement.line_start, 5);
        assert!(placement.ambiguous);
    }

    #[test]
    fn missing_anchor_is_outdated_but_keeps_display_hint() {
        let annotation = annotation("gone", 0, 1);
        let previous = Placement {
            annotation_id: "a".into(),
            version_id: "v1".into(),
            side: AnchorSide::New,
            line_start: 8,
            line_end: 8,
            outdated: false,
            ambiguous: false,
        };
        let placement = reanchor(&annotation, &previous, "v2", "different content");
        assert!(placement.outdated);
        assert_eq!(placement.line_start, 8);
    }
}

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::annotations::{follow_rename, reanchor_outcome, should_auto_dismiss};
use crate::config::AppPaths;
use crate::diff::{parse_unified, DiffSet, FileStatus};
use crate::domain::{BaseBranchSource, Repo, Version, VersionKind, WorkItem};
use crate::git::{Git, LocalRepoState};
use crate::storage::{now, Storage};

#[derive(Clone, Debug)]
pub(crate) struct ReviewRepo {
    pub(crate) record: Repo,
    pub(crate) version: Version,
    pub(crate) diff: DiffSet,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedWorkItem {
    pub(crate) item: WorkItem,
    pub(crate) repos: Vec<ReviewRepo>,
    pub(crate) session_root: PathBuf,
}

pub(crate) fn combine_resolved(
    local: ResolvedWorkItem,
    remote: ResolvedWorkItem,
    paths: &AppPaths,
    storage: &Storage,
) -> Result<ResolvedWorkItem> {
    let mut remote_urls = remote
        .repos
        .iter()
        .filter_map(|repo| repo.record.remote_pr_url.clone())
        .collect::<Vec<_>>();
    remote_urls.sort();
    let identity = format!(
        "{}\n{}",
        local.item.workspace_root.display(),
        remote_urls.join("\n")
    );
    let item_id = stable_id("work", &identity);
    let session_root = paths.roots.join(&item_id);
    let timestamp = now();
    let item = WorkItem {
        id: item_id.clone(),
        name: format!("{} + {} PRs", local.item.name, remote.repos.len()),
        workspace_root: session_root.clone(),
        created_at: storage
            .work_item_by_root(&session_root)?
            .map(|item| item.created_at)
            .unwrap_or_else(|| timestamp.clone()),
        updated_at: timestamp.clone(),
        last_opened_at: Some(timestamp),
    };
    storage.upsert_work_item(&item)?;
    storage.set_setting(
        &format!("mixed_local_root:{item_id}"),
        &local.item.workspace_root.to_string_lossy(),
    )?;

    let mut repos = local
        .repos
        .into_iter()
        .chain(remote.repos)
        .collect::<Vec<_>>();
    repos.sort_by(|left, right| {
        right
            .record
            .last_activity_at
            .cmp(&left.record.last_activity_at)
    });
    for repo in &mut repos {
        repo.record.work_item_id = item_id.clone();
        storage.upsert_repo(&repo.record)?;
    }
    create_combined_root(&session_root, &repos)?;
    Ok(ResolvedWorkItem {
        item,
        repos,
        session_root,
    })
}

pub(crate) fn resolve_local(
    workspace: &Path,
    base_override: Option<&str>,
    paths: &AppPaths,
    storage: &Storage,
) -> Result<ResolvedWorkItem> {
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", workspace.display()))?;
    let git = Git::default();
    let candidates = git.discover_repositories(&workspace)?;
    if candidates.is_empty() {
        bail!("no git repositories found under {}", workspace.display());
    }

    let timestamp = now();
    let existing_item = storage.work_item_by_root(&workspace)?;
    let existing_repos = existing_item
        .as_ref()
        .map(|item| storage.repos_for_work_item(&item.id))
        .transpose()?
        .unwrap_or_default();
    let global_base = storage.setting("base_branch")?;
    let mut states = candidates
        .iter()
        .filter_map(|path| {
            let persisted = existing_repos.iter().find(|repo| {
                repo.path == *path && repo.base_branch_source != BaseBranchSource::Auto
            });
            let selected_base = base_override
                .or_else(|| persisted.and_then(|repo| repo.base_branch.as_deref()))
                .or(global_base.as_deref());
            let overridden =
                base_override.is_some() || persisted.is_some() || global_base.is_some();
            match git.inspect_local_repo(path, selected_base) {
                Ok(Some(state)) => Some(Ok((state, overridden))),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    states.sort_by(|left, right| right.0.last_activity_at.cmp(&left.0.last_activity_at));
    if states.is_empty() {
        bail!("no repositories with changes against their base were found");
    }

    let mut item = existing_item.unwrap_or_else(|| WorkItem {
        id: Uuid::new_v4().to_string(),
        name: workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("review")
            .to_owned(),
        workspace_root: workspace.clone(),
        created_at: timestamp.clone(),
        updated_at: timestamp.clone(),
        last_opened_at: None,
    });
    item.updated_at = timestamp.clone();
    item.last_opened_at = Some(timestamp.clone());
    storage.upsert_work_item(&item)?;

    let repos = states
        .into_iter()
        .map(|(state, overridden)| resolve_repo(&item, state, overridden, storage))
        .collect::<Result<Vec<_>>>()?;

    let session_root = paths.roots.join(&item.id);
    create_synthetic_root(&session_root, &repos)?;
    Ok(ResolvedWorkItem {
        item,
        repos,
        session_root,
    })
}

fn resolve_repo(
    item: &WorkItem,
    state: LocalRepoState,
    overridden: bool,
    storage: &Storage,
) -> Result<ReviewRepo> {
    let repo_id = stable_id("repo", &format!("{}:{}", item.id, state.path.display()));
    let repo = Repo {
        id: repo_id.clone(),
        work_item_id: item.id.clone(),
        name: state.name,
        path: state.path.clone(),
        remote_pr_url: None,
        pr_meta_json: None,
        base_branch: Some(state.base_branch),
        base_branch_source: if overridden {
            BaseBranchSource::PerRepo
        } else {
            BaseBranchSource::Auto
        },
        last_activity_at: Some(state.last_activity_at),
    };
    storage.upsert_repo(&repo)?;

    let version = Version {
        id: format!("{repo_id}:working-tree"),
        repo_id,
        version_num: 0,
        kind: VersionKind::WorkingTree,
        created_at: now(),
        head_sha: state.merge_base.clone(),
        worktree_path: None,
        last_opened_at: Some(now()),
    };
    storage.upsert_version(&version)?;
    let diff = parse_unified(&state.raw_diff)?;
    refresh_local_placements(storage, &repo, &version, &diff)?;
    Ok(ReviewRepo {
        record: repo,
        version,
        diff,
    })
}

fn refresh_local_placements(
    storage: &Storage,
    repo: &Repo,
    version: &Version,
    diff: &DiffSet,
) -> Result<()> {
    for (mut annotation, previous) in storage.annotation_history_for_version(&version.id)? {
        let renamed_to = diff.files.iter().find_map(|file| {
            (file.status == FileStatus::Renamed
                && file.old_path.as_ref() == Some(&annotation.file_path))
            .then_some(file.display_path.as_path())
        });
        follow_rename(storage, &mut annotation, renamed_to)?;
        let content_result = std::fs::read_to_string(repo.path.join(&annotation.file_path));
        let allow_dismiss = match &content_result {
            Ok(_) => true,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        let content = content_result.unwrap_or_default();
        let outcome = reanchor_outcome(&annotation, &previous, &version.id, &content);
        let manual_override = annotation
            .status_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("Auto-dismiss override:"));
        let reason = (annotation.status == crate::domain::AnnotationStatus::Active
            && allow_dismiss
            && !manual_override
            && should_auto_dismiss(&previous, &outcome))
        .then(|| {
            format!(
                "Auto-dismissed after the selected new-side code in {} changed or disappeared",
                annotation.file_path.display()
            )
        });
        storage.carry_forward_placement(
            &outcome.placement,
            reason.as_deref(),
            annotation.status_changed_at.as_deref(),
        )?;
    }
    Ok(())
}

fn stable_id(namespace: &str, value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace.as_bytes());
    digest.update([0]);
    digest.update(value.as_bytes());
    let hex = format!("{:x}", digest.finalize());
    format!("{namespace}-{}", &hex[..24])
}

fn create_synthetic_root(root: &Path, repos: &[ReviewRepo]) -> Result<()> {
    std::fs::create_dir_all(root)?;
    for repo in repos {
        let link = root.join(&repo.record.name);
        if link.exists() {
            continue;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo.record.path, &link).with_context(|| {
            format!(
                "cannot link {} to {}",
                link.display(),
                repo.record.path.display()
            )
        })?;
    }
    Ok(())
}

fn create_combined_root(root: &Path, repos: &[ReviewRepo]) -> Result<()> {
    std::fs::create_dir_all(root)?;
    for (index, repo) in repos.iter().enumerate() {
        let mut name = repo.record.name.clone();
        if repos[..index]
            .iter()
            .any(|candidate| candidate.record.name == name)
        {
            name = format!("{name}-{index}");
        }
        let link = root.join(name);
        let target = repo
            .version
            .worktree_path
            .as_deref()
            .unwrap_or(&repo.record.path);
        if link.symlink_metadata().is_ok() {
            if std::fs::read_link(&link).is_ok_and(|existing| existing == target) {
                continue;
            }
            std::fs::remove_file(&link)?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &link)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::stable_id;

    #[test]
    fn stable_ids_are_repeatable_and_namespaced() {
        assert_eq!(stable_id("repo", "x"), stable_id("repo", "x"));
        assert_ne!(stable_id("repo", "x"), stable_id("work", "x"));
    }
}

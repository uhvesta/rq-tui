use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::config::AppPaths;
use crate::export::ReviewArchive;
use crate::git::Git;
use crate::storage::Storage;

fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn prune_export_path(paths: &AppPaths, item_id: &str, item_name: &str) -> std::path::PathBuf {
    let digest = format!("{:x}", Sha256::digest(item_id.as_bytes()));
    paths.exports.join(format!(
        "{}-{}-pruned.md",
        item_name.replace(['/', ' '], "-"),
        &digest[..12]
    ))
}

/// Remove one reviewed Work Item after its Copilot sessions have been
/// confirmed absent.
///
/// Every filesystem/Git step is idempotent and the SQLite row is deleted last.
/// A crash therefore leaves enough database state to retry the local phase; if
/// the row is already absent, the previous attempt reached its final step.
pub(crate) fn prune_work_item(
    storage: &Storage,
    paths: &AppPaths,
    work_item_id: &str,
    export_first: bool,
    prune_operation_id: Option<&str>,
    lease_owner: Option<&str>,
) -> Result<()> {
    if storage.work_item_by_id(work_item_id)?.is_none() {
        return Ok(());
    }
    if let Some(owner_id) = lease_owner {
        anyhow::ensure!(
            storage.renew_ephemeral_session_lease(work_item_id, owner_id, wall_clock_ms())?,
            "Work Item cleanup ownership changed before local cleanup"
        );
    }
    prune_work_item_inner(
        storage,
        paths,
        work_item_id,
        export_first,
        prune_operation_id,
    )?;
    match (prune_operation_id, lease_owner) {
        (Some(operation_id), Some(owner_id)) => {
            anyhow::ensure!(
                storage.finalize_prune_operation(
                    operation_id,
                    work_item_id,
                    owner_id,
                    wall_clock_ms(),
                )?,
                "the Work Item disappeared before final local deletion"
            );
        }
        (None, None) => storage.delete_work_item(work_item_id)?,
        _ => anyhow::bail!("durable prune cleanup requires both operation and lease owner"),
    }
    Ok(())
}

fn prune_work_item_inner(
    storage: &Storage,
    paths: &AppPaths,
    work_item_id: &str,
    export_first: bool,
    prune_operation_id: Option<&str>,
) -> Result<()> {
    let item = storage
        .work_item_by_id(work_item_id)?
        .ok_or_else(|| anyhow::anyhow!("Work Item disappeared during local cleanup"))?;
    if let Some(operation_id) = prune_operation_id {
        anyhow::ensure!(
            storage
                .prune_targets(operation_id)?
                .iter()
                .all(|target| target.state == "deleted"),
            "a late Copilot session was captured; remote cleanup must retry before local deletion"
        );
    }
    if export_first {
        let archive = ReviewArchive::load(storage, &item)?;
        if !archive.annotations.is_empty() {
            let path = prune_export_path(paths, &item.id, &item.name);
            archive.write_markdown(&path)?;
        }
    }

    let git = Git::default();
    let repos = storage.repos_for_work_item(work_item_id)?;
    let mut remote_cache_roots = Vec::new();
    for repo in &repos {
        for version in storage.versions_for_repo(&repo.id)? {
            match version.kind {
                crate::domain::VersionKind::Remote => {
                    if let Some(worktree) = version.worktree_path {
                        if !storage.worktree_path_is_shared(&worktree, work_item_id)? {
                            git.remove_worktree(&repo.path, &worktree)?;
                        }
                    }
                }
                crate::domain::VersionKind::Snapshot => {
                    git.delete_snapshot_ref(&repo.path, &version.id)?;
                    let materialized = paths
                        .cache
                        .join("history")
                        .join(&repo.id)
                        .join(format!("s{}", version.version_num));
                    git.remove_worktree(&repo.path, &materialized)?;
                }
                crate::domain::VersionKind::WorkingTree => {}
            }
        }
        if repo.remote_pr_url.is_some() && !storage.repo_path_is_shared(&repo.path, work_item_id)? {
            if let Some(cache_root) = repo.path.parent() {
                remote_cache_roots.push(cache_root.to_path_buf());
            }
        }
    }
    let canonical_pr_cache = std::fs::canonicalize(&paths.prs).ok();
    for cache_root in remote_cache_roots {
        if let (Some(pr_cache), Ok(candidate)) = (
            canonical_pr_cache.as_ref(),
            std::fs::canonicalize(&cache_root),
        ) {
            if candidate.starts_with(pr_cache) && candidate.as_path() != pr_cache.as_path() {
                std::fs::remove_dir_all(candidate)?;
            }
        }
    }
    let work_item_component = std::path::Path::new(work_item_id);
    if work_item_component
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
        && work_item_component.components().count() == 1
    {
        let synthetic_root = paths.roots.join(work_item_component);
        if synthetic_root != paths.roots && synthetic_root.exists() {
            std::fs::remove_dir_all(synthetic_root)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{prune_export_path, prune_work_item};
    use crate::config::AppPaths;
    use crate::domain::{BaseBranchSource, Repo, SessionRecord, Version, VersionKind, WorkItem};
    use crate::storage::Storage;

    #[test]
    fn failed_local_cleanup_keeps_the_work_item_and_is_retryable() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let cache = directory.path().join("cache");
        let paths = AppPaths {
            database: data.join("review.db"),
            roots: data.join("roots"),
            exports: data.join("exports"),
            skills: data.join("skills"),
            plugins: data.join("plugins"),
            prs: cache.join("prs"),
            data,
            cache,
        };
        paths.ensure().unwrap();
        let storage = Storage::open(&paths.database).unwrap();
        let item = WorkItem {
            id: "retry-local-prune".into(),
            name: "retry local prune".into(),
            workspace_root: directory.path().join("workspace"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: Some("1".into()),
        };
        storage.upsert_work_item(&item).unwrap();
        storage.set_query_only_for_testing(true).unwrap();

        assert!(prune_work_item(&storage, &paths, &item.id, false, None, None).is_err());
        assert!(storage.work_item_by_id(&item.id).unwrap().is_some());

        storage.set_query_only_for_testing(false).unwrap();
        prune_work_item(&storage, &paths, &item.id, false, None, None).unwrap();
        assert!(storage.work_item_by_id(&item.id).unwrap().is_none());
        prune_work_item(&storage, &paths, &item.id, false, None, None).unwrap();
    }

    #[test]
    fn tampered_work_item_id_cannot_escape_the_synthetic_roots_directory() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let cache = directory.path().join("cache");
        let paths = AppPaths {
            database: data.join("review.db"),
            roots: data.join("roots"),
            exports: data.join("exports"),
            skills: data.join("skills"),
            plugins: data.join("plugins"),
            prs: cache.join("prs"),
            data,
            cache,
        };
        paths.ensure().unwrap();
        let outside = paths.roots.parent().unwrap().join("must-survive");
        std::fs::create_dir_all(&outside).unwrap();
        let marker = outside.join("marker");
        std::fs::write(&marker, "safe").unwrap();
        let storage = Storage::open(&paths.database).unwrap();
        let item = WorkItem {
            id: "../must-survive".into(),
            name: "tampered".into(),
            workspace_root: outside.clone(),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: Some("1".into()),
        };
        storage.upsert_work_item(&item).unwrap();

        prune_work_item(&storage, &paths, &item.id, false, None, None).unwrap();

        assert!(marker.exists());
        assert!(storage.work_item_by_id(&item.id).unwrap().is_none());
    }

    #[test]
    fn pruning_one_work_item_preserves_remote_cache_shared_by_another() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let cache = directory.path().join("cache");
        let paths = AppPaths {
            database: data.join("review.db"),
            roots: data.join("roots"),
            exports: data.join("exports"),
            skills: data.join("skills"),
            plugins: data.join("plugins"),
            prs: cache.join("prs"),
            data,
            cache,
        };
        paths.ensure().unwrap();
        let storage = Storage::open(&paths.database).unwrap();
        let cache_root = paths.prs.join("acme-api-42");
        let bare = cache_root.join("repo.git");
        let worktree = cache_root.join("worktrees").join("v1");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(bare.join("must-survive"), "shared bare repository").unwrap();
        std::fs::write(worktree.join("must-survive"), "shared worktree").unwrap();

        for item_id in ["first-review", "second-review"] {
            storage
                .upsert_work_item(&WorkItem {
                    id: item_id.into(),
                    name: item_id.into(),
                    workspace_root: paths.roots.join(item_id),
                    created_at: "1".into(),
                    updated_at: "1".into(),
                    last_opened_at: Some("1".into()),
                })
                .unwrap();
            let repo_id = format!("{item_id}-repo");
            storage
                .upsert_repo(&Repo {
                    id: repo_id.clone(),
                    work_item_id: item_id.into(),
                    name: "api".into(),
                    path: bare.clone(),
                    remote_pr_url: Some("https://github.com/acme/api/pull/42".into()),
                    pr_meta_json: None,
                    base_branch: Some("main".into()),
                    base_branch_source: BaseBranchSource::Auto,
                    last_activity_at: None,
                })
                .unwrap();
            storage
                .upsert_version(&Version {
                    id: format!("{repo_id}:v1"),
                    repo_id,
                    version_num: 1,
                    kind: VersionKind::Remote,
                    created_at: "1".into(),
                    head_sha: "shared-head".into(),
                    worktree_path: Some(worktree.clone()),
                    last_opened_at: Some("1".into()),
                })
                .unwrap();
        }

        prune_work_item(&storage, &paths, "first-review", false, None, None).unwrap();

        assert!(storage.work_item_by_id("first-review").unwrap().is_none());
        assert!(storage.work_item_by_id("second-review").unwrap().is_some());
        assert!(bare.join("must-survive").is_file());
        assert!(worktree.join("must-survive").is_file());
    }

    #[test]
    fn same_named_work_items_get_distinct_stable_export_paths() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            database: directory.path().join("db"),
            roots: directory.path().join("roots"),
            exports: directory.path().join("exports"),
            skills: directory.path().join("skills"),
            plugins: directory.path().join("plugins"),
            prs: directory.path().join("prs"),
            data: directory.path().join("data"),
            cache: directory.path().join("cache"),
        };

        let first = prune_export_path(&paths, "first-id", "same name");
        let second = prune_export_path(&paths, "second-id", "same name");

        assert_ne!(first, second);
        assert_eq!(first, prune_export_path(&paths, "first-id", "same name"));
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-pruned.md"));
    }

    #[test]
    fn late_captured_session_blocks_local_deletion_until_remote_retry_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let cache = directory.path().join("cache");
        let paths = AppPaths {
            database: data.join("review.db"),
            roots: data.join("roots"),
            exports: data.join("exports"),
            skills: data.join("skills"),
            plugins: data.join("plugins"),
            prs: cache.join("prs"),
            data,
            cache,
        };
        paths.ensure().unwrap();
        let storage = Storage::open(&paths.database).unwrap();
        let item = WorkItem {
            id: "late-session-prune".into(),
            name: "late session".into(),
            workspace_root: directory.path().join("workspace"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: Some("1".into()),
        };
        storage.upsert_work_item(&item).unwrap();
        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "owner", 1_000, 0)
            .unwrap());
        let operation_id = storage
            .begin_prune_operation("operation", &item.id, false)
            .unwrap();
        let activation = storage.activate_session(&SessionRecord {
            id: "late-session".into(),
            work_item_id: item.id.clone(),
            parent_id: Some("parent".into()),
            active: true,
            created_at: "2".into(),
        });
        assert!(activation.is_err());

        let first = prune_work_item(
            &storage,
            &paths,
            &item.id,
            false,
            Some(&operation_id),
            Some("owner"),
        );
        assert!(first
            .unwrap_err()
            .to_string()
            .contains("late Copilot session"));
        assert!(storage.work_item_by_id(&item.id).unwrap().is_some());
        assert!(!storage.clear_prune_operation(&operation_id).unwrap());

        assert!(storage
            .mark_prune_target_deleted_if_owned(
                &operation_id,
                "persistent:late-session",
                &item.id,
                "owner",
            )
            .unwrap());
        prune_work_item(
            &storage,
            &paths,
            &item.id,
            false,
            Some(&operation_id),
            Some("owner"),
        )
        .unwrap();
        assert!(storage.work_item_by_id(&item.id).unwrap().is_none());
        assert!(storage.prune_targets(&operation_id).unwrap().is_empty());
    }
}

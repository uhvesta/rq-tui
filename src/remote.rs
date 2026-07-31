use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::annotations::{follow_rename, reanchor};
use crate::config::AppPaths;
use crate::diff::parse_unified;
use crate::domain::{
    BaseBranchSource, DeliveryState, Repo, ReviewContext, Version, VersionKind, WorkItem,
};
use crate::process_control::output_with_timeout;
use crate::storage::{now, Storage};
use crate::work_item::{ResolvedWorkItem, ReviewRepo};

const NETWORK_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrReference {
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) number: u64,
}

impl PrReference {
    pub(crate) fn parse(input: &str) -> Result<Self> {
        let value = input
            .trim()
            .trim_end_matches('/')
            .strip_prefix("https://")
            .or_else(|| input.trim().trim_end_matches('/').strip_prefix("http://"))
            .unwrap_or(input.trim().trim_end_matches('/'))
            .strip_prefix("github.com/")
            .unwrap_or_else(|| {
                input
                    .trim()
                    .trim_end_matches('/')
                    .strip_prefix("https://github.com/")
                    .unwrap_or(input.trim().trim_end_matches('/'))
            });

        if let Some((name, number)) = value.rsplit_once('#') {
            let (owner, repo) = name
                .trim_start_matches("github.com/")
                .split_once('/')
                .context("PR reference must include owner and repository")?;
            let reference = Self {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                number: number.parse().context("invalid PR number")?,
            };
            reference.validate()?;
            return Ok(reference);
        }

        let parts = value
            .trim_start_matches("github.com/")
            .split('/')
            .collect::<Vec<_>>();
        if let [owner, repo, "pull", number] = parts.as_slice() {
            let reference = Self {
                owner: (*owner).to_owned(),
                repo: (*repo).to_owned(),
                number: number.parse().context("invalid PR number")?,
            };
            reference.validate()?;
            return Ok(reference);
        }
        bail!("invalid PR reference: {input}")
    }

    fn validate(&self) -> Result<()> {
        for (label, value) in [("owner", &self.owner), ("repository", &self.repo)] {
            if value.is_empty()
                || matches!(value.as_str(), "." | "..")
                || !value
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
            {
                bail!("invalid GitHub {label} in pull-request reference")
            }
        }
        if self.number == 0 {
            bail!("pull-request number must be greater than zero")
        }
        Ok(())
    }

    pub(crate) fn canonical_url(&self) -> String {
        format!(
            "https://github.com/{}/{}/pull/{}",
            self.owner, self.repo, self.number
        )
    }

    pub(crate) fn gh_selector(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }

    pub(crate) fn cache_relative_path(&self) -> PathBuf {
        PathBuf::from(&self.owner)
            .join(&self.repo)
            .join(format!("pr-{}", self.number))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrMetadata {
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) url: String,
    pub(crate) state: String,
    pub(crate) merged_at: Option<String>,
    pub(crate) updated_at: String,
    pub(crate) base_ref_name: String,
    pub(crate) head_ref_oid: String,
}

pub(crate) trait ProcessRunner {
    fn output(&self, command: &mut Command) -> Result<Output>;
}

#[derive(Default)]
pub(crate) struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn output(&self, command: &mut Command) -> Result<Output> {
        let operation = std::iter::once(command.get_program().to_string_lossy().into_owned())
            .chain(
                command
                    .get_args()
                    .map(|arg| arg.to_string_lossy().into_owned()),
            )
            .collect::<Vec<_>>()
            .join(" ");
        output_with_timeout(command, &operation, NETWORK_COMMAND_TIMEOUT)
    }
}

pub(crate) struct RemoteResolver<R = SystemProcessRunner> {
    runner: R,
}

impl Default for RemoteResolver<SystemProcessRunner> {
    fn default() -> Self {
        Self {
            runner: SystemProcessRunner,
        }
    }
}

impl<R: ProcessRunner> RemoteResolver<R> {
    pub(crate) fn metadata(&self, reference: &PrReference) -> Result<PrMetadata> {
        let output = self.runner.output(Command::new("gh").args([
            "pr",
            "view",
            &reference.gh_selector(),
            "--json",
            "number,title,body,url,state,mergedAt,updatedAt,baseRefName,headRefOid",
        ]))?;
        ensure_success("gh pr view", &output)?;
        serde_json::from_slice(&output.stdout).context("invalid PR metadata from gh")
    }

    pub(crate) fn resolve(
        &self,
        references: &[PrReference],
        paths: &AppPaths,
        storage: &Storage,
    ) -> Result<ResolvedWorkItem> {
        if references.is_empty() {
            bail!("at least one PR is required");
        }
        let references = canonical_references(references);
        let metadata = references
            .iter()
            .map(|reference| self.metadata(reference).map(|meta| (reference, meta)))
            .collect::<Result<Vec<_>>>()?;
        let canonical_urls = references
            .iter()
            .map(PrReference::canonical_url)
            .collect::<Vec<_>>();
        let stable_item_id = stable_id("work", &canonical_urls.join("\n"));
        let existing = storage.work_item_by_id(&stable_item_id)?;
        let timestamp = now();
        let item_id = existing
            .as_ref()
            .map(|item| item.id.clone())
            .unwrap_or(stable_item_id);
        let session_root = paths.roots.join(&item_id);
        let item = WorkItem {
            id: item_id.clone(),
            name: if references.len() == 1 {
                format!("{}#{}", references[0].repo, references[0].number)
            } else {
                format!("{} PRs", references.len())
            },
            workspace_root: session_root.clone(),
            created_at: existing
                .as_ref()
                .map(|item| item.created_at.clone())
                .unwrap_or_else(|| timestamp.clone()),
            updated_at: timestamp.clone(),
            last_opened_at: Some(timestamp),
        };
        storage.upsert_work_item(&item)?;
        let mut provisional = existing.is_none().then(|| {
            ProvisionalRemoteWorkItem::new(storage, item.id.clone(), session_root.clone())
        });
        fs::create_dir_all(&session_root)?;

        let metadata_for_context = metadata
            .iter()
            .map(|(_, metadata)| (*metadata).clone())
            .collect::<Vec<_>>();
        let mut repos = metadata
            .into_iter()
            .map(|(reference, metadata)| {
                self.resolve_repo(reference, metadata, &item, paths, storage)
            })
            .collect::<Result<Vec<_>>>()?;
        repos.sort_by(|left, right| {
            right
                .record
                .last_activity_at
                .cmp(&left.record.last_activity_at)
        });
        for repo in &repos {
            ensure_link(
                &session_root.join(remote_session_link_name(repo)),
                repo.version.worktree_path.as_ref(),
            )?;
        }
        write_remote_metadata(&session_root, &metadata_for_context)?;
        if let [metadata] = metadata_for_context.as_slice() {
            storage.upsert_context(&ReviewContext {
                work_item_id: item.id.clone(),
                title: metadata.title.clone(),
                what: metadata.body.clone(),
                why: String::new(),
                how: String::new(),
                considerations: String::new(),
                alternatives: String::new(),
                source: "remote_pr".into(),
                attached_to_session: false,
                delivery_state: DeliveryState::Draft,
            })?;
        }
        if let Some(provisional) = &mut provisional {
            provisional.commit();
        }
        Ok(ResolvedWorkItem {
            item,
            repos,
            session_root,
        })
    }

    fn resolve_repo(
        &self,
        reference: &PrReference,
        metadata: PrMetadata,
        item: &WorkItem,
        paths: &AppPaths,
        storage: &Storage,
    ) -> Result<ReviewRepo> {
        let cache = paths.prs.join(reference.cache_relative_path());
        let bare = cache.join("repo.git");
        fs::create_dir_all(&cache)?;
        self.ensure_bare_clone(reference, &bare)?;

        let head_ref = format!("refs/rq-tui/pr/{}/head", reference.number);
        let base_ref = format!("refs/rq-tui/pr/{}/base", reference.number);
        let fetch_specs = [
            format!("+refs/pull/{}/head:{head_ref}", reference.number),
            format!("+refs/heads/{}:{base_ref}", metadata.base_ref_name),
        ];
        let output = self.runner.output(
            Command::new("git")
                .arg("--git-dir")
                .arg(&bare)
                .args(["fetch", "--prune", "origin"])
                .args(fetch_specs),
        )?;
        ensure_success("git fetch", &output)?;
        let head_sha = self.git_dir_stdout(&bare, ["rev-parse", &head_ref])?;
        let base_sha = self.git_dir_stdout(&bare, ["rev-parse", &base_ref])?;

        let repo_id = stable_id(
            "repo",
            &format!("{}:{}", item.id, reference.canonical_url()),
        );
        let record = Repo {
            id: repo_id.clone(),
            work_item_id: item.id.clone(),
            name: reference.repo.clone(),
            path: bare.clone(),
            remote_pr_url: Some(reference.canonical_url()),
            pr_meta_json: Some(serde_json::to_string(&metadata)?),
            base_branch: Some(metadata.base_ref_name.clone()),
            base_branch_source: BaseBranchSource::Auto,
            last_activity_at: Some(metadata.updated_at.clone()),
        };
        storage.upsert_repo(&record)?;

        let existing = storage.latest_remote_version(&repo_id)?;
        let created_new_version = !existing
            .as_ref()
            .is_some_and(|version| version.head_sha == head_sha);
        let version = if !created_new_version {
            existing.clone().expect("checked as present")
        } else {
            let version_num = existing
                .as_ref()
                .map(|version| version.version_num + 1)
                .unwrap_or(1);
            let id = format!("{repo_id}:v{version_num}");
            let worktree = remote_worktree_path(&cache, &repo_id, version_num);
            self.add_worktree(&bare, &worktree, &head_sha)?;
            let version = Version {
                id,
                repo_id: repo_id.clone(),
                version_num,
                kind: VersionKind::Remote,
                created_at: now(),
                head_sha: head_sha.clone(),
                worktree_path: Some(worktree),
                last_opened_at: existing.is_none().then(now),
            };
            storage.upsert_version(&version)?;
            version
        };
        if created_new_version {
            if let Some(previous) = existing.as_ref() {
                self.carry_forward_annotations(storage, &bare, &base_sha, previous, &version)?;
            }
        }
        if !created_new_version || existing.is_none() {
            storage.mark_version_opened(&version.id)?;
        }
        let worktree = version
            .worktree_path
            .as_ref()
            .context("remote version has no worktree")?;
        let merge_base = self.git_stdout(worktree, ["merge-base", &base_sha, &head_sha])?;
        let raw = self.git_stdout(
            worktree,
            [
                "diff",
                "--find-renames",
                "--no-ext-diff",
                "--unified=6",
                "--no-color",
                &merge_base,
                &head_sha,
                "--",
            ],
        )?;
        Ok(ReviewRepo {
            record,
            version,
            diff: parse_unified(&raw)?,
        })
    }

    fn carry_forward_annotations(
        &self,
        storage: &Storage,
        bare: &Path,
        base_sha: &str,
        previous: &Version,
        current: &Version,
    ) -> Result<()> {
        let current_worktree = current
            .worktree_path
            .as_ref()
            .context("new remote version has no worktree")?;
        let renames = self.git_dir_stdout(
            bare,
            [
                "diff",
                "--name-status",
                "--find-renames",
                &previous.head_sha,
                &current.head_sha,
            ],
        )?;
        let rename_map = renames
            .lines()
            .filter_map(|line| {
                let fields = line.split('\t').collect::<Vec<_>>();
                (fields.len() == 3 && fields[0].starts_with('R'))
                    .then(|| (PathBuf::from(fields[1]), PathBuf::from(fields[2])))
            })
            .collect::<std::collections::HashMap<_, _>>();

        for (mut annotation, previous_placement) in storage.annotations_for_version(&previous.id)? {
            let previous_path = annotation.file_path.clone();
            let renamed_to = rename_map.get(&annotation.file_path);
            if renamed_to.is_some() {
                follow_rename(storage, &mut annotation, renamed_to.map(PathBuf::as_path))?;
            }
            let content = match previous_placement.side {
                crate::domain::AnchorSide::Old => {
                    let object = format!("{base_sha}:{}", previous_path.display());
                    self.git_dir_stdout(bare, ["show", object.as_str()])
                        .unwrap_or_default()
                }
                crate::domain::AnchorSide::New => {
                    fs::read_to_string(current_worktree.join(&annotation.file_path))
                        .unwrap_or_default()
                }
            };
            storage.upsert_placement(&reanchor(
                &annotation,
                &previous_placement,
                &current.id,
                &content,
            ))?;
        }
        Ok(())
    }

    fn ensure_bare_clone(&self, reference: &PrReference, bare: &Path) -> Result<()> {
        let clone_url = format!(
            "https://github.com/{}/{}.git",
            reference.owner, reference.repo
        );
        if bare.exists() {
            let output = self.runner.output(
                Command::new("git")
                    .arg("--git-dir")
                    .arg(bare)
                    .args(["remote", "set-url", "origin", &clone_url]),
            )?;
            return ensure_success("git remote set-url", &output);
        }
        if let Some(parent) = bare.parent() {
            fs::create_dir_all(parent)?;
        }
        let output = self.runner.output(
            Command::new("git")
                .args(["clone", "--bare", &clone_url])
                .arg(bare),
        )?;
        ensure_success("git clone --bare", &output)
    }

    fn add_worktree(&self, bare: &Path, worktree: &Path, head_sha: &str) -> Result<()> {
        if worktree.exists() {
            return Ok(());
        }
        if let Some(parent) = worktree.parent() {
            fs::create_dir_all(parent)?;
        }
        let output = self.runner.output(
            Command::new("git")
                .arg("--git-dir")
                .arg(bare)
                .args(["worktree", "add", "--detach"])
                .arg(worktree)
                .arg(head_sha),
        )?;
        ensure_success("git worktree add", &output)
    }

    fn git_dir_stdout<const N: usize>(&self, git_dir: &Path, args: [&str; N]) -> Result<String> {
        let output = self
            .runner
            .output(Command::new("git").arg("--git-dir").arg(git_dir).args(args))?;
        ensure_success("git", &output)?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn git_stdout<const N: usize>(&self, repo: &Path, args: [&str; N]) -> Result<String> {
        let output = self
            .runner
            .output(Command::new("git").arg("-C").arg(repo).args(args))?;
        ensure_success("git", &output)?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

struct ProvisionalRemoteWorkItem<'a> {
    storage: &'a Storage,
    id: String,
    session_root: PathBuf,
    committed: bool,
}

impl<'a> ProvisionalRemoteWorkItem<'a> {
    fn new(storage: &'a Storage, id: String, session_root: PathBuf) -> Self {
        Self {
            storage,
            id,
            session_root,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for ProvisionalRemoteWorkItem<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.storage.delete_work_item(&self.id).ok();
        fs::remove_dir_all(&self.session_root).ok();
    }
}

fn canonical_references(references: &[PrReference]) -> Vec<PrReference> {
    let mut references = references.to_vec();
    references.sort_by_key(PrReference::canonical_url);
    references.dedup_by(|left, right| left.canonical_url() == right.canonical_url());
    references
}

fn remote_session_link_name(repo: &ReviewRepo) -> String {
    repo.record
        .remote_pr_url
        .as_deref()
        .and_then(|url| PrReference::parse(url).ok())
        .map(|reference| format!("{}--{}", reference.owner, reference.repo))
        .unwrap_or_else(|| repo.record.name.clone())
}

fn remote_worktree_path(cache: &Path, repo_id: &str, version_num: i64) -> PathBuf {
    cache
        .join("worktrees")
        .join(repo_id)
        .join(format!("v{version_num}"))
}

fn write_remote_metadata(root: &Path, metadata: &[PrMetadata]) -> Result<()> {
    let directory = root.join(".rq-tui").join("metadata");
    fs::create_dir_all(&directory)?;
    for entry in metadata {
        let name = entry.url.split('/').rev().nth(2).unwrap_or("pull-request");
        fs::write(
            directory.join(format!("{name}-{}.md", entry.number)),
            format!(
                "# {}\n\n{}\n\n- URL: {}\n- State: {}\n- Base: {}\n- Head: {}\n",
                entry.title,
                entry.body,
                entry.url,
                entry.state,
                entry.base_ref_name,
                entry.head_ref_oid,
            ),
        )?;
    }
    Ok(())
}

fn ensure_success(operation: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "{operation} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn stable_id(namespace: &str, value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace.as_bytes());
    digest.update([0]);
    digest.update(value.as_bytes());
    let hex = format!("{:x}", digest.finalize());
    format!("{namespace}-{}", &hex[..24])
}

fn ensure_link(link: &Path, target: Option<&PathBuf>) -> Result<()> {
    let target = target.context("remote version has no worktree")?;
    if link.symlink_metadata().is_ok() {
        if std::fs::read_link(link).is_ok_and(|existing| existing == target.as_path()) {
            return Ok(());
        }
        std::fs::remove_file(link)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        canonical_references, remote_session_link_name, remote_worktree_path, PrMetadata,
        PrReference, ProvisionalRemoteWorkItem,
    };
    use crate::diff::DiffSet;
    use crate::domain::{BaseBranchSource, Repo, Version, VersionKind, WorkItem};
    use crate::storage::Storage;
    use crate::work_item::ReviewRepo;
    use tempfile::tempdir;

    #[test]
    fn parses_supported_pr_reference_forms() {
        let expected = PrReference {
            owner: "acme".into(),
            repo: "api".into(),
            number: 42,
        };
        for value in [
            "acme/api#42",
            "github.com/acme/api#42",
            "https://github.com/acme/api/pull/42",
        ] {
            assert_eq!(PrReference::parse(value).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_incomplete_pr_references() {
        assert!(PrReference::parse("api#42").is_err());
        assert!(PrReference::parse("acme/api").is_err());
        assert!(PrReference::parse("../api#42").is_err());
        assert!(PrReference::parse("acme/../pull/42").is_err());
        assert!(PrReference::parse("acme/api#0").is_err());
    }

    #[test]
    fn cache_is_namespaced_by_owner_repository_and_pr_number() {
        let reference = PrReference::parse("https://github.com/acme/api/pull/42").unwrap();
        assert_eq!(
            reference.cache_relative_path(),
            PathBuf::from("acme/api/pr-42")
        );
    }

    #[test]
    fn github_updated_at_is_preserved_for_remote_activity_ordering() {
        let metadata: PrMetadata = serde_json::from_str(
            r#"{
                "number": 42,
                "title": "Fix ordering",
                "body": "",
                "url": "https://github.com/acme/api/pull/42",
                "state": "OPEN",
                "mergedAt": null,
                "updatedAt": "2026-07-29T10:11:12Z",
                "baseRefName": "main",
                "headRefOid": "abc123"
            }"#,
        )
        .unwrap();

        assert_eq!(metadata.updated_at, "2026-07-29T10:11:12Z");
    }

    #[test]
    fn failed_new_remote_resolution_removes_its_provisional_history() {
        let temp = tempdir().unwrap();
        let storage = Storage::open(&temp.path().join("review.db")).unwrap();
        let session_root = temp.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        let item = WorkItem {
            id: "provisional".into(),
            name: "provisional".into(),
            workspace_root: session_root.clone(),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_opened_at: Some("now".into()),
        };
        storage.upsert_work_item(&item).unwrap();

        drop(ProvisionalRemoteWorkItem::new(
            &storage,
            item.id.clone(),
            session_root.clone(),
        ));

        assert!(storage.work_item_by_id(&item.id).unwrap().is_none());
        assert!(!session_root.exists());
    }

    #[test]
    fn canonical_pr_sets_are_order_independent_and_deduplicated() {
        let first = PrReference::parse("other/api#7").unwrap();
        let second = PrReference::parse("acme/api#42").unwrap();
        let ordered = canonical_references(&[first.clone(), second.clone(), first]);

        assert_eq!(
            ordered,
            vec![second, PrReference::parse("other/api#7").unwrap()]
        );
    }

    #[test]
    fn same_named_remote_repositories_get_distinct_session_links() {
        let repo = |id: &str, url: &str| ReviewRepo {
            record: Repo {
                id: id.into(),
                work_item_id: "work".into(),
                name: "api".into(),
                path: PathBuf::from(format!("/{id}/repo.git")),
                remote_pr_url: Some(url.into()),
                pr_meta_json: None,
                base_branch: Some("main".into()),
                base_branch_source: BaseBranchSource::Auto,
                last_activity_at: None,
            },
            version: Version {
                id: format!("{id}:v1"),
                repo_id: id.into(),
                version_num: 1,
                kind: VersionKind::Remote,
                created_at: "1".into(),
                head_sha: "head".into(),
                worktree_path: Some(PathBuf::from(format!("/{id}/v1"))),
                last_opened_at: Some("1".into()),
            },
            diff: DiffSet::default(),
        };
        let acme = repo("acme", "https://github.com/acme/api/pull/42");
        let other = repo("other", "https://github.com/other/api/pull/7");

        assert_eq!(remote_session_link_name(&acme), "acme--api");
        assert_eq!(remote_session_link_name(&other), "other--api");
    }

    #[test]
    fn remote_worktrees_are_scoped_to_the_owning_repo_record() {
        let cache = Path::new("/cache/acme_api_42");
        assert_ne!(
            remote_worktree_path(cache, "work-a-repo", 1),
            remote_worktree_path(cache, "work-b-repo", 1)
        );
    }
}

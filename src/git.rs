use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

use crate::process_control::output_with_timeout;

const LOCAL_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub(crate) struct LocalRepoState {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) base_branch: String,
    pub(crate) merge_base: String,
    pub(crate) raw_diff: String,
    pub(crate) last_activity_at: String,
}

pub(crate) trait CommandRunner {
    fn output(&self, program: &OsStr, args: &[OsString]) -> Result<Output>;
}

#[derive(Default)]
pub(crate) struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn output(&self, program: &OsStr, args: &[OsString]) -> Result<Output> {
        let operation = std::iter::once(program.to_string_lossy().into_owned())
            .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
            .collect::<Vec<_>>()
            .join(" ");
        output_with_timeout(
            Command::new(program).args(args),
            &operation,
            LOCAL_COMMAND_TIMEOUT,
        )
    }
}

pub(crate) struct Git<R = SystemCommandRunner> {
    runner: R,
}

impl Default for Git<SystemCommandRunner> {
    fn default() -> Self {
        Self {
            runner: SystemCommandRunner,
        }
    }
}

impl<R: CommandRunner> Git<R> {
    pub(crate) fn discover_repositories(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", root.display()))?;
        if self.repository_root(&root).is_ok_and(|repo| repo == root) {
            return Ok(vec![root]);
        }

        let mut repositories = Vec::new();
        for entry in WalkDir::new(&root)
            .min_depth(1)
            .max_depth(4)
            .follow_links(false)
            .into_iter()
            .filter_entry(should_descend)
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_dir())
        {
            let path = entry.path();
            if path.join(".git").exists() && self.repository_root(path).is_ok() {
                repositories.push(path.to_path_buf());
            }
        }
        repositories.sort();
        repositories.dedup();
        Ok(repositories)
    }

    pub(crate) fn inspect_local_repo(
        &self,
        path: &Path,
        base_override: Option<&str>,
    ) -> Result<Option<LocalRepoState>> {
        let path = self.repository_root(path)?;
        let base_branch = match base_override {
            Some(base) => base.to_owned(),
            None => self.detect_default_branch(&path)?,
        };
        let merge_base = self.git_stdout(&path, ["merge-base", "HEAD", &base_branch])?;
        // Materialize the review diff once. The old quiet probe scanned every
        // changed repository and then immediately ran the full diff again.
        // Clean repositories still produce an empty string, while changed
        // repositories carry their already-computed payload into resolution.
        let raw_diff = self.diff(&path, &merge_base, 6)?;
        if raw_diff.is_empty() {
            return Ok(None);
        }
        let last_activity_at = self.last_tracked_activity(&path)?;
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("repository")
            .to_owned();
        Ok(Some(LocalRepoState {
            path,
            name,
            base_branch,
            merge_base,
            raw_diff,
            last_activity_at,
        }))
    }

    pub(crate) fn diff(&self, repo: &Path, merge_base: &str, context: usize) -> Result<String> {
        self.git_stdout(
            repo,
            [
                "diff",
                "--find-renames",
                "--no-ext-diff",
                &format!("--unified={context}"),
                "--no-color",
                merge_base,
                "--",
            ],
        )
    }

    pub(crate) fn merge_base(&self, repo: &Path, left: &str, right: &str) -> Result<String> {
        self.git_stdout(repo, ["merge-base", left, right])
    }

    pub(crate) fn diff_commits(
        &self,
        repo: &Path,
        base: &str,
        head: &str,
        context: usize,
    ) -> Result<String> {
        self.git_stdout(
            repo,
            [
                "diff",
                "--find-renames",
                "--no-ext-diff",
                &format!("--unified={context}"),
                "--no-color",
                base,
                head,
                "--",
            ],
        )
    }

    pub(crate) fn snapshot(&self, repo: &Path, version_id: &str) -> Result<String> {
        let created = self.git_stdout(repo, ["stash", "create", "rq-tui snapshot"])?;
        let commit = if created.is_empty() {
            self.git_stdout(repo, ["rev-parse", "HEAD"])?
        } else {
            created
        };
        let reference = snapshot_ref(version_id);
        self.git_status(repo, ["update-ref", &reference, &commit])?;
        Ok(commit)
    }

    pub(crate) fn tree_id(&self, repo: &Path, commit: &str) -> Result<String> {
        self.git_stdout(repo, ["rev-parse", &format!("{commit}^{{tree}}")])
    }

    pub(crate) fn materialize_worktree(
        &self,
        repo: &Path,
        reference: &str,
        destination: &Path,
    ) -> Result<()> {
        if destination.exists() {
            return Ok(());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        self.git_status(
            repo,
            [
                "worktree",
                "add",
                "--detach",
                destination
                    .to_str()
                    .context("worktree path is not valid UTF-8")?,
                reference,
            ],
        )
    }

    pub(crate) fn remove_worktree(&self, repo: &Path, destination: &Path) -> Result<()> {
        if !destination.exists() {
            return Ok(());
        }
        self.git_status(
            repo,
            [
                "worktree",
                "remove",
                "--force",
                destination
                    .to_str()
                    .context("worktree path is not valid UTF-8")?,
            ],
        )
    }

    pub(crate) fn delete_snapshot_ref(&self, repo: &Path, version_id: &str) -> Result<()> {
        let reference = snapshot_ref(version_id);
        self.git_status(repo, ["update-ref", "-d", &reference])
    }

    pub(crate) fn detect_default_branch(&self, repo: &Path) -> Result<String> {
        if let Ok(symbolic) = self.git_stdout(
            repo,
            ["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        ) {
            if let Some(branch) = symbolic.strip_prefix("origin/") {
                return Ok(format!("origin/{branch}"));
            }
        }

        // Keep local review startup strictly local. `git remote show origin`
        // refreshes remote metadata by default and can block the TUI launch on
        // DNS, authentication, or an unavailable network. The symbolic ref
        // above and these common refs cover normal clones without any I/O
        // outside the repository; uncommon layouts can use `review --base`.
        for candidate in ["origin/main", "main", "origin/master", "master"] {
            if self
                .git_status(repo, ["rev-parse", "--verify", "--quiet", candidate])
                .is_ok()
            {
                return Ok(candidate.to_owned());
            }
        }
        bail!("cannot detect the default branch for {}", repo.display())
    }

    pub(crate) fn repository_root(&self, path: &Path) -> Result<PathBuf> {
        Ok(PathBuf::from(
            self.git_stdout(path, ["rev-parse", "--show-toplevel"])?,
        ))
    }

    fn last_tracked_activity(&self, repo: &Path) -> Result<String> {
        let tip = self
            .git_stdout(repo, ["show", "-s", "--format=%ct", "HEAD"])?
            .parse::<i64>()
            .context("invalid commit timestamp")?;
        let mut latest = tip;
        let output = self.git_output(
            repo,
            ["status", "--porcelain=v1", "-z", "--untracked-files=no"],
        )?;
        ensure_success("git status", repo, &output)?;
        let fields = output.stdout.split(|byte| *byte == 0);
        for field in fields.filter(|field| field.len() >= 4) {
            let relative = String::from_utf8_lossy(&field[3..]).into_owned();
            let path = repo.join(relative);
            let modified = fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|timestamp| timestamp.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs() as i64);
            latest = latest.max(modified.unwrap_or(tip));
        }
        let timestamp =
            DateTime::<Utc>::from_timestamp(latest, 0).context("invalid activity timestamp")?;
        Ok(timestamp.to_rfc3339())
    }

    fn git_stdout<const N: usize>(&self, repo: &Path, args: [&str; N]) -> Result<String> {
        let output = self.git_output(repo, args)?;
        ensure_success("git", repo, &output)?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn git_status<const N: usize>(&self, repo: &Path, args: [&str; N]) -> Result<()> {
        let output = self.git_output(repo, args)?;
        ensure_success("git", repo, &output)
    }

    fn git_output<const N: usize>(&self, repo: &Path, args: [&str; N]) -> Result<Output> {
        let mut all_args = vec![OsString::from("-C"), repo.as_os_str().to_owned()];
        all_args.extend(args.into_iter().map(OsString::from));
        self.runner.output(OsStr::new("git"), &all_args)
    }
}

fn should_descend(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    !matches!(
        name.as_ref(),
        ".git" | "target" | "node_modules" | "bazel-bin" | "bazel-out" | "bazel-testlogs"
    )
}

fn snapshot_ref(version_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(version_id.as_bytes());
    format!("refs/rq-tui/snapshots/{:x}", digest.finalize())
}

fn ensure_success(operation: &str, repo: &Path, output: &Output) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "{operation} failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{Command, ExitStatus, Output};
    use std::sync::Mutex;

    use tempfile::tempdir;

    use super::{snapshot_ref, CommandRunner, Git};

    #[derive(Default)]
    struct LocalOnlyRunner {
        commands: Mutex<Vec<Vec<String>>>,
    }

    impl CommandRunner for LocalOnlyRunner {
        fn output(&self, _program: &OsStr, args: &[OsString]) -> anyhow::Result<Output> {
            let command = args
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            self.commands.lock().unwrap().push(command.clone());
            assert!(
                !command
                    .windows(3)
                    .any(|parts| parts == ["remote", "show", "origin"]),
                "default branch detection attempted network-dependent remote inspection"
            );
            let symbolic = command
                .windows(3)
                .any(|parts| parts == ["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
            let origin_main = command
                .windows(4)
                .any(|parts| parts == ["rev-parse", "--verify", "--quiet", "origin/main"]);
            Ok(Output {
                status: ExitStatus::from_raw(if origin_main { 0 } else { 1 }),
                stdout: if symbolic {
                    Vec::new()
                } else {
                    b"deadbeef\n".to_vec()
                },
                stderr: Vec::new(),
            })
        }
    }

    fn git(path: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git_output(path: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn detects_local_changes_and_activity() {
        let temp = tempdir().unwrap();
        git(temp.path(), &["init", "-b", "main"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        fs::write(temp.path().join("demo.rs"), "fn main() {}\n").unwrap();
        git(temp.path(), &["add", "demo.rs"]);
        git(temp.path(), &["commit", "-m", "initial"]);
        fs::write(
            temp.path().join("demo.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .unwrap();

        let state = Git::default()
            .inspect_local_repo(temp.path(), Some("main"))
            .unwrap()
            .unwrap();
        assert_eq!(
            state.name,
            temp.path().file_name().unwrap().to_string_lossy()
        );
        assert_eq!(state.base_branch, "main");
        assert!(state.raw_diff.contains("println!"));
    }

    #[test]
    fn default_branch_detection_never_queries_the_remote() {
        let git = Git {
            runner: LocalOnlyRunner::default(),
        };
        assert_eq!(
            git.detect_default_branch(std::path::Path::new("/local/repo"))
                .unwrap(),
            "origin/main"
        );
        assert_eq!(git.runner.commands.lock().unwrap().len(), 2);
    }

    #[test]
    fn snapshot_preserves_head_and_index_and_pins_the_worktree() {
        let temp = tempdir().unwrap();
        git(temp.path(), &["init", "-b", "main"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        fs::write(temp.path().join("demo.rs"), "one\n").unwrap();
        git(temp.path(), &["add", "demo.rs"]);
        git(temp.path(), &["commit", "-m", "initial"]);
        fs::write(temp.path().join("demo.rs"), "two\n").unwrap();
        git(temp.path(), &["add", "demo.rs"]);
        fs::write(temp.path().join("demo.rs"), "three\n").unwrap();

        let before_head = git_output(temp.path(), &["rev-parse", "HEAD"]);
        let before_index = git_output(temp.path(), &["diff", "--cached"]);
        let commit = Git::default()
            .snapshot(temp.path(), "snapshot-test")
            .unwrap();

        assert_eq!(before_head, git_output(temp.path(), &["rev-parse", "HEAD"]));
        assert_eq!(before_index, git_output(temp.path(), &["diff", "--cached"]));
        assert_eq!(
            git_output(temp.path(), &["show", &format!("{commit}:demo.rs")]),
            "three\n"
        );
        assert_eq!(
            git_output(temp.path(), &["rev-parse", &snapshot_ref("snapshot-test")]).trim(),
            commit
        );
        Git::default()
            .snapshot(temp.path(), "repo:working-tree:snapshot:1")
            .unwrap();
        assert!(!git_output(
            temp.path(),
            &["rev-parse", &snapshot_ref("repo:working-tree:snapshot:1")]
        )
        .trim()
        .is_empty());
    }
}

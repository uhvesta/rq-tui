use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use rq_tui_domain::{
    AnchorSide, Annotation, AnnotationKind, AskMessage, BaseBranchSource, DeliveryState,
    EphemeralSessionRecord, PendingChat, Placement, Repo, ReviewContext, SessionRecord, Version,
    VersionKind, WorkItem,
};

pub const MIGRATION_1: &str = include_str!("../migrations/0001_initial.sql");
pub const MIGRATION_2: &str = include_str!("../migrations/0002_placement_side.sql");
pub const MIGRATION_3: &str = include_str!("../migrations/0003_chat_outbox.sql");
pub const MIGRATION_4: &str = include_str!("../migrations/0004_ephemeral_sessions.sql");
pub const MIGRATION_5: &str = include_str!("../migrations/0005_prune_journal.sql");
pub const MIGRATION_6: &str = include_str!("../migrations/0006_outbox_metadata.sql");
pub const MIGRATION_7: &str = include_str!("../migrations/0007_session_ephemeral.sql");
pub const MIGRATION_8: &str = include_str!("../migrations/0008_rev_question_sessions.sql");

pub struct Storage {
    connection: Connection,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ReviewHistoryItem {
    pub id: String,
    pub name: String,
    pub last_opened_at: String,
    pub versions: usize,
    pub annotations: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevQuestionSession {
    pub annotation_id: String,
    pub session_id: String,
    pub model_id: String,
    pub reasoning_effort: Option<String>,
    pub context_tier: Option<String>,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneOperation {
    pub operation_id: String,
    pub work_item_id: String,
    pub export_first: bool,
    pub phase: String,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneTarget {
    pub operation_id: String,
    pub target_key: String,
    pub kind: String,
    pub session_id: Option<String>,
    pub side_operation_id: Option<String>,
    pub parent_id: Option<String>,
    pub state: String,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// The canonical persisted representation of a SIDE session.
///
/// This is intentionally separate from `domain::SessionRecord`: the latter is
/// used by existing MAIN/fork integration call sites that predate the
/// `sessions.ephemeral` column. Keeping that input type stable lets storage
/// expose the new schema contract without requiring a runtime integration
/// change in this migration-only step.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct SideSessionRecord {
    pub id: String,
    pub work_item_id: String,
    pub parent_id: Option<String>,
    pub active: bool,
    pub created_at: String,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("cannot create database directory {}", parent.display())
            })?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        Self::configure(&connection)?;
        let storage = Self {
            connection,
            path: path.to_path_buf(),
        };
        storage.migrate()?;
        Ok(storage)
    }

    pub fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        Self::configure(&connection)?;
        let storage = Self {
            connection,
            path: PathBuf::from(":memory:"),
        };
        storage.migrate()?;
        Ok(storage)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_busy_timeout(&self, timeout: Duration) -> Result<()> {
        self.connection
            .busy_timeout(timeout)
            .context("cannot configure SQLite busy timeout")
    }

    pub fn set_query_only_for_testing(&self, enabled: bool) -> Result<()> {
        self.connection
            .pragma_update(None, "query_only", enabled)
            .context("cannot change SQLite query-only mode")?;
        Ok(())
    }

    fn configure(connection: &Connection) -> Result<()> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let mut last_error = None;
        for _ in 0..50 {
            match connection.pragma_update(None, "journal_mode", "WAL") {
                Ok(()) => return Ok(()),
                Err(error) => {
                    last_error = Some(error);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        Err(last_error.expect("WAL retry loop always records an error"))
            .context("cannot enable SQLite WAL mode")
    }

    fn migrate(&self) -> Result<()> {
        // Reserve the database before creating or checking the ledger so
        // concurrent first launches cannot race any part of initialization.
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
            .context("cannot reserve database for migrations")?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );",
        )?;
        for (version, sql) in [
            (1, MIGRATION_1),
            (2, MIGRATION_2),
            (3, MIGRATION_3),
            (4, MIGRATION_4),
            (5, MIGRATION_5),
            (6, MIGRATION_6),
            (7, MIGRATION_7),
            (8, MIGRATION_8),
        ] {
            let applied = tx
                .query_row(
                    "SELECT 1 FROM schema_migrations WHERE version = ?1",
                    [version],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !applied {
                tx.execute_batch(sql)?;
                tx.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
                    params![version, now()],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_work_item(&self, item: &WorkItem) -> Result<()> {
        self.connection.execute(
            "INSERT INTO work_items(
                id, name, workspace_root, created_at, updated_at, last_opened_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                workspace_root = excluded.workspace_root,
                updated_at = excluded.updated_at,
                last_opened_at = excluded.last_opened_at",
            params![
                item.id,
                item.name,
                item.workspace_root.to_string_lossy(),
                item.created_at,
                item.updated_at,
                item.last_opened_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_work_items(&self) -> Result<Vec<WorkItem>> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, workspace_root, created_at, updated_at, last_opened_at
             FROM work_items
             ORDER BY COALESCE(last_opened_at, created_at) DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(WorkItem {
                id: row.get(0)?,
                name: row.get(1)?,
                workspace_root: PathBuf::from(row.get::<_, String>(2)?),
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                last_opened_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn review_history(&self) -> Result<Vec<ReviewHistoryItem>> {
        let mut statement = self.connection.prepare(
            "SELECT w.id, w.name, COALESCE(w.last_opened_at, w.created_at),
                    COUNT(DISTINCT v.id), COUNT(DISTINCT a.id)
             FROM work_items w
             LEFT JOIN repos r ON r.work_item_id = w.id
             LEFT JOIN versions v ON v.repo_id = r.id
             LEFT JOIN annotations a ON a.repo_id = r.id
             WHERE w.last_opened_at IS NOT NULL
                OR EXISTS (
                    SELECT 1 FROM annotations a2
                    JOIN repos r2 ON r2.id = a2.repo_id
                    WHERE r2.work_item_id = w.id
                )
             GROUP BY w.id
             ORDER BY COALESCE(w.last_opened_at, w.created_at), w.name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ReviewHistoryItem {
                id: row.get(0)?,
                name: row.get(1)?,
                last_opened_at: row.get(2)?,
                versions: row.get::<_, i64>(3)? as usize,
                annotations: row.get::<_, i64>(4)? as usize,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_work_item(&self, work_item_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM work_items WHERE id = ?1", [work_item_id])?;
        Ok(())
    }

    /// Remove review artifacts while retaining the workspace and repository
    /// metadata needed to open it again immediately.
    pub fn clear_review_history(&self, work_item_id: &str) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM annotations
             WHERE repo_id IN (SELECT id FROM repos WHERE work_item_id = ?1)",
            [work_item_id],
        )?;
        tx.execute(
            "DELETE FROM chat_outbox WHERE work_item_id = ?1",
            [work_item_id],
        )?;
        tx.execute(
            "DELETE FROM contexts WHERE work_item_id = ?1",
            [work_item_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_rev_question_session(&self, session: &RevQuestionSession) -> Result<()> {
        self.connection.execute(
            "INSERT INTO rev_question_sessions(
                annotation_id, session_id, model_id, reasoning_effort,
                context_tier, state, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(annotation_id) DO UPDATE SET
                session_id = excluded.session_id,
                model_id = excluded.model_id,
                reasoning_effort = excluded.reasoning_effort,
                context_tier = excluded.context_tier,
                state = excluded.state,
                updated_at = excluded.updated_at",
            params![
                session.annotation_id,
                session.session_id,
                session.model_id,
                session.reasoning_effort,
                session.context_tier,
                session.state,
                session.created_at,
                session.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn rev_question_session(&self, annotation_id: &str) -> Result<Option<RevQuestionSession>> {
        Ok(self
            .connection
            .query_row(
                "SELECT annotation_id, session_id, model_id, reasoning_effort,
                        context_tier, state, created_at, updated_at
                 FROM rev_question_sessions WHERE annotation_id = ?1",
                [annotation_id],
                |row| {
                    Ok(RevQuestionSession {
                        annotation_id: row.get(0)?,
                        session_id: row.get(1)?,
                        model_id: row.get(2)?,
                        reasoning_effort: row.get(3)?,
                        context_tier: row.get(4)?,
                        state: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn work_item_by_root(&self, root: &Path) -> Result<Option<WorkItem>> {
        let root = root.to_string_lossy();
        Ok(self
            .connection
            .query_row(
                "SELECT id, name, workspace_root, created_at, updated_at, last_opened_at
                 FROM work_items WHERE workspace_root = ?1",
                [root.as_ref()],
                |row| {
                    Ok(WorkItem {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        workspace_root: PathBuf::from(row.get::<_, String>(2)?),
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                        last_opened_at: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn work_item_by_id(&self, id: &str) -> Result<Option<WorkItem>> {
        Ok(self
            .connection
            .query_row(
                "SELECT id, name, workspace_root, created_at, updated_at, last_opened_at
                 FROM work_items WHERE id = ?1",
                [id],
                |row| {
                    Ok(WorkItem {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        workspace_root: PathBuf::from(row.get::<_, String>(2)?),
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                        last_opened_at: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn upsert_repo(&self, repo: &Repo) -> Result<()> {
        self.connection.execute(
            "INSERT INTO repos(
                id, work_item_id, name, path, remote_pr_url, pr_meta_json,
                base_branch, base_branch_source, last_activity_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                work_item_id = excluded.work_item_id,
                name = excluded.name,
                path = excluded.path,
                remote_pr_url = excluded.remote_pr_url,
                pr_meta_json = excluded.pr_meta_json,
                base_branch = excluded.base_branch,
                base_branch_source = excluded.base_branch_source,
                last_activity_at = excluded.last_activity_at",
            params![
                repo.id,
                repo.work_item_id,
                repo.name,
                repo.path.to_string_lossy(),
                repo.remote_pr_url,
                repo.pr_meta_json,
                repo.base_branch,
                repo.base_branch_source.as_str(),
                repo.last_activity_at,
            ],
        )?;
        Ok(())
    }

    pub fn repos_for_work_item(&self, work_item_id: &str) -> Result<Vec<Repo>> {
        let mut statement = self.connection.prepare(
            "SELECT id, work_item_id, name, path, remote_pr_url, pr_meta_json,
                    base_branch, base_branch_source, last_activity_at
             FROM repos WHERE work_item_id = ?1
             ORDER BY last_activity_at DESC, name",
        )?;
        let rows = statement.query_map([work_item_id], |row| {
            let source: String = row.get(7)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                source,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;
        rows.map(|row| {
            let (
                id,
                work_item_id,
                name,
                path,
                remote_pr_url,
                pr_meta_json,
                base_branch,
                source,
                last_activity_at,
            ) = row?;
            Ok(Repo {
                id,
                work_item_id,
                name,
                path: PathBuf::from(path),
                remote_pr_url,
                pr_meta_json,
                base_branch,
                base_branch_source: BaseBranchSource::try_from(source.as_str())?,
                last_activity_at,
            })
        })
        .collect()
    }

    pub fn repo_path_is_shared(&self, path: &Path, excluding_work_item_id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM repos
                WHERE path = ?1 AND work_item_id != ?2
             )",
            params![path.to_string_lossy(), excluding_work_item_id],
            |row| row.get(0),
        )?)
    }

    pub fn worktree_path_is_shared(
        &self,
        path: &Path,
        excluding_work_item_id: &str,
    ) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(
                SELECT 1
                FROM versions v
                JOIN repos r ON r.id = v.repo_id
                WHERE v.worktree_path = ?1 AND r.work_item_id != ?2
             )",
            params![path.to_string_lossy(), excluding_work_item_id],
            |row| row.get(0),
        )?)
    }

    pub fn upsert_version(&self, version: &Version) -> Result<()> {
        self.connection.execute(
            "INSERT INTO versions(
                id, repo_id, version_num, kind, created_at, head_sha,
                worktree_path, last_opened_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                created_at = excluded.created_at,
                head_sha = excluded.head_sha,
                worktree_path = excluded.worktree_path,
                last_opened_at = excluded.last_opened_at",
            params![
                version.id,
                version.repo_id,
                version.version_num,
                version.kind.as_str(),
                version.created_at,
                version.head_sha,
                version
                    .worktree_path
                    .as_ref()
                    .map(|path| path.to_string_lossy()),
                version.last_opened_at,
            ],
        )?;
        Ok(())
    }

    pub fn versions_for_repo(&self, repo_id: &str) -> Result<Vec<Version>> {
        let mut statement = self.connection.prepare(
            "SELECT id, repo_id, version_num, kind, created_at, head_sha,
                    worktree_path, last_opened_at
             FROM versions WHERE repo_id = ?1
             ORDER BY version_num DESC, created_at DESC",
        )?;
        let rows = statement.query_map([repo_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        rows.map(|row| {
            let (
                id,
                repo_id,
                version_num,
                kind,
                created_at,
                head_sha,
                worktree_path,
                last_opened_at,
            ) = row?;
            Ok(Version {
                id,
                repo_id,
                version_num,
                kind: VersionKind::try_from(kind.as_str())?,
                created_at,
                head_sha,
                worktree_path: worktree_path.map(PathBuf::from),
                last_opened_at,
            })
        })
        .collect()
    }

    pub fn annotation_counts_for_version(&self, version_id: &str) -> Result<(usize, usize)> {
        let (asks, comments) = self.connection.query_row(
            "SELECT
                 SUM(CASE WHEN a.kind = 'ask' THEN 1 ELSE 0 END),
                 SUM(CASE WHEN a.kind = 'comment' THEN 1 ELSE 0 END)
             FROM placements p
             JOIN annotations a ON a.id = p.annotation_id
             WHERE p.version_id = ?1",
            [version_id],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(0) as usize,
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0) as usize,
                ))
            },
        )?;
        Ok((asks, comments))
    }

    pub fn latest_remote_version(&self, repo_id: &str) -> Result<Option<Version>> {
        let row = self
            .connection
            .query_row(
                "SELECT id, repo_id, version_num, kind, created_at, head_sha,
                        worktree_path, last_opened_at
                 FROM versions
                 WHERE repo_id = ?1 AND kind = 'remote'
                 ORDER BY version_num DESC LIMIT 1",
                [repo_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(
                id,
                repo_id,
                version_num,
                kind,
                created_at,
                head_sha,
                worktree_path,
                last_opened_at,
            )| {
                Ok(Version {
                    id,
                    repo_id,
                    version_num,
                    kind: VersionKind::try_from(kind.as_str())?,
                    created_at,
                    head_sha,
                    worktree_path: worktree_path.map(PathBuf::from),
                    last_opened_at,
                })
            },
        )
        .transpose()
    }

    pub fn mark_version_opened(&self, version_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE versions SET last_opened_at = ?2 WHERE id = ?1",
            params![version_id, now()],
        )?;
        Ok(())
    }

    pub fn unopened_remote_versions_older_than(
        &self,
        repo_id: &str,
        reviewed_version_num: i64,
    ) -> Result<Vec<Version>> {
        Ok(self
            .versions_for_repo(repo_id)?
            .into_iter()
            .filter(|version| {
                version.kind == VersionKind::Remote
                    && version.last_opened_at.is_none()
                    && version.version_num < reviewed_version_num
            })
            .collect())
    }

    pub fn delete_version(&self, version_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM versions WHERE id = ?1", [version_id])?;
        Ok(())
    }

    pub fn activate_session(&self, session: &SessionRecord) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        if let Some((work_item_id, ephemeral)) = tx
            .query_row(
                "SELECT work_item_id, ephemeral FROM sessions WHERE id = ?1",
                [&session.id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .optional()?
        {
            if work_item_id != session.work_item_id {
                anyhow::bail!(
                    "session {} already belongs to Work Item {}",
                    session.id,
                    work_item_id
                );
            }
            if ephemeral {
                anyhow::bail!(
                    "session {} is an ephemeral SIDE session; activate it with the SIDE API",
                    session.id
                );
            }
        }
        let pruning_operation = tx
            .query_row(
                "SELECT operation_id
                 FROM prune_operations
                 WHERE work_item_id = ?1",
                [&session.work_item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(operation_id) = pruning_operation {
            let timestamp = now();
            tx.execute(
                "INSERT OR IGNORE INTO prune_targets(
                    operation_id, target_key, kind, session_id, side_operation_id,
                    parent_id, state, last_error, created_at, updated_at
                 ) VALUES (?1, ?2, 'persistent', ?3, NULL, ?4, 'pending', NULL, ?5, ?5)",
                params![
                    operation_id,
                    format!("persistent:{}", session.id),
                    session.id,
                    session.parent_id,
                    timestamp
                ],
            )?;
            tx.commit()?;
            anyhow::bail!(
                "the Work Item is being pruned; the late Copilot session was captured for cleanup"
            );
        }
        tx.execute(
            "UPDATE sessions SET active = 0 WHERE work_item_id = ?1",
            [&session.work_item_id],
        )?;
        tx.execute(
            "INSERT INTO sessions(id, work_item_id, parent_id, active, ephemeral, created_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET active = 1, parent_id = excluded.parent_id
             WHERE sessions.ephemeral = 0",
            params![
                session.id,
                session.work_item_id,
                session.parent_id,
                0,
                session.created_at
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically promotes a remotely-created fork into persistent MAIN state
    /// and clears the durable creation intent while the same owner still holds
    /// the Work Item lease.
    pub fn activate_session_from_ephemeral(
        &self,
        session: &SessionRecord,
        operation_id: &str,
        owner_id: &str,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let may_activate = tx.query_row(
            "SELECT
                EXISTS (
                  SELECT 1 FROM ephemeral_session_leases
                  WHERE work_item_id = ?1 AND owner_id = ?2
                )
                AND NOT EXISTS (
                  SELECT 1 FROM prune_operations
                  WHERE work_item_id = ?1
                )
                AND EXISTS (
                  SELECT 1 FROM ephemeral_sessions
                  WHERE operation_id = ?3 AND owner_id = ?2
                )",
            params![session.work_item_id, owner_id, operation_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if !may_activate {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE sessions SET active = 0 WHERE work_item_id = ?1",
            [&session.work_item_id],
        )?;
        tx.execute(
            "INSERT INTO sessions(id, work_item_id, parent_id, active, ephemeral, created_at)
             VALUES (?1, ?2, ?3, 1, 0, ?4)
             ON CONFLICT(id) DO UPDATE SET
                parent_id = excluded.parent_id,
                active = 1,
                ephemeral = 0",
            params![
                session.id,
                session.work_item_id,
                session.parent_id,
                session.created_at
            ],
        )?;
        if tx.execute(
            "DELETE FROM ephemeral_sessions
             WHERE operation_id = ?1 AND owner_id = ?2",
            params![operation_id, owner_id],
        )? != 1
        {
            anyhow::bail!("MAIN fork creation intent disappeared during activation");
        }
        tx.commit()?;
        Ok(true)
    }

    /// Persists the canonical SIDE row. When `session.active` is true, the
    /// ownership switch happens in the same transaction as the upsert, so a
    /// reader can never observe two active sessions for this Work Item.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn persist_ephemeral_side_session(&self, session: &SideSessionRecord) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if let Some((work_item_id, ephemeral)) = tx
            .query_row(
                "SELECT work_item_id, ephemeral FROM sessions WHERE id = ?1",
                [&session.id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .optional()?
        {
            if work_item_id != session.work_item_id {
                anyhow::bail!(
                    "session {} already belongs to Work Item {}",
                    session.id,
                    work_item_id
                );
            }
            if !ephemeral {
                anyhow::bail!(
                    "session {} is persistent and cannot be reused as SIDE",
                    session.id
                );
            }
        }
        if session.active {
            tx.execute(
                "UPDATE sessions SET active = 0 WHERE work_item_id = ?1",
                [&session.work_item_id],
            )?;
        }
        tx.execute(
            "INSERT INTO sessions(
                id, work_item_id, parent_id, active, ephemeral, created_at
             ) VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT(id) DO UPDATE SET
                parent_id = excluded.parent_id,
                active = excluded.active,
                ephemeral = 1",
            params![
                session.id,
                session.work_item_id,
                session.parent_id,
                session.active as i64,
                session.created_at,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Returns the active canonical SIDE session, if one exists.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn ephemeral_side_session(&self, work_item_id: &str) -> Result<Option<SideSessionRecord>> {
        Ok(self
            .connection
            .query_row(
                "SELECT id, work_item_id, parent_id, active, created_at
                 FROM sessions
                 WHERE work_item_id = ?1 AND ephemeral = 1 AND active = 1",
                [work_item_id],
                |row| {
                    Ok(SideSessionRecord {
                        id: row.get(0)?,
                        work_item_id: row.get(1)?,
                        parent_id: row.get(2)?,
                        active: row.get::<_, i64>(3)? != 0,
                        created_at: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Returns all persisted SIDE rows, including an inactive row awaiting
    /// teardown. The cleanup ledger remains separate and is not replaced by
    /// this canonical session history.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn ephemeral_side_sessions_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Vec<SideSessionRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, work_item_id, parent_id, active, created_at
             FROM sessions
             WHERE work_item_id = ?1 AND ephemeral = 1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([work_item_id], |row| {
            Ok(SideSessionRecord {
                id: row.get(0)?,
                work_item_id: row.get(1)?,
                parent_id: row.get(2)?,
                active: row.get::<_, i64>(3)? != 0,
                created_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Atomically moves active ownership to an already-persisted SIDE row.
    /// Returns false when the requested row is not an ephemeral SIDE for the
    /// supplied Work Item; in that case no ownership is changed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn activate_ephemeral_side_session(
        &self,
        work_item_id: &str,
        session_id: &str,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let exists = tx
            .query_row(
                "SELECT 1 FROM sessions
                 WHERE id = ?1 AND work_item_id = ?2 AND ephemeral = 1",
                params![session_id, work_item_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE sessions SET active = 0 WHERE work_item_id = ?1",
            [work_item_id],
        )?;
        tx.execute(
            "UPDATE sessions SET active = 1
             WHERE id = ?1 AND work_item_id = ?2 AND ephemeral = 1",
            params![session_id, work_item_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Removes an inactive canonical SIDE row after the SDK session has been
    /// deleted. Active rows must first be switched away from by the caller.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn delete_ephemeral_side_session(
        &self,
        work_item_id: &str,
        session_id: &str,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let active = tx
            .query_row(
                "SELECT active FROM sessions
                 WHERE id = ?1 AND work_item_id = ?2 AND ephemeral = 1",
                params![session_id, work_item_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if active == Some(1) {
            anyhow::bail!("cannot delete active SIDE session {session_id}");
        }
        let deleted = tx.execute(
            "DELETE FROM sessions
             WHERE id = ?1 AND work_item_id = ?2 AND ephemeral = 1",
            params![session_id, work_item_id],
        )? == 1;
        tx.commit()?;
        Ok(deleted)
    }

    pub fn active_session(&self, work_item_id: &str) -> Result<Option<SessionRecord>> {
        Ok(self
            .connection
            .query_row(
                "SELECT id, work_item_id, parent_id, active, created_at
                 FROM sessions WHERE work_item_id = ?1 AND active = 1",
                [work_item_id],
                |row| {
                    Ok(SessionRecord {
                        id: row.get(0)?,
                        work_item_id: row.get(1)?,
                        parent_id: row.get(2)?,
                        active: row.get::<_, i64>(3)? != 0,
                        created_at: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    #[cfg(test)]
    pub fn sessions_for_work_item(&self, work_item_id: &str) -> Result<Vec<SessionRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, work_item_id, parent_id, active, created_at
             FROM sessions
             WHERE work_item_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([work_item_id], |row| {
            Ok(SessionRecord {
                id: row.get(0)?,
                work_item_id: row.get(1)?,
                parent_id: row.get(2)?,
                active: row.get::<_, i64>(3)? != 0,
                created_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Starts a durable, resumable prune operation by snapshotting every known
    /// persistent and SIDE session before any Work Item cascade can remove it.
    /// An existing unfinished operation wins over the supplied request id.
    pub fn begin_prune_operation(
        &self,
        requested_operation_id: &str,
        work_item_id: &str,
        export_first: bool,
    ) -> Result<String> {
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if let Some(operation_id) = tx
            .query_row(
                "SELECT operation_id
                 FROM prune_operations
                 WHERE work_item_id = ?1",
                [work_item_id],
                |row| row.get(0),
            )
            .optional()?
        {
            tx.commit()?;
            return Ok(operation_id);
        }
        let timestamp = now();
        tx.execute(
            "INSERT INTO prune_operations(
                operation_id, work_item_id, export_first, phase, last_error,
                created_at, updated_at
             ) VALUES (?1, ?2, ?3, 'remote_pending', NULL, ?4, ?4)",
            params![
                requested_operation_id,
                work_item_id,
                export_first,
                timestamp
            ],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO prune_targets(
                operation_id, target_key, kind, session_id, side_operation_id,
                parent_id, state, last_error, created_at, updated_at
             )
             SELECT ?1, 'persistent:' || id, 'persistent', id, NULL, parent_id,
                    'pending', NULL, ?2, ?2
             FROM sessions
             WHERE work_item_id = ?3",
            params![requested_operation_id, timestamp, work_item_id],
        )?;
        // `rev` gives each question its own SDK session. Normally those
        // sessions are also represented in `sessions`, but snapshot the
        // question ownership table explicitly so a partial startup or an
        // older database cannot make permanent deletion forget a remote ID.
        tx.execute(
            "INSERT OR IGNORE INTO prune_targets(
                operation_id, target_key, kind, session_id, side_operation_id,
                parent_id, state, last_error, created_at, updated_at
             )
             SELECT ?1, 'persistent:' || q.session_id, 'persistent',
                    q.session_id, NULL, NULL, 'pending', NULL, ?2, ?2
             FROM rev_question_sessions q
             JOIN annotations a ON a.id = q.annotation_id
             JOIN repos r ON r.id = a.repo_id
             WHERE r.work_item_id = ?3",
            params![requested_operation_id, timestamp, work_item_id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO prune_targets(
                operation_id, target_key, kind, session_id, side_operation_id,
                parent_id, state, last_error, created_at, updated_at
             )
             SELECT ?1, 'side:' || operation_id, 'side', side_id, operation_id,
                    parent_id, 'pending', NULL, ?2, ?2
             FROM ephemeral_sessions
             WHERE work_item_id = ?3",
            params![requested_operation_id, timestamp, work_item_id],
        )?;
        tx.commit()?;
        Ok(requested_operation_id.to_owned())
    }

    pub fn prune_operation(&self, operation_id: &str) -> Result<Option<PruneOperation>> {
        Ok(self
            .connection
            .query_row(
                "SELECT operation_id, work_item_id, export_first, phase, last_error,
                        created_at, updated_at
                 FROM prune_operations
                 WHERE operation_id = ?1",
                [operation_id],
                |row| {
                    Ok(PruneOperation {
                        operation_id: row.get(0)?,
                        work_item_id: row.get(1)?,
                        export_first: row.get::<_, i64>(2)? != 0,
                        phase: row.get(3)?,
                        last_error: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn pending_prune_operations(&self) -> Result<Vec<PruneOperation>> {
        let mut statement = self.connection.prepare(
            "SELECT operation_id, work_item_id, export_first, phase, last_error,
                    created_at, updated_at
             FROM prune_operations
             ORDER BY created_at, operation_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(PruneOperation {
                operation_id: row.get(0)?,
                work_item_id: row.get(1)?,
                export_first: row.get::<_, i64>(2)? != 0,
                phase: row.get(3)?,
                last_error: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn prune_targets(&self, operation_id: &str) -> Result<Vec<PruneTarget>> {
        let mut statement = self.connection.prepare(
            "SELECT operation_id, target_key, kind, session_id, side_operation_id,
                    parent_id, state, last_error, created_at, updated_at
             FROM prune_targets
             WHERE operation_id = ?1
             ORDER BY target_key",
        )?;
        let rows = statement.query_map([operation_id], |row| {
            Ok(PruneTarget {
                operation_id: row.get(0)?,
                target_key: row.get(1)?,
                kind: row.get(2)?,
                session_id: row.get(3)?,
                side_operation_id: row.get(4)?,
                parent_id: row.get(5)?,
                state: row.get(6)?,
                last_error: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[cfg(test)]
    pub fn update_prune_target_session_id(
        &self,
        operation_id: &str,
        target_key: &str,
        session_id: Option<&str>,
    ) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE prune_targets
             SET session_id = ?3, updated_at = ?4
             WHERE operation_id = ?1 AND target_key = ?2",
            params![operation_id, target_key, session_id, now()],
        )? == 1)
    }

    #[cfg(test)]
    pub fn mark_prune_target_deleted(&self, operation_id: &str, target_key: &str) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE prune_targets
             SET state = 'deleted', last_error = NULL, updated_at = ?3
             WHERE operation_id = ?1 AND target_key = ?2",
            params![operation_id, target_key, now()],
        )? == 1)
    }

    #[cfg(test)]
    pub fn mark_prune_phase(
        &self,
        operation_id: &str,
        phase: &str,
        last_error: Option<&str>,
    ) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE prune_operations
             SET phase = ?2, last_error = ?3, updated_at = ?4
             WHERE operation_id = ?1",
            params![operation_id, phase, last_error, now()],
        )? == 1)
    }

    pub fn update_prune_target_session_id_if_owned(
        &self,
        operation_id: &str,
        target_key: &str,
        session_id: Option<&str>,
        work_item_id: &str,
        owner_id: &str,
    ) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE prune_targets
             SET session_id = ?3, updated_at = ?6
             WHERE operation_id = ?1 AND target_key = ?2
               AND EXISTS (
                 SELECT 1 FROM ephemeral_session_leases
                 WHERE work_item_id = ?4 AND owner_id = ?5
               )",
            params![
                operation_id,
                target_key,
                session_id,
                work_item_id,
                owner_id,
                now()
            ],
        )? == 1)
    }

    pub fn mark_prune_target_deleted_if_owned(
        &self,
        operation_id: &str,
        target_key: &str,
        work_item_id: &str,
        owner_id: &str,
    ) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE prune_targets
             SET state = 'deleted', last_error = NULL, updated_at = ?5
             WHERE operation_id = ?1 AND target_key = ?2
               AND EXISTS (
                 SELECT 1 FROM ephemeral_session_leases
                 WHERE work_item_id = ?3 AND owner_id = ?4
               )",
            params![operation_id, target_key, work_item_id, owner_id, now()],
        )? == 1)
    }

    pub fn mark_prune_target_error_if_owned(
        &self,
        operation_id: &str,
        target_key: &str,
        error: &str,
        work_item_id: &str,
        owner_id: &str,
    ) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE prune_targets
             SET last_error = ?3, updated_at = ?6
             WHERE operation_id = ?1 AND target_key = ?2
               AND EXISTS (
                 SELECT 1 FROM ephemeral_session_leases
                 WHERE work_item_id = ?4 AND owner_id = ?5
               )",
            params![
                operation_id,
                target_key,
                error,
                work_item_id,
                owner_id,
                now()
            ],
        )? == 1)
    }

    pub fn mark_prune_phase_if_owned(
        &self,
        operation_id: &str,
        phase: &str,
        last_error: Option<&str>,
        work_item_id: &str,
        owner_id: &str,
    ) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE prune_operations
             SET phase = ?2, last_error = ?3, updated_at = ?6
             WHERE operation_id = ?1
               AND EXISTS (
                 SELECT 1 FROM ephemeral_session_leases
                 WHERE work_item_id = ?4 AND owner_id = ?5
               )",
            params![
                operation_id,
                phase,
                last_error,
                work_item_id,
                owner_id,
                now()
            ],
        )? == 1)
    }

    pub fn clear_prune_operation(&self, operation_id: &str) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM prune_operations
             WHERE operation_id = ?1
               AND NOT EXISTS (
                 SELECT 1 FROM prune_targets
                 WHERE operation_id = ?1 AND state != 'deleted'
               )",
            [operation_id],
        )? == 1)
    }

    pub fn record_ephemeral_session(&self, session: &EphemeralSessionRecord) -> Result<bool> {
        Ok(self.connection.execute(
            "INSERT INTO ephemeral_sessions(
                operation_id, work_item_id, owner_id, parent_id, side_id,
                state, last_error, created_at, updated_at
             )
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9
             WHERE EXISTS (
                SELECT 1
                FROM ephemeral_session_leases
                WHERE work_item_id = ?2 AND owner_id = ?3
             )
             ON CONFLICT(operation_id) DO UPDATE SET
                work_item_id = excluded.work_item_id,
                owner_id = excluded.owner_id,
                parent_id = excluded.parent_id,
                side_id = excluded.side_id,
                state = excluded.state,
                last_error = excluded.last_error,
                updated_at = excluded.updated_at
             WHERE ephemeral_sessions.owner_id = excluded.owner_id",
            params![
                session.operation_id,
                session.work_item_id,
                session.owner_id,
                session.parent_id,
                session.side_id,
                session.state,
                session.last_error,
                session.created_at,
                session.updated_at
            ],
        )? == 1)
    }

    pub fn ephemeral_sessions(&self, work_item_id: &str) -> Result<Vec<EphemeralSessionRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT operation_id, work_item_id, owner_id, parent_id, side_id,
                    state, last_error, created_at, updated_at
             FROM ephemeral_sessions
             WHERE work_item_id = ?1
             ORDER BY created_at, operation_id",
        )?;
        let rows = statement.query_map([work_item_id], |row| {
            Ok(EphemeralSessionRecord {
                operation_id: row.get(0)?,
                work_item_id: row.get(1)?,
                owner_id: row.get(2)?,
                parent_id: row.get(3)?,
                side_id: row.get(4)?,
                state: row.get(5)?,
                last_error: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_ephemeral_session(&self, operation_id: &str, owner_id: &str) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM ephemeral_sessions
             WHERE operation_id = ?1 AND owner_id = ?2",
            params![operation_id, owner_id],
        )? == 1)
    }

    pub fn claim_ephemeral_session_lease(
        &self,
        work_item_id: &str,
        owner_id: &str,
        heartbeat_ms: i64,
        stale_before_ms: i64,
    ) -> Result<bool> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO ephemeral_session_leases(work_item_id, owner_id, heartbeat_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(work_item_id) DO UPDATE SET
                owner_id = excluded.owner_id,
                heartbeat_ms = excluded.heartbeat_ms
             WHERE ephemeral_session_leases.owner_id = excluded.owner_id
                OR ephemeral_session_leases.heartbeat_ms < ?4",
            params![work_item_id, owner_id, heartbeat_ms, stale_before_ms],
        )?;
        let claimed = tx.query_row(
            "SELECT owner_id = ?2
             FROM ephemeral_session_leases
             WHERE work_item_id = ?1",
            params![work_item_id, owner_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if claimed {
            tx.execute(
                "UPDATE ephemeral_sessions
                 SET owner_id = ?2, updated_at = ?3
                 WHERE work_item_id = ?1",
                params![work_item_id, owner_id, now()],
            )?;
        }
        tx.commit()?;
        Ok(claimed)
    }

    pub fn renew_ephemeral_session_lease(
        &self,
        work_item_id: &str,
        owner_id: &str,
        heartbeat_ms: i64,
    ) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE ephemeral_session_leases
             SET heartbeat_ms = ?3
             WHERE work_item_id = ?1 AND owner_id = ?2",
            params![work_item_id, owner_id, heartbeat_ms],
        )? == 1)
    }

    /// Atomically verifies prune ownership/remote completion, clears the
    /// durable journal, and deletes the Work Item as the final local step.
    pub fn finalize_prune_operation(
        &self,
        operation_id: &str,
        work_item_id: &str,
        owner_id: &str,
        heartbeat_ms: i64,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let owned = tx
            .query_row(
                "SELECT owner_id = ?2
                 FROM ephemeral_session_leases
                 WHERE work_item_id = ?1",
                params![work_item_id, owner_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some_and(|owned| owned != 0);
        anyhow::ensure!(
            owned,
            "Work Item cleanup ownership changed before final local deletion"
        );
        tx.execute(
            "UPDATE ephemeral_session_leases
             SET heartbeat_ms = ?3
             WHERE work_item_id = ?1 AND owner_id = ?2",
            params![work_item_id, owner_id, heartbeat_ms],
        )?;
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*)
             FROM prune_targets
             WHERE operation_id = ?1 AND state != 'deleted'",
            [operation_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            pending == 0,
            "a late Copilot session was captured; remote cleanup must retry before local deletion"
        );
        anyhow::ensure!(
            tx.execute(
                "DELETE FROM prune_operations WHERE operation_id = ?1",
                [operation_id],
            )? == 1,
            "the durable prune journal changed before final local deletion"
        );
        let deleted = tx.execute("DELETE FROM work_items WHERE id = ?1", [work_item_id])? == 1;
        tx.commit()?;
        Ok(deleted)
    }

    pub fn release_ephemeral_session_lease(
        &self,
        work_item_id: &str,
        owner_id: &str,
    ) -> Result<()> {
        self.connection.execute(
            "DELETE FROM ephemeral_session_leases
             WHERE work_item_id = ?1 AND owner_id = ?2",
            params![work_item_id, owner_id],
        )?;
        Ok(())
    }

    pub fn enqueue_chat(&self, chat: &PendingChat) -> Result<()> {
        self.connection.execute(
            "INSERT INTO chat_outbox(id, work_item_id, text, kind, lane, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                text = excluded.text,
                kind = excluded.kind,
                lane = excluded.lane",
            params![
                chat.id,
                chat.work_item_id,
                chat.text,
                chat.kind,
                chat.lane,
                chat.created_at
            ],
        )?;
        Ok(())
    }

    pub fn replace_queued_chat(&self, old_id: &str, replacement: &PendingChat) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM chat_outbox WHERE id = ?1", [old_id])?;
        tx.execute(
            "INSERT INTO chat_outbox(id, work_item_id, text, kind, lane, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                replacement.id,
                replacement.work_item_id,
                replacement.text,
                replacement.kind,
                replacement.lane,
                replacement.created_at,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_queued_chat(&self, id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM chat_outbox WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn pending_chats(&self, work_item_id: &str) -> Result<Vec<PendingChat>> {
        let mut statement = self.connection.prepare(
            "SELECT id, work_item_id, text, kind, lane, created_at
             FROM chat_outbox
             WHERE work_item_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([work_item_id], |row| {
            Ok(PendingChat {
                id: row.get(0)?,
                work_item_id: row.get(1)?,
                text: row.get(2)?,
                kind: row.get(3)?,
                lane: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn upsert_context(&self, context: &ReviewContext) -> Result<()> {
        self.connection.execute(
            "INSERT INTO contexts(
                work_item_id, title, what, why, how, considerations,
                alternatives, source, attached_to_session, delivery_state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(work_item_id) DO UPDATE SET
                title = excluded.title,
                what = excluded.what,
                why = excluded.why,
                how = excluded.how,
                considerations = excluded.considerations,
                alternatives = excluded.alternatives,
                source = excluded.source,
                attached_to_session = excluded.attached_to_session,
                delivery_state = excluded.delivery_state",
            params![
                context.work_item_id,
                context.title,
                context.what,
                context.why,
                context.how,
                context.considerations,
                context.alternatives,
                context.source,
                context.attached_to_session as i64,
                context.delivery_state.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn context_for_work_item(&self, work_item_id: &str) -> Result<Option<ReviewContext>> {
        let row = self
            .connection
            .query_row(
                "SELECT work_item_id, title, what, why, how, considerations,
                        alternatives, source, attached_to_session, delivery_state
                 FROM contexts WHERE work_item_id = ?1",
                [work_item_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(
                work_item_id,
                title,
                what,
                why,
                how,
                considerations,
                alternatives,
                source,
                attached,
                delivery,
            )| {
                Ok(ReviewContext {
                    work_item_id,
                    title,
                    what,
                    why,
                    how,
                    considerations,
                    alternatives,
                    source,
                    attached_to_session: attached != 0,
                    delivery_state: DeliveryState::try_from(delivery.as_str())?,
                })
            },
        )
        .transpose()
    }

    pub fn mark_context_sent(&self, work_item_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE contexts
             SET attached_to_session = 1, delivery_state = 'sent'
             WHERE work_item_id = ?1",
            [work_item_id],
        )?;
        Ok(())
    }

    pub fn mark_context_delivery(&self, work_item_id: &str, state: DeliveryState) -> Result<()> {
        self.connection.execute(
            "UPDATE contexts
             SET delivery_state = ?2,
                 attached_to_session = CASE WHEN ?2 = 'sent' THEN 1 ELSE 0 END
             WHERE work_item_id = ?1",
            params![work_item_id, state.as_str()],
        )?;
        Ok(())
    }

    pub fn add_annotation(&self, annotation: &Annotation, placement: &Placement) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO annotations(
                id, repo_id, kind, file_path, anchor_snippet, anchor_hash,
                anchor_start_offset, anchor_line_count, text, submitted,
                delivery_state, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                annotation.id,
                annotation.repo_id,
                annotation.kind.as_str(),
                annotation.file_path.to_string_lossy(),
                annotation.anchor_snippet,
                annotation.anchor_hash,
                annotation.anchor_start_offset,
                annotation.anchor_line_count,
                annotation.text,
                annotation.submitted as i64,
                annotation.delivery_state.as_str(),
                annotation.created_at,
            ],
        )?;
        tx.execute(
            "INSERT INTO placements(
                annotation_id, version_id, side, line_start, line_end, outdated, ambiguous
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                placement.annotation_id,
                placement.version_id,
                placement.side.as_str(),
                placement.line_start,
                placement.line_end,
                placement.outdated as i64,
                placement.ambiguous as i64,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn annotations_for_version(
        &self,
        version_id: &str,
    ) -> Result<Vec<(Annotation, Placement)>> {
        let mut statement = self.connection.prepare(
            "SELECT a.id, a.repo_id, a.kind, a.file_path, a.anchor_snippet,
                    a.anchor_hash, a.anchor_start_offset, a.anchor_line_count,
                    a.text, a.submitted, a.delivery_state, a.created_at,
                    p.side, p.line_start, p.line_end, p.outdated, p.ambiguous
             FROM annotations a
             JOIN placements p ON p.annotation_id = a.id
             WHERE p.version_id = ?1
             ORDER BY a.file_path, p.line_start, a.created_at",
        )?;
        let rows = statement.query_map([version_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, i64>(15)?,
                row.get::<_, i64>(16)?,
            ))
        })?;
        rows.map(|row| {
            let (
                id,
                repo_id,
                kind,
                file_path,
                anchor_snippet,
                anchor_hash,
                anchor_start_offset,
                anchor_line_count,
                text,
                submitted,
                delivery_state,
                created_at,
                side,
                line_start,
                line_end,
                outdated,
                ambiguous,
            ) = row?;
            Ok((
                Annotation {
                    id: id.clone(),
                    repo_id,
                    kind: AnnotationKind::try_from(kind.as_str())?,
                    file_path: PathBuf::from(file_path),
                    anchor_snippet,
                    anchor_hash,
                    anchor_start_offset,
                    anchor_line_count,
                    text,
                    submitted: submitted != 0,
                    delivery_state: DeliveryState::try_from(delivery_state.as_str())?,
                    created_at,
                },
                Placement {
                    annotation_id: id,
                    version_id: version_id.to_owned(),
                    side: AnchorSide::try_from(side.as_str())?,
                    line_start,
                    line_end,
                    outdated: outdated != 0,
                    ambiguous: ambiguous != 0,
                },
            ))
        })
        .collect()
    }

    pub fn annotation_by_id(&self, annotation_id: &str) -> Result<Option<Annotation>> {
        let row = self
            .connection
            .query_row(
                "SELECT id, repo_id, kind, file_path, anchor_snippet, anchor_hash,
                        anchor_start_offset, anchor_line_count, text, submitted,
                        delivery_state, created_at
                 FROM annotations WHERE id = ?1",
                [annotation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, String>(11)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(
                id,
                repo_id,
                kind,
                file_path,
                anchor_snippet,
                anchor_hash,
                anchor_start_offset,
                anchor_line_count,
                text,
                submitted,
                delivery_state,
                created_at,
            )| {
                Ok(Annotation {
                    id,
                    repo_id,
                    kind: AnnotationKind::try_from(kind.as_str())?,
                    file_path: PathBuf::from(file_path),
                    anchor_snippet,
                    anchor_hash,
                    anchor_start_offset,
                    anchor_line_count,
                    text,
                    submitted: submitted != 0,
                    delivery_state: DeliveryState::try_from(delivery_state.as_str())?,
                    created_at,
                })
            },
        )
        .transpose()
    }

    pub fn upsert_placement(&self, placement: &Placement) -> Result<()> {
        self.connection.execute(
            "INSERT INTO placements(
                annotation_id, version_id, side, line_start, line_end, outdated, ambiguous
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(annotation_id, version_id) DO UPDATE SET
                side = excluded.side,
                line_start = excluded.line_start,
                line_end = excluded.line_end,
                outdated = excluded.outdated,
                ambiguous = excluded.ambiguous",
            params![
                placement.annotation_id,
                placement.version_id,
                placement.side.as_str(),
                placement.line_start,
                placement.line_end,
                placement.outdated as i64,
                placement.ambiguous as i64,
            ],
        )?;
        Ok(())
    }

    pub fn update_annotation_file_path(&self, annotation_id: &str, path: &Path) -> Result<()> {
        self.connection.execute(
            "UPDATE annotations SET file_path = ?2 WHERE id = ?1",
            params![annotation_id, path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn update_annotation_text(&self, annotation_id: &str, text: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE annotations SET text = ?2 WHERE id = ?1",
            params![annotation_id, text],
        )?;
        Ok(())
    }

    pub fn update_annotation_anchor(&self, annotation: &Annotation) -> Result<()> {
        self.connection.execute(
            "UPDATE annotations
             SET file_path = ?2, anchor_snippet = ?3, anchor_hash = ?4,
                 anchor_start_offset = ?5, anchor_line_count = ?6
             WHERE id = ?1",
            params![
                annotation.id,
                annotation.file_path.to_string_lossy(),
                annotation.anchor_snippet,
                annotation.anchor_hash,
                annotation.anchor_start_offset,
                annotation.anchor_line_count,
            ],
        )?;
        Ok(())
    }

    pub fn placements_for_annotation(&self, annotation_id: &str) -> Result<Vec<Placement>> {
        let mut statement = self.connection.prepare(
            "SELECT annotation_id, version_id, side, line_start, line_end, outdated, ambiguous
             FROM placements WHERE annotation_id = ?1",
        )?;
        let rows = statement.query_map([annotation_id], |row| {
            Ok(Placement {
                annotation_id: row.get(0)?,
                version_id: row.get(1)?,
                side: AnchorSide::try_from(row.get::<_, String>(2)?.as_str()).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        error.into(),
                    )
                })?,
                line_start: row.get(3)?,
                line_end: row.get(4)?,
                outdated: row.get::<_, i64>(5)? != 0,
                ambiguous: row.get::<_, i64>(6)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_annotation(&self, annotation_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM annotations WHERE id = ?1", [annotation_id])?;
        Ok(())
    }

    pub fn pending_comments_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Vec<(Annotation, Repo)>> {
        let mut statement = self.connection.prepare(
            "SELECT a.id, a.repo_id, a.file_path, a.anchor_snippet, a.anchor_hash,
                    a.anchor_start_offset, a.anchor_line_count, a.text, a.submitted,
                    a.delivery_state, a.created_at,
                    r.work_item_id, r.name, r.path, r.remote_pr_url, r.pr_meta_json,
                    r.base_branch, r.base_branch_source, r.last_activity_at
             FROM annotations a
             JOIN repos r ON r.id = a.repo_id
             WHERE r.work_item_id = ?1
               AND a.kind = 'comment'
               AND a.submitted = 0
             ORDER BY r.name, a.file_path, a.created_at",
        )?;
        let rows = statement.query_map([work_item_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, Option<String>>(16)?,
                row.get::<_, String>(17)?,
                row.get::<_, Option<String>>(18)?,
            ))
        })?;
        rows.map(|row| {
            let (
                annotation_id,
                repo_id,
                file_path,
                anchor_snippet,
                anchor_hash,
                anchor_start_offset,
                anchor_line_count,
                text,
                submitted,
                delivery_state,
                created_at,
                repo_work_item_id,
                repo_name,
                repo_path,
                remote_pr_url,
                pr_meta_json,
                base_branch,
                base_branch_source,
                last_activity_at,
            ) = row?;
            Ok((
                Annotation {
                    id: annotation_id,
                    repo_id: repo_id.clone(),
                    kind: AnnotationKind::Comment,
                    file_path: PathBuf::from(file_path),
                    anchor_snippet,
                    anchor_hash,
                    anchor_start_offset,
                    anchor_line_count,
                    text,
                    submitted: submitted != 0,
                    delivery_state: DeliveryState::try_from(delivery_state.as_str())?,
                    created_at,
                },
                Repo {
                    id: repo_id,
                    work_item_id: repo_work_item_id,
                    name: repo_name,
                    path: PathBuf::from(repo_path),
                    remote_pr_url,
                    pr_meta_json,
                    base_branch,
                    base_branch_source: BaseBranchSource::try_from(base_branch_source.as_str())?,
                    last_activity_at,
                },
            ))
        })
        .collect()
    }

    pub fn pending_comment_delivery_ids(&self, work_item_id: &str) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT a.id
             FROM annotations a
             JOIN repos r ON r.id = a.repo_id
             WHERE r.work_item_id = ?1
               AND a.kind = 'comment'
               AND a.delivery_state = 'pending'
             ORDER BY a.created_at",
        )?;
        let rows = statement.query_map([work_item_id], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn mark_comments_delivery(
        &self,
        annotation_ids: &[String],
        state: DeliveryState,
    ) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        for id in annotation_ids {
            tx.execute(
                "UPDATE annotations
                 SET delivery_state = ?2,
                     submitted = CASE WHEN ?2 = 'sent' THEN 1 ELSE submitted END
                 WHERE id = ?1",
                params![id, state.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn latest_placement_for_annotation(
        &self,
        annotation_id: &str,
    ) -> Result<Option<Placement>> {
        Ok(self
            .connection
            .query_row(
                "SELECT p.annotation_id, p.version_id, p.side, p.line_start, p.line_end,
                        p.outdated, p.ambiguous
                 FROM placements p
                 JOIN versions v ON v.id = p.version_id
                 WHERE p.annotation_id = ?1
                 ORDER BY COALESCE(v.last_opened_at, v.created_at) DESC
                 LIMIT 1",
                [annotation_id],
                |row| {
                    Ok(Placement {
                        annotation_id: row.get(0)?,
                        version_id: row.get(1)?,
                        side: AnchorSide::try_from(row.get::<_, String>(2)?.as_str()).map_err(
                            |error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    2,
                                    rusqlite::types::Type::Text,
                                    error.into(),
                                )
                            },
                        )?,
                        line_start: row.get(3)?,
                        line_end: row.get(4)?,
                        outdated: row.get::<_, i64>(5)? != 0,
                        ambiguous: row.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .optional()?)
    }

    pub fn append_ask_message(&self, message: &AskMessage) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO ask_messages(
                id, annotation_id, seq, role, text, sent, delivery_state, ts
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                message.id,
                message.annotation_id,
                message.seq,
                message.role,
                message.text,
                message.sent as i64,
                message.delivery_state.as_str(),
                message.ts,
            ],
        )?;
        if message.delivery_state == DeliveryState::Pending {
            tx.execute(
                "UPDATE annotations SET delivery_state = 'pending' WHERE id = ?1",
                [&message.annotation_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn acknowledge_ask_with_response_start(
        &self,
        user_message_id: &str,
        response: &AskMessage,
    ) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "UPDATE ask_messages
             SET sent = 1, delivery_state = 'sent'
             WHERE id = ?1",
            [user_message_id],
        )?;
        tx.execute(
            "UPDATE annotations
             SET delivery_state = 'sent'
             WHERE id = ?1",
            [&response.annotation_id],
        )?;
        tx.execute(
            "INSERT INTO ask_messages(
                id, annotation_id, seq, role, text, sent, delivery_state, ts
             ) VALUES (?1, ?2, ?3, ?4, ?5, 1, 'sent', ?6)",
            params![
                response.id,
                response.annotation_id,
                response.seq,
                response.role,
                response.text,
                response.ts,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_ask_message_text(&self, message_id: &str, text: &str) -> Result<()> {
        let updated = self.connection.execute(
            "UPDATE ask_messages SET text = ?2 WHERE id = ?1",
            params![message_id, text],
        )?;
        anyhow::ensure!(updated == 1, "Ask message {message_id} no longer exists");
        Ok(())
    }

    pub fn append_ask_message_delta(&self, message_id: &str, delta: &str) -> Result<()> {
        let updated = self.connection.execute(
            "UPDATE ask_messages SET text = text || ?2 WHERE id = ?1",
            params![message_id, delta],
        )?;
        anyhow::ensure!(updated == 1, "Ask message {message_id} no longer exists");
        Ok(())
    }

    pub fn pending_ask_messages(&self, work_item_id: &str) -> Result<Vec<AskMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT m.id, m.annotation_id, m.seq, m.role, m.text, m.sent,
                    m.delivery_state, m.ts
             FROM ask_messages m
             JOIN annotations a ON a.id = m.annotation_id
             JOIN repos r ON r.id = a.repo_id
             WHERE r.work_item_id = ?1 AND m.delivery_state = 'pending'
             ORDER BY m.ts, m.annotation_id, m.seq",
        )?;
        let rows = statement.query_map([work_item_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        rows.map(|row| {
            let (id, annotation_id, seq, role, text, sent, state, ts) = row?;
            Ok(AskMessage {
                id,
                annotation_id,
                seq,
                role,
                text,
                sent: sent != 0,
                delivery_state: DeliveryState::try_from(state.as_str())?,
                ts,
            })
        })
        .collect()
    }

    pub fn ask_messages_for_annotation(&self, annotation_id: &str) -> Result<Vec<AskMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT id, annotation_id, seq, role, text, sent, delivery_state, ts
             FROM ask_messages
             WHERE annotation_id = ?1
             ORDER BY seq",
        )?;
        let rows = statement.query_map([annotation_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        rows.map(|row| {
            let (id, annotation_id, seq, role, text, sent, state, ts) = row?;
            Ok(AskMessage {
                id,
                annotation_id,
                seq,
                role,
                text,
                sent: sent != 0,
                delivery_state: DeliveryState::try_from(state.as_str())?,
                ts,
            })
        })
        .collect()
    }

    pub fn discard_pending_ask(&self, message_id: &str) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "UPDATE ask_messages SET delivery_state = 'draft' WHERE id = ?1",
            [message_id],
        )?;
        tx.execute(
            "UPDATE annotations
             SET delivery_state = 'draft'
             WHERE id = (
                 SELECT annotation_id FROM ask_messages WHERE id = ?1
             )",
            [message_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }
}

pub fn now() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::{
        RevQuestionSession, SideSessionRecord, Storage, MIGRATION_1, MIGRATION_2, MIGRATION_3,
        MIGRATION_4, MIGRATION_5, MIGRATION_6,
    };
    use rq_tui_domain::{
        AnchorSide, Annotation, AnnotationKind, AskMessage, BaseBranchSource, DeliveryState,
        EphemeralSessionRecord, PendingChat, Placement, Repo, SessionRecord, Version, VersionKind,
        WorkItem,
    };

    #[test]
    fn migrations_create_expected_tables_and_indexes() {
        let storage = Storage::in_memory().unwrap();
        let count: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type IN ('table', 'index')
                   AND name IN (
                     'work_items', 'sessions', 'repos', 'contexts', 'versions',
                     'chat_outbox', 'ephemeral_sessions',
                     'ephemeral_session_leases',
                     'prune_operations', 'prune_targets',
                     'annotations', 'placements', 'ask_messages', 'settings',
                     'versions_repo_version', 'placements_version',
                     'annotations_repo_submitted', 'ask_messages_annotation_seq',
                     'work_items_last_opened', 'chat_outbox_work_item_created',
                     'ephemeral_sessions_work_item_created',
                     'sessions_work_item_ephemeral',
                     'prune_operations_one_unfinished_per_work_item',
                     'prune_targets_operation_state',
                     'rev_question_sessions',
                     'rev_question_sessions_state'
                   )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 26);
        let ephemeral_column: i64 = storage
            .connection
            .query_row(
                r#"SELECT COUNT(*) FROM pragma_table_info('sessions')
                 WHERE name = 'ephemeral' AND "notnull" = 1 AND dflt_value = '0'"#,
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ephemeral_column, 1);
    }

    #[test]
    fn canonical_side_sessions_round_trip_and_switch_active_ownership() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "canonical-side".into(),
            name: "canonical side".into(),
            workspace_root: PathBuf::from("/canonical-side"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "main".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();

        let side = SideSessionRecord {
            id: "side".into(),
            work_item_id: item.id.clone(),
            parent_id: Some("main".into()),
            active: false,
            created_at: "2".into(),
        };
        storage.persist_ephemeral_side_session(&side).unwrap();
        assert_eq!(storage.ephemeral_side_session(&item.id).unwrap(), None);
        assert_eq!(
            storage
                .ephemeral_side_sessions_for_work_item(&item.id)
                .unwrap(),
            vec![side.clone()]
        );
        assert_eq!(
            storage
                .connection
                .query_row(
                    "SELECT ephemeral FROM sessions WHERE id = 'side'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            storage.active_session(&item.id).unwrap().unwrap().id,
            "main"
        );

        assert!(storage
            .activate_ephemeral_side_session(&item.id, &side.id)
            .unwrap());
        assert_eq!(
            storage.ephemeral_side_session(&item.id).unwrap(),
            Some(SideSessionRecord {
                active: true,
                ..side.clone()
            })
        );
        assert_eq!(
            storage.active_session(&item.id).unwrap().unwrap().id,
            "side"
        );
        let active_count: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE work_item_id = ?1 AND active = 1",
                [&item.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_count, 1);

        storage
            .activate_session(&SessionRecord {
                id: "main".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();
        assert!(storage
            .delete_ephemeral_side_session(&item.id, &side.id)
            .unwrap());
        assert!(storage
            .ephemeral_side_sessions_for_work_item(&item.id)
            .unwrap()
            .is_empty());
        assert!(!storage
            .activate_ephemeral_side_session(&item.id, &side.id)
            .unwrap());
    }

    #[test]
    fn persisting_active_side_switches_ownership_in_one_transaction() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "canonical-side-active".into(),
            name: "canonical side active".into(),
            workspace_root: PathBuf::from("/canonical-side-active"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "main".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();
        storage
            .persist_ephemeral_side_session(&SideSessionRecord {
                id: "side".into(),
                work_item_id: item.id.clone(),
                parent_id: Some("main".into()),
                active: true,
                created_at: "2".into(),
            })
            .unwrap();

        assert_eq!(
            storage.active_session(&item.id).unwrap().unwrap().id,
            "side"
        );
        assert!(!storage
            .sessions_for_work_item(&item.id)
            .unwrap()
            .iter()
            .any(|session| session.id == "main" && session.active));
    }

    #[test]
    fn session_enumeration_keeps_inactive_parents_and_active_forks() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "session-history".into(),
            name: "session history".into(),
            workspace_root: PathBuf::from("/session-history"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: Some("1".into()),
        };
        storage.upsert_work_item(&item).unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "main-parent".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "main-fork".into(),
                work_item_id: item.id.clone(),
                parent_id: Some("main-parent".into()),
                active: true,
                created_at: "2".into(),
            })
            .unwrap();

        let sessions = storage.sessions_for_work_item(&item.id).unwrap();

        assert_eq!(
            sessions
                .iter()
                .map(|session| (session.id.as_str(), session.active))
                .collect::<Vec<_>>(),
            [("main-parent", false), ("main-fork", true)]
        );
        assert_eq!(sessions[1].parent_id.as_deref(), Some("main-parent"));
    }

    #[test]
    fn unopened_remote_cleanup_never_targets_a_newer_version() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "remote-history".into(),
            name: "remote history".into(),
            workspace_root: PathBuf::from("/remote-history"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: Some("1".into()),
        };
        storage.upsert_work_item(&item).unwrap();
        storage
            .upsert_repo(&Repo {
                id: "repo".into(),
                work_item_id: item.id,
                name: "repo".into(),
                path: PathBuf::from("/remote-history/repo.git"),
                remote_pr_url: Some("https://github.com/acme/repo/pull/1".into()),
                pr_meta_json: None,
                base_branch: Some("main".into()),
                base_branch_source: BaseBranchSource::Auto,
                last_activity_at: None,
            })
            .unwrap();
        for (number, opened) in [(1, None), (2, Some("reviewed")), (3, None)] {
            storage
                .upsert_version(&Version {
                    id: format!("v{number}"),
                    repo_id: "repo".into(),
                    version_num: number,
                    kind: VersionKind::Remote,
                    created_at: number.to_string(),
                    head_sha: format!("head-{number}"),
                    worktree_path: Some(PathBuf::from(format!("/worktrees/v{number}"))),
                    last_opened_at: opened.map(str::to_owned),
                })
                .unwrap();
        }

        let stale = storage
            .unopened_remote_versions_older_than("repo", 2)
            .unwrap();

        assert_eq!(
            stale
                .iter()
                .map(|version| version.id.as_str())
                .collect::<Vec<_>>(),
            ["v1"]
        );
        assert!(storage
            .versions_for_repo("repo")
            .unwrap()
            .iter()
            .any(|version| version.id == "v3"));
    }

    #[test]
    fn begin_prune_operation_snapshots_persistent_and_side_session_history() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "prune-snapshot".into(),
            name: "prune snapshot".into(),
            workspace_root: PathBuf::from("/prune-snapshot"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: Some("1".into()),
        };
        storage.upsert_work_item(&item).unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "main-parent".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "main-fork".into(),
                work_item_id: item.id.clone(),
                parent_id: Some("main-parent".into()),
                active: true,
                created_at: "2".into(),
            })
            .unwrap();
        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "owner", 1_000, 0)
            .unwrap());
        for (operation_id, side_id, parent_id) in [
            ("side-known", Some("side-session"), Some("main-fork")),
            ("side-unknown", None, Some("main-parent")),
        ] {
            assert!(storage
                .record_ephemeral_session(&EphemeralSessionRecord {
                    operation_id: operation_id.into(),
                    work_item_id: item.id.clone(),
                    owner_id: "owner".into(),
                    parent_id: parent_id.map(str::to_owned),
                    side_id: side_id.map(str::to_owned),
                    state: "active".into(),
                    last_error: None,
                    created_at: "3".into(),
                    updated_at: "3".into(),
                })
                .unwrap());
        }

        let operation_id = storage
            .begin_prune_operation("prune-op", &item.id, true)
            .unwrap();

        assert_eq!(operation_id, "prune-op");
        let operation = storage.prune_operation(&operation_id).unwrap().unwrap();
        assert_eq!(operation.work_item_id, item.id);
        assert!(operation.export_first);
        assert_eq!(operation.phase, "remote_pending");
        assert_eq!(operation.last_error, None);

        let targets = storage.prune_targets(&operation_id).unwrap();
        assert_eq!(targets.len(), 4);
        let persistent_parent = targets
            .iter()
            .find(|target| target.target_key == "persistent:main-parent")
            .unwrap();
        assert_eq!(persistent_parent.kind, "persistent");
        assert_eq!(persistent_parent.session_id.as_deref(), Some("main-parent"));
        assert_eq!(persistent_parent.parent_id, None);
        let persistent_fork = targets
            .iter()
            .find(|target| target.target_key == "persistent:main-fork")
            .unwrap();
        assert_eq!(persistent_fork.parent_id.as_deref(), Some("main-parent"));
        let known_side = targets
            .iter()
            .find(|target| target.target_key == "side:side-known")
            .unwrap();
        assert_eq!(known_side.kind, "side");
        assert_eq!(known_side.session_id.as_deref(), Some("side-session"));
        assert_eq!(known_side.side_operation_id.as_deref(), Some("side-known"));
        assert_eq!(known_side.parent_id.as_deref(), Some("main-fork"));
        let unknown_side = targets
            .iter()
            .find(|target| target.target_key == "side:side-unknown")
            .unwrap();
        assert_eq!(unknown_side.session_id, None);
        assert_eq!(
            unknown_side.side_operation_id.as_deref(),
            Some("side-unknown")
        );
        assert_eq!(unknown_side.parent_id.as_deref(), Some("main-parent"));
        assert_eq!(unknown_side.state, "pending");

        assert!(storage
            .update_prune_target_session_id(
                &operation_id,
                "side:side-unknown",
                Some("reconciled-side"),
            )
            .unwrap());
        assert!(storage
            .mark_prune_target_deleted(&operation_id, "side:side-unknown")
            .unwrap());
        assert!(storage
            .mark_prune_phase(&operation_id, "local_pending", None)
            .unwrap());
        let updated = storage
            .prune_targets(&operation_id)
            .unwrap()
            .into_iter()
            .find(|target| target.target_key == "side:side-unknown")
            .unwrap();
        assert_eq!(updated.session_id.as_deref(), Some("reconciled-side"));
        assert_eq!(updated.state, "deleted");
        assert_eq!(
            storage
                .prune_operation(&operation_id)
                .unwrap()
                .unwrap()
                .phase,
            "local_pending"
        );
    }

    #[test]
    fn unfinished_prune_reuses_its_snapshot_and_captures_rejected_late_sessions() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "prune-reuse".into(),
            name: "prune reuse".into(),
            workspace_root: PathBuf::from("/prune-reuse"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();

        assert_eq!(
            storage
                .begin_prune_operation("first-operation", &item.id, true)
                .unwrap(),
            "first-operation"
        );
        let late_error = storage
            .activate_session(&SessionRecord {
                id: "created-after-first-snapshot".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "2".into(),
            })
            .unwrap_err();
        assert!(late_error.to_string().contains("captured for cleanup"));
        assert_eq!(
            storage
                .begin_prune_operation("second-operation", &item.id, false)
                .unwrap(),
            "first-operation"
        );
        let operations = storage.pending_prune_operations().unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].operation_id, "first-operation");
        assert!(operations[0].export_first);
        assert!(storage
            .prune_targets("first-operation")
            .unwrap()
            .iter()
            .any(|target| target.session_id.as_deref() == Some("created-after-first-snapshot")));
    }

    #[test]
    fn prune_journal_survives_work_item_delete_until_explicitly_cleared() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "prune-survives-cascade".into(),
            name: "prune survives cascade".into(),
            workspace_root: PathBuf::from("/prune-survives-cascade"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "persistent-session".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();
        let operation_id = storage
            .begin_prune_operation("durable-prune", &item.id, false)
            .unwrap();
        assert!(!storage.clear_prune_operation(&operation_id).unwrap());
        assert!(storage
            .mark_prune_target_deleted(&operation_id, "persistent:persistent-session")
            .unwrap());

        storage.delete_work_item(&item.id).unwrap();

        assert!(storage.work_item_by_id(&item.id).unwrap().is_none());
        assert_eq!(
            storage
                .prune_operation(&operation_id)
                .unwrap()
                .unwrap()
                .work_item_id,
            item.id
        );
        assert_eq!(storage.prune_targets(&operation_id).unwrap().len(), 1);
        assert!(storage.clear_prune_operation(&operation_id).unwrap());
        assert!(storage.prune_operation(&operation_id).unwrap().is_none());
        assert!(storage.prune_targets(&operation_id).unwrap().is_empty());
    }

    #[test]
    fn concurrent_first_open_applies_each_migration_once() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("review.db");
        let workers = 8;
        let barrier = Arc::new(Barrier::new(workers));
        let handles = (0..workers)
            .map(|_| {
                let database = database.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    Storage::open(&database).map(|_| ())
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let storage = Storage::open(&database).unwrap();
        let count: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version IN (1, 2, 3, 4, 5, 6, 7, 8)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 8);
    }

    #[test]
    fn schema_four_database_upgrades_prune_journal_in_place() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("review.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                    version INTEGER PRIMARY KEY,
                    applied_at TEXT NOT NULL
                 );",
            )
            .unwrap();
        for (version, sql) in [
            (1, MIGRATION_1),
            (2, MIGRATION_2),
            (3, MIGRATION_3),
            (4, MIGRATION_4),
        ] {
            connection.execute_batch(sql).unwrap();
            connection
                .execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, 'now')",
                    [version],
                )
                .unwrap();
        }
        drop(connection);

        let storage = Storage::open(&database).unwrap();
        let migrations_applied: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version IN (5, 6, 7, 8)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let tables: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table'
                   AND name IN ('prune_operations', 'prune_targets')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migrations_applied, 4);
        assert_eq!(tables, 2);
    }

    #[test]
    fn schema_six_database_adds_ephemeral_with_safe_default() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("review.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                    version INTEGER PRIMARY KEY,
                    applied_at TEXT NOT NULL
                 );",
            )
            .unwrap();
        for (version, sql) in [
            (1, MIGRATION_1),
            (2, MIGRATION_2),
            (3, MIGRATION_3),
            (4, MIGRATION_4),
            (5, MIGRATION_5),
            (6, MIGRATION_6),
        ] {
            connection.execute_batch(sql).unwrap();
            connection
                .execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, 'now')",
                    [version],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO work_items(
                    id, name, workspace_root, created_at, updated_at
                 ) VALUES ('legacy', 'legacy', '/legacy', '1', '1')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions(
                    id, work_item_id, parent_id, active, created_at
                 ) VALUES ('legacy-main', 'legacy', NULL, 1, '1')",
                [],
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&database).unwrap();
        let version_count: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 7",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version_count, 1);
        let legacy_ephemeral: i64 = storage
            .connection
            .query_row(
                "SELECT ephemeral FROM sessions WHERE id = 'legacy-main'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_ephemeral, 0);
        assert_eq!(
            storage.active_session("legacy").unwrap().unwrap().id,
            "legacy-main"
        );
    }

    #[test]
    fn work_items_round_trip() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "work-1".into(),
            name: "demo".into(),
            workspace_root: PathBuf::from("/tmp/demo"),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();
        assert_eq!(storage.list_work_items().unwrap(), vec![item]);
    }

    #[test]
    fn chat_outbox_replaces_and_clears_atomically() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "work-chat".into(),
            name: "demo".into(),
            workspace_root: PathBuf::from("/tmp/demo"),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();
        let original = PendingChat {
            id: "chat-1".into(),
            work_item_id: item.id.clone(),
            text: "original".into(),
            kind: "chat".into(),
            lane: "main".into(),
            created_at: "1".into(),
        };
        storage.enqueue_chat(&original).unwrap();
        assert_eq!(storage.pending_chats(&item.id).unwrap(), vec![original]);

        let replacement = PendingChat {
            id: "chat-2".into(),
            work_item_id: item.id.clone(),
            text: "replacement".into(),
            kind: "chat".into(),
            lane: "main".into(),
            created_at: "2".into(),
        };
        storage.replace_queued_chat("chat-1", &replacement).unwrap();
        assert_eq!(
            storage.pending_chats(&item.id).unwrap(),
            vec![replacement.clone()]
        );
        storage.delete_queued_chat(&replacement.id).unwrap();
        assert!(storage.pending_chats(&item.id).unwrap().is_empty());
    }

    #[test]
    fn ephemeral_session_ledger_survives_until_cleanup_acknowledgement() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "work-side".into(),
            name: "demo".into(),
            workspace_root: PathBuf::from("/tmp/demo"),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();
        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "owner-1", 1_000, 0)
            .unwrap());
        let side = EphemeralSessionRecord {
            operation_id: "operation-1".into(),
            work_item_id: item.id.clone(),
            owner_id: "owner-1".into(),
            parent_id: None,
            side_id: None,
            state: "intent".into(),
            last_error: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        };

        assert!(storage.record_ephemeral_session(&side).unwrap());
        assert_eq!(
            storage.ephemeral_sessions(&item.id).unwrap(),
            vec![side.clone()]
        );

        let updated = EphemeralSessionRecord {
            parent_id: Some("main-1".into()),
            side_id: Some("side-1".into()),
            state: "active".into(),
            updated_at: "2".into(),
            ..side.clone()
        };
        assert!(storage.record_ephemeral_session(&updated).unwrap());
        assert_eq!(
            storage.ephemeral_sessions(&item.id).unwrap(),
            vec![updated.clone()]
        );

        assert!(storage
            .delete_ephemeral_session(&updated.operation_id, &updated.owner_id)
            .unwrap());
        assert!(storage.ephemeral_sessions(&item.id).unwrap().is_empty());
    }

    #[test]
    fn ephemeral_session_lease_prevents_live_cross_process_cleanup() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "work-lease".into(),
            name: "demo".into(),
            workspace_root: PathBuf::from("/tmp/demo"),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();
        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "old-owner", 1, 0)
            .unwrap());
        let stale_record = EphemeralSessionRecord {
            operation_id: "leased-operation".into(),
            work_item_id: item.id.clone(),
            owner_id: "old-owner".into(),
            parent_id: Some("main".into()),
            side_id: Some("side".into()),
            state: "active".into(),
            last_error: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        };
        assert!(storage.record_ephemeral_session(&stale_record).unwrap());

        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "owner-a", 1_000, 2)
            .unwrap());
        assert!(!storage.record_ephemeral_session(&stale_record).unwrap());
        assert!(!storage
            .delete_ephemeral_session(&stale_record.operation_id, "old-owner")
            .unwrap());
        let adopted = storage.ephemeral_sessions(&item.id).unwrap();
        assert_eq!(adopted[0].owner_id, "owner-a");
        assert!(!storage
            .claim_ephemeral_session_lease(&item.id, "owner-b", 1_001, 999)
            .unwrap());
        assert!(storage
            .renew_ephemeral_session_lease(&item.id, "owner-a", 1_002)
            .unwrap());
        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "owner-b", 2_000, 1_500)
            .unwrap());
        assert!(!storage
            .renew_ephemeral_session_lease(&item.id, "owner-a", 2_001)
            .unwrap());

        storage
            .release_ephemeral_session_lease(&item.id, "owner-a")
            .unwrap();
        assert!(!storage
            .claim_ephemeral_session_lease(&item.id, "owner-a", 2_002, 1_999)
            .unwrap());
        storage
            .release_ephemeral_session_lease(&item.id, "owner-b")
            .unwrap();
        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "owner-a", 2_003, 2_002)
            .unwrap());
    }

    #[test]
    fn independent_connections_fence_the_previous_side_owner() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("review.db");
        let first = Storage::open(&database).unwrap();
        let item = WorkItem {
            id: "work-two-processes".into(),
            name: "demo".into(),
            workspace_root: PathBuf::from("/tmp/demo"),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_opened_at: None,
        };
        first.upsert_work_item(&item).unwrap();
        assert!(first
            .claim_ephemeral_session_lease(&item.id, "owner-a", 1_000, 0)
            .unwrap());
        let record = EphemeralSessionRecord {
            operation_id: "operation".into(),
            work_item_id: item.id.clone(),
            owner_id: "owner-a".into(),
            parent_id: Some("main".into()),
            side_id: Some("side".into()),
            state: "opening".into(),
            last_error: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        };
        assert!(first.record_ephemeral_session(&record).unwrap());

        let second = Storage::open(&database).unwrap();
        assert!(!second
            .claim_ephemeral_session_lease(&item.id, "owner-b", 1_001, 999)
            .unwrap());
        assert!(second
            .claim_ephemeral_session_lease(&item.id, "owner-b", 2_000, 1_500)
            .unwrap());
        assert!(!first.record_ephemeral_session(&record).unwrap());
        let stale_new_intent = EphemeralSessionRecord {
            operation_id: "stale-new-operation".into(),
            side_id: None,
            state: "intent".into(),
            ..record.clone()
        };
        assert!(!first.record_ephemeral_session(&stale_new_intent).unwrap());
        assert!(!first
            .delete_ephemeral_session(&record.operation_id, "owner-a")
            .unwrap());
        let adopted = second.ephemeral_sessions(&item.id).unwrap();
        assert_eq!(adopted[0].owner_id, "owner-b");
    }

    #[test]
    fn final_prune_transition_clears_journal_and_work_item_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("review.db");
        let first = Storage::open(&database).unwrap();
        let item = WorkItem {
            id: "fenced-local-prune".into(),
            name: "fenced".into(),
            workspace_root: PathBuf::from("/tmp/fenced"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: None,
        };
        first.upsert_work_item(&item).unwrap();
        assert!(first
            .claim_ephemeral_session_lease(&item.id, "owner-a", 1_000, 0)
            .unwrap());
        let operation_id = first
            .begin_prune_operation("atomic-finalize", &item.id, false)
            .unwrap();
        assert!(first
            .finalize_prune_operation(&operation_id, &item.id, "owner-a", 1_001)
            .unwrap());
        assert!(first.work_item_by_id(&item.id).unwrap().is_none());
        assert!(first.prune_operation(&operation_id).unwrap().is_none());
    }

    #[test]
    fn stale_owner_cannot_advance_the_durable_prune_journal() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("review.db");
        let first = Storage::open(&database).unwrap();
        let item = WorkItem {
            id: "fenced-journal".into(),
            name: "fenced".into(),
            workspace_root: PathBuf::from("/tmp/fenced"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: None,
        };
        first.upsert_work_item(&item).unwrap();
        assert!(first
            .claim_ephemeral_session_lease(&item.id, "owner-a", 1_000, 0)
            .unwrap());
        let operation_id = first
            .begin_prune_operation("fenced-operation", &item.id, false)
            .unwrap();
        let second = Storage::open(&database).unwrap();
        assert!(second
            .claim_ephemeral_session_lease(&item.id, "owner-b", 2_000, 1_500)
            .unwrap());

        assert!(!first
            .mark_prune_phase_if_owned(&operation_id, "local_pending", None, &item.id, "owner-a",)
            .unwrap());
        assert!(second
            .mark_prune_phase_if_owned(&operation_id, "local_pending", None, &item.id, "owner-b",)
            .unwrap());
    }

    #[test]
    fn main_fork_activation_and_intent_clear_are_one_owned_transaction() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "fork-intent".into(),
            name: "fork".into(),
            workspace_root: PathBuf::from("/fork"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "parent".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();
        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "owner", 1_000, 0)
            .unwrap());
        let intent = EphemeralSessionRecord {
            operation_id: "fork-operation".into(),
            work_item_id: item.id.clone(),
            owner_id: "owner".into(),
            parent_id: Some("parent".into()),
            side_id: Some("fork-session".into()),
            state: "opening".into(),
            last_error: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        };
        assert!(storage.record_ephemeral_session(&intent).unwrap());

        assert!(storage
            .activate_session_from_ephemeral(
                &SessionRecord {
                    id: "fork-session".into(),
                    work_item_id: item.id.clone(),
                    parent_id: Some("parent".into()),
                    active: true,
                    created_at: "2".into(),
                },
                &intent.operation_id,
                "owner",
            )
            .unwrap());

        assert_eq!(
            storage.active_session(&item.id).unwrap().unwrap().id,
            "fork-session"
        );
        assert!(storage.ephemeral_sessions(&item.id).unwrap().is_empty());
    }

    #[test]
    fn ask_delivery_ack_is_persisted_atomically_with_response_start() {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "work".into(),
            name: "demo".into(),
            workspace_root: "/demo".into(),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_opened_at: Some("now".into()),
        };
        storage.upsert_work_item(&item).unwrap();
        storage
            .upsert_repo(&Repo {
                id: "repo".into(),
                work_item_id: item.id.clone(),
                name: "repo".into(),
                path: "/demo".into(),
                remote_pr_url: None,
                pr_meta_json: None,
                base_branch: Some("main".into()),
                base_branch_source: BaseBranchSource::Auto,
                last_activity_at: None,
            })
            .unwrap();
        storage
            .upsert_version(&Version {
                id: "version".into(),
                repo_id: "repo".into(),
                version_num: 0,
                kind: VersionKind::WorkingTree,
                created_at: "now".into(),
                head_sha: "head".into(),
                worktree_path: None,
                last_opened_at: Some("now".into()),
            })
            .unwrap();
        storage
            .add_annotation(
                &Annotation {
                    id: "annotation".into(),
                    repo_id: "repo".into(),
                    kind: AnnotationKind::Ask,
                    file_path: "src/lib.rs".into(),
                    anchor_snippet: "line".into(),
                    anchor_hash: "hash".into(),
                    anchor_start_offset: 0,
                    anchor_line_count: 1,
                    text: None,
                    submitted: false,
                    delivery_state: DeliveryState::Pending,
                    created_at: "now".into(),
                },
                &Placement {
                    annotation_id: "annotation".into(),
                    version_id: "version".into(),
                    side: AnchorSide::New,
                    line_start: 1,
                    line_end: 1,
                    outdated: false,
                    ambiguous: false,
                },
            )
            .unwrap();
        let user = AskMessage {
            id: "user".into(),
            annotation_id: "annotation".into(),
            seq: 0,
            role: "user".into(),
            text: "why?".into(),
            sent: false,
            delivery_state: DeliveryState::Pending,
            ts: "now".into(),
        };
        storage.append_ask_message(&user).unwrap();
        assert_eq!(storage.pending_ask_messages("work").unwrap(), vec![user]);
        storage
            .acknowledge_ask_with_response_start(
                "user",
                &AskMessage {
                    id: "assistant".into(),
                    annotation_id: "annotation".into(),
                    seq: 1,
                    role: "assistant".into(),
                    text: "because".into(),
                    sent: true,
                    delivery_state: DeliveryState::Sent,
                    ts: "later".into(),
                },
            )
            .unwrap();
        assert!(storage.pending_ask_messages("work").unwrap().is_empty());
        assert_eq!(
            storage
                .annotation_by_id("annotation")
                .unwrap()
                .unwrap()
                .delivery_state,
            DeliveryState::Sent
        );
        assert_eq!(
            storage
                .ask_messages_for_annotation("annotation")
                .unwrap()
                .len(),
            2
        );
        storage
            .append_ask_message_delta("assistant", " more")
            .unwrap();
        storage
            .update_ask_message_text("assistant", "exact response")
            .unwrap();
        assert_eq!(
            storage.ask_messages_for_annotation("annotation").unwrap()[1].text,
            "exact response"
        );
        assert!(storage.append_ask_message_delta("missing", "lost").is_err());
        assert!(storage.update_ask_message_text("missing", "lost").is_err());

        let question_session = RevQuestionSession {
            annotation_id: "annotation".into(),
            session_id: "copilot-question-1".into(),
            model_id: "gpt-5".into(),
            reasoning_effort: Some("high".into()),
            context_tier: Some("large".into()),
            state: "ready".into(),
            created_at: "now".into(),
            updated_at: "later".into(),
        };
        storage
            .upsert_rev_question_session(&question_session)
            .unwrap();
        assert_eq!(
            storage.rev_question_session("annotation").unwrap(),
            Some(question_session)
        );
        let operation = storage
            .begin_prune_operation("rev-question-prune", "work", false)
            .unwrap();
        assert!(storage
            .prune_targets(&operation)
            .unwrap()
            .iter()
            .any(|target| {
                target.target_key == "persistent:copilot-question-1"
                    && target.session_id.as_deref() == Some("copilot-question-1")
            }));

        storage.clear_review_history("work").unwrap();
        assert!(storage.annotation_by_id("annotation").unwrap().is_none());
        assert!(storage
            .rev_question_session("annotation")
            .unwrap()
            .is_none());
        assert!(storage.work_item_by_id("work").unwrap().is_some());
    }
}

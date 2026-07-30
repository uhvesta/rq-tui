use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::domain::{
    AnchorSide, Annotation, AnnotationKind, AskMessage, BaseBranchSource, DeliveryState, Placement,
    Repo, ReviewContext, SessionRecord, Version, VersionKind, WorkItem,
};

const MIGRATION_1: &str = include_str!("../migrations/0001_initial.sql");
const MIGRATION_2: &str = include_str!("../migrations/0002_placement_side.sql");

pub(crate) struct Storage {
    connection: Connection,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct ReviewHistoryItem {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) last_opened_at: String,
    pub(crate) versions: usize,
    pub(crate) annotations: usize,
}

impl Storage {
    pub(crate) fn open(path: &Path) -> Result<Self> {
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

    #[cfg(test)]
    pub(crate) fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        Self::configure(&connection)?;
        let storage = Self {
            connection,
            path: PathBuf::from(":memory:"),
        };
        storage.migrate()?;
        Ok(storage)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn set_query_only_for_testing(&self, enabled: bool) -> Result<()> {
        self.connection
            .pragma_update(None, "query_only", enabled)
            .context("cannot change SQLite query-only mode")?;
        Ok(())
    }

    fn configure(connection: &Connection) -> Result<()> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("cannot enable SQLite WAL mode")?;
        Ok(())
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
        for (version, sql) in [(1, MIGRATION_1), (2, MIGRATION_2)] {
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

    pub(crate) fn upsert_work_item(&self, item: &WorkItem) -> Result<()> {
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

    pub(crate) fn list_work_items(&self) -> Result<Vec<WorkItem>> {
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

    pub(crate) fn review_history(&self) -> Result<Vec<ReviewHistoryItem>> {
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

    pub(crate) fn delete_work_item(&self, work_item_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM work_items WHERE id = ?1", [work_item_id])?;
        Ok(())
    }

    pub(crate) fn work_item_by_root(&self, root: &Path) -> Result<Option<WorkItem>> {
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

    pub(crate) fn work_item_by_id(&self, id: &str) -> Result<Option<WorkItem>> {
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

    pub(crate) fn upsert_repo(&self, repo: &Repo) -> Result<()> {
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

    pub(crate) fn repos_for_work_item(&self, work_item_id: &str) -> Result<Vec<Repo>> {
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

    pub(crate) fn upsert_version(&self, version: &Version) -> Result<()> {
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

    pub(crate) fn versions_for_repo(&self, repo_id: &str) -> Result<Vec<Version>> {
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

    pub(crate) fn annotation_counts_for_version(&self, version_id: &str) -> Result<(usize, usize)> {
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

    pub(crate) fn latest_remote_version(&self, repo_id: &str) -> Result<Option<Version>> {
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

    pub(crate) fn mark_version_opened(&self, version_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE versions SET last_opened_at = ?2 WHERE id = ?1",
            params![version_id, now()],
        )?;
        Ok(())
    }

    pub(crate) fn unopened_remote_versions(
        &self,
        repo_id: &str,
        except_version_id: &str,
    ) -> Result<Vec<Version>> {
        Ok(self
            .versions_for_repo(repo_id)?
            .into_iter()
            .filter(|version| {
                version.kind == VersionKind::Remote
                    && version.last_opened_at.is_none()
                    && version.id != except_version_id
            })
            .collect())
    }

    pub(crate) fn delete_version(&self, version_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM versions WHERE id = ?1", [version_id])?;
        Ok(())
    }

    pub(crate) fn activate_session(&self, session: &SessionRecord) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "UPDATE sessions SET active = 0 WHERE work_item_id = ?1",
            [&session.work_item_id],
        )?;
        tx.execute(
            "INSERT INTO sessions(id, work_item_id, parent_id, active, created_at)
             VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(id) DO UPDATE SET active = 1",
            params![
                session.id,
                session.work_item_id,
                session.parent_id,
                session.created_at
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn active_session(&self, work_item_id: &str) -> Result<Option<SessionRecord>> {
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

    pub(crate) fn upsert_context(&self, context: &ReviewContext) -> Result<()> {
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

    pub(crate) fn context_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Option<ReviewContext>> {
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

    pub(crate) fn mark_context_sent(&self, work_item_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE contexts
             SET attached_to_session = 1, delivery_state = 'sent'
             WHERE work_item_id = ?1",
            [work_item_id],
        )?;
        Ok(())
    }

    pub(crate) fn mark_context_delivery(
        &self,
        work_item_id: &str,
        state: DeliveryState,
    ) -> Result<()> {
        self.connection.execute(
            "UPDATE contexts
             SET delivery_state = ?2,
                 attached_to_session = CASE WHEN ?2 = 'sent' THEN 1 ELSE 0 END
             WHERE work_item_id = ?1",
            params![work_item_id, state.as_str()],
        )?;
        Ok(())
    }

    pub(crate) fn add_annotation(
        &self,
        annotation: &Annotation,
        placement: &Placement,
    ) -> Result<()> {
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

    pub(crate) fn annotations_for_version(
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

    pub(crate) fn annotation_by_id(&self, annotation_id: &str) -> Result<Option<Annotation>> {
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

    pub(crate) fn upsert_placement(&self, placement: &Placement) -> Result<()> {
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

    pub(crate) fn update_annotation_file_path(
        &self,
        annotation_id: &str,
        path: &Path,
    ) -> Result<()> {
        self.connection.execute(
            "UPDATE annotations SET file_path = ?2 WHERE id = ?1",
            params![annotation_id, path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub(crate) fn update_annotation_text(&self, annotation_id: &str, text: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE annotations SET text = ?2 WHERE id = ?1",
            params![annotation_id, text],
        )?;
        Ok(())
    }

    pub(crate) fn update_annotation_anchor(&self, annotation: &Annotation) -> Result<()> {
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

    pub(crate) fn placements_for_annotation(&self, annotation_id: &str) -> Result<Vec<Placement>> {
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

    pub(crate) fn delete_annotation(&self, annotation_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM annotations WHERE id = ?1", [annotation_id])?;
        Ok(())
    }

    pub(crate) fn pending_comments_for_work_item(
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

    pub(crate) fn pending_comment_delivery_ids(&self, work_item_id: &str) -> Result<Vec<String>> {
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

    pub(crate) fn mark_comments_delivery(
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

    pub(crate) fn latest_placement_for_annotation(
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

    pub(crate) fn append_ask_message(&self, message: &AskMessage) -> Result<()> {
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

    pub(crate) fn acknowledge_ask_with_response_start(
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

    pub(crate) fn update_latest_ask_response(&self, annotation_id: &str, text: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE ask_messages
             SET text = ?2
             WHERE id = (
                 SELECT id FROM ask_messages
                 WHERE annotation_id = ?1 AND role = 'assistant'
                 ORDER BY seq DESC
                 LIMIT 1
             )",
            params![annotation_id, text],
        )?;
        Ok(())
    }

    pub(crate) fn update_ask_message_text(&self, message_id: &str, text: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE ask_messages SET text = ?2 WHERE id = ?1",
            params![message_id, text],
        )?;
        Ok(())
    }

    pub(crate) fn pending_ask_messages(&self, work_item_id: &str) -> Result<Vec<AskMessage>> {
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

    pub(crate) fn ask_messages_for_annotation(
        &self,
        annotation_id: &str,
    ) -> Result<Vec<AskMessage>> {
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

    pub(crate) fn discard_pending_ask(&self, message_id: &str) -> Result<()> {
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

    pub(crate) fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub(crate) fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }
}

pub(crate) fn now() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::Storage;
    use crate::domain::{
        AnchorSide, Annotation, AnnotationKind, AskMessage, BaseBranchSource, DeliveryState,
        Placement, Repo, Version, VersionKind, WorkItem,
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
                     'annotations', 'placements', 'ask_messages', 'settings',
                     'versions_repo_version', 'placements_version',
                     'annotations_repo_submitted', 'ask_messages_annotation_seq',
                     'work_items_last_opened'
                   )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 14);
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
                "SELECT COUNT(*) FROM schema_migrations WHERE version IN (1, 2)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
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
    }
}

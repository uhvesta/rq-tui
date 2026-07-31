use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub name: String,
    pub workspace_root: PathBuf,
    pub created_at: String,
    pub updated_at: String,
    pub last_opened_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub work_item_id: String,
    pub parent_id: Option<String>,
    pub active: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EphemeralSessionRecord {
    pub operation_id: String,
    pub work_item_id: String,
    pub owner_id: String,
    pub parent_id: Option<String>,
    pub side_id: Option<String>,
    pub state: String,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingChat {
    pub id: String,
    pub work_item_id: String,
    pub text: String,
    pub kind: String,
    pub lane: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewContext {
    pub work_item_id: String,
    pub title: String,
    pub what: String,
    pub why: String,
    pub how: String,
    pub considerations: String,
    pub alternatives: String,
    pub source: String,
    pub attached_to_session: bool,
    pub delivery_state: DeliveryState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    pub work_item_id: String,
    pub name: String,
    pub path: PathBuf,
    pub remote_pr_url: Option<String>,
    pub pr_meta_json: Option<String>,
    pub base_branch: Option<String>,
    pub base_branch_source: BaseBranchSource,
    pub last_activity_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseBranchSource {
    #[default]
    Auto,
    Global,
    PerRepo,
}

impl BaseBranchSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Global => "global",
            Self::PerRepo => "per_repo",
        }
    }
}

impl TryFrom<&str> for BaseBranchSource {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "auto" => Ok(Self::Auto),
            "global" => Ok(Self::Global),
            "per_repo" => Ok(Self::PerRepo),
            other => anyhow::bail!("invalid base-branch source: {other}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionKind {
    Remote,
    WorkingTree,
    Snapshot,
}

impl VersionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::WorkingTree => "working_tree",
            Self::Snapshot => "snapshot",
        }
    }
}

impl TryFrom<&str> for VersionKind {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "remote" => Ok(Self::Remote),
            "working_tree" => Ok(Self::WorkingTree),
            "snapshot" => Ok(Self::Snapshot),
            other => anyhow::bail!("invalid version kind: {other}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub repo_id: String,
    pub version_num: i64,
    pub kind: VersionKind,
    pub created_at: String,
    pub head_sha: String,
    pub worktree_path: Option<PathBuf>,
    pub last_opened_at: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationKind {
    Ask,
    Comment,
}

impl AnnotationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Comment => "comment",
        }
    }
}

impl TryFrom<&str> for AnnotationKind {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "ask" => Ok(Self::Ask),
            "comment" => Ok(Self::Comment),
            other => anyhow::bail!("invalid annotation kind: {other}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationStatus {
    #[default]
    Active,
    Resolved,
    AutoDismissed,
}

impl AnnotationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Resolved => "resolved",
            Self::AutoDismissed => "auto_dismissed",
        }
    }
}

impl TryFrom<&str> for AnnotationStatus {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "active" => Ok(Self::Active),
            "resolved" => Ok(Self::Resolved),
            "auto_dismissed" => Ok(Self::AutoDismissed),
            other => anyhow::bail!("invalid annotation status: {other}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    #[default]
    Draft,
    Pending,
    Sent,
}

impl DeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Pending => "pending",
            Self::Sent => "sent",
        }
    }
}

impl TryFrom<&str> for DeliveryState {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "draft" => Ok(Self::Draft),
            "pending" => Ok(Self::Pending),
            "sent" => Ok(Self::Sent),
            other => anyhow::bail!("invalid delivery state: {other}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorSide {
    Old,
    #[default]
    New,
}

impl AnchorSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }
}

impl TryFrom<&str> for AnchorSide {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "old" => Ok(Self::Old),
            "new" => Ok(Self::New),
            other => anyhow::bail!("invalid anchor side: {other}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    pub id: String,
    pub repo_id: String,
    pub kind: AnnotationKind,
    pub file_path: PathBuf,
    pub anchor_snippet: String,
    pub anchor_hash: String,
    pub anchor_start_offset: i64,
    pub anchor_line_count: i64,
    pub text: Option<String>,
    pub submitted: bool,
    pub delivery_state: DeliveryState,
    #[serde(default)]
    pub status: AnnotationStatus,
    #[serde(default)]
    pub status_reason: Option<String>,
    #[serde(default)]
    pub status_changed_at: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub annotation_id: String,
    pub version_id: String,
    pub side: AnchorSide,
    pub line_start: i64,
    pub line_end: i64,
    pub outdated: bool,
    pub ambiguous: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskMessage {
    pub id: String,
    pub annotation_id: String,
    pub seq: i64,
    pub role: String,
    pub text: String,
    pub sent: bool,
    pub delivery_state: DeliveryState,
    pub ts: String,
}

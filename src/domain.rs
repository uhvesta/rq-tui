use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkItem {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) workspace_root: PathBuf,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) last_opened_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionRecord {
    pub(crate) id: String,
    pub(crate) work_item_id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) active: bool,
    pub(crate) created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingChat {
    pub(crate) id: String,
    pub(crate) work_item_id: String,
    pub(crate) text: String,
    pub(crate) created_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewContext {
    pub(crate) work_item_id: String,
    pub(crate) title: String,
    pub(crate) what: String,
    pub(crate) why: String,
    pub(crate) how: String,
    pub(crate) considerations: String,
    pub(crate) alternatives: String,
    pub(crate) source: String,
    pub(crate) attached_to_session: bool,
    pub(crate) delivery_state: DeliveryState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Repo {
    pub(crate) id: String,
    pub(crate) work_item_id: String,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) remote_pr_url: Option<String>,
    pub(crate) pr_meta_json: Option<String>,
    pub(crate) base_branch: Option<String>,
    pub(crate) base_branch_source: BaseBranchSource,
    pub(crate) last_activity_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BaseBranchSource {
    #[default]
    Auto,
    Global,
    PerRepo,
}

impl BaseBranchSource {
    pub(crate) fn as_str(self) -> &'static str {
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
pub(crate) enum VersionKind {
    Remote,
    WorkingTree,
    Snapshot,
}

impl VersionKind {
    pub(crate) fn as_str(self) -> &'static str {
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
pub(crate) struct Version {
    pub(crate) id: String,
    pub(crate) repo_id: String,
    pub(crate) version_num: i64,
    pub(crate) kind: VersionKind,
    pub(crate) created_at: String,
    pub(crate) head_sha: String,
    pub(crate) worktree_path: Option<PathBuf>,
    pub(crate) last_opened_at: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnnotationKind {
    Ask,
    Comment,
}

impl AnnotationKind {
    pub(crate) fn as_str(self) -> &'static str {
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
pub(crate) enum DeliveryState {
    #[default]
    Draft,
    Pending,
    Sent,
}

impl DeliveryState {
    pub(crate) fn as_str(self) -> &'static str {
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
pub(crate) enum AnchorSide {
    Old,
    #[default]
    New,
}

impl AnchorSide {
    pub(crate) fn as_str(self) -> &'static str {
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
pub(crate) struct Annotation {
    pub(crate) id: String,
    pub(crate) repo_id: String,
    pub(crate) kind: AnnotationKind,
    pub(crate) file_path: PathBuf,
    pub(crate) anchor_snippet: String,
    pub(crate) anchor_hash: String,
    pub(crate) anchor_start_offset: i64,
    pub(crate) anchor_line_count: i64,
    pub(crate) text: Option<String>,
    pub(crate) submitted: bool,
    pub(crate) delivery_state: DeliveryState,
    pub(crate) created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Placement {
    pub(crate) annotation_id: String,
    pub(crate) version_id: String,
    pub(crate) side: AnchorSide,
    pub(crate) line_start: i64,
    pub(crate) line_end: i64,
    pub(crate) outdated: bool,
    pub(crate) ambiguous: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AskMessage {
    pub(crate) id: String,
    pub(crate) annotation_id: String,
    pub(crate) seq: i64,
    pub(crate) role: String,
    pub(crate) text: String,
    pub(crate) sent: bool,
    pub(crate) delivery_state: DeliveryState,
    pub(crate) ts: String,
}

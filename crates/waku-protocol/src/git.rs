use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::ProviderKind;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct BranchEntry {
    pub name: String,
    pub checked_out_elsewhere: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct BranchSnapshot {
    #[ts(type = "string")]
    pub repository: PathBuf,
    pub current: Option<String>,
    pub detached_head: Option<String>,
    pub default_branch: Option<String>,
    pub branches: Vec<BranchEntry>,
    pub additions: u64,
    pub deletions: u64,
}

impl BranchSnapshot {
    pub fn display_branch(&self) -> Option<&str> {
        self.current.as_deref().or(self.detached_head.as_deref())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct CommitSnapshot {
    pub branch: String,
    pub additions: u64,
    pub deletions: u64,
    pub staged_additions: u64,
    pub staged_deletions: u64,
    pub has_staged: bool,
    pub has_unstaged: bool,
    pub can_push: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct AgentInvocation {
    pub provider: ProviderKind,
    #[ts(type = "string")]
    pub binary: PathBuf,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct CreatedWorktree {
    #[ts(type = "string")]
    pub path: PathBuf,
    pub branch: String,
}

/// How a path changed, in a commit or in the uncommitted row.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum GitGraphFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    TypeChanged,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct GitGraphFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: GitGraphFileStatus,
}

/// A ref painted as a capsule on a commit. `current` is also written out as
/// text in the UI — the color of the capsule is not the only signal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum GitGraphRefKind {
    /// Detached `HEAD`, when no branch is checked out.
    Head,
    Branch,
    Remote,
    Tag,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct GitGraphRef {
    pub kind: GitGraphRefKind,
    pub name: String,
    pub current: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct GitGraphCommit {
    pub hash: String,
    pub parents: Vec<String>,
    pub author: String,
    pub authored_at: i64,
    pub subject: String,
    pub refs: Vec<GitGraphRef>,
}

/// One column of a commit row. `node` is the commit dot; other columns are
/// lines passing through. `up` / `down` say which half of the row the line
/// occupies. A curve between columns is a [`GitGraphEdge`], not a lane.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct GitGraphLane {
    pub color: u8,
    pub up: bool,
    pub down: bool,
    pub node: bool,
}

/// A curve from the commit dot (`from`) to another column's bottom (`to`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct GitGraphEdge {
    pub from: u16,
    pub to: u16,
    pub color: u8,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct GitGraphRow {
    pub node_lane: u16,
    pub lanes: Vec<GitGraphLane>,
    pub edges: Vec<GitGraphEdge>,
}

/// One repository's graph, already laid out. The client paints `rows` and
/// never walks parents on a frame.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct GitGraphSnapshot {
    pub commits: Vec<GitGraphCommit>,
    pub rows: Vec<GitGraphRow>,
    pub uncommitted: Vec<GitGraphFile>,
    /// Index into `commits` of the checked-out commit, when it is loaded.
    pub head_row: Option<u32>,
    /// Widest row, so the client sizes the graph column without a scan.
    pub lane_count: u16,
    pub truncated: bool,
    pub limit: u32,
}

/// The body and file list of one commit, loaded only after that row is selected.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct GitCommitDetail {
    pub hash: String,
    pub body: String,
    pub files: Vec<GitGraphFile>,
}

/// Which two trees a Git Graph diff compares. A commit compares against its
/// first parent (or the empty tree for a root commit); `Uncommitted` compares
/// the working tree, index included, against `HEAD`.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum GitDiffSpec {
    Uncommitted,
    Commit {
        hash: String,
    },
    /// `from` is the older side. Both are full object ids.
    Compare {
        from: String,
        to: String,
    },
}

/// One file's patch for the Git Graph's inline diff. Large patches are cut at
/// a line budget so a generated file cannot flood the wire or the pane.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct GitFileDiff {
    pub patch: String,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum GitResetMode {
    Soft,
    Mixed,
    Hard,
}

/// A repository change started from the Git Graph's context menus. Commit
/// arguments are full object ids; ref arguments are short names exactly as
/// the graph's capsules show them, and the daemon validates both before
/// anything reaches `git`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum GitGraphAction {
    CheckoutCommit {
        hash: String,
    },
    CheckoutBranch {
        name: String,
    },
    /// Check out `remote/branch` as a local tracking branch, or switch to the
    /// local branch of the same name when it already exists.
    CheckoutRemoteBranch {
        name: String,
    },
    CreateBranch {
        name: String,
        start: String,
        checkout: bool,
    },
    RenameBranch {
        from: String,
        to: String,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    DeleteRemoteBranch {
        name: String,
    },
    PushBranch {
        name: String,
    },
    CreateTag {
        name: String,
        target: String,
        message: Option<String>,
    },
    DeleteTag {
        name: String,
    },
    PushTag {
        name: String,
    },
    /// Merge a commit or ref into the checked-out branch.
    Merge {
        rev: String,
    },
    /// Rebase the checked-out branch onto a commit or ref.
    Rebase {
        onto: String,
    },
    CherryPick {
        hash: String,
    },
    Revert {
        hash: String,
    },
    Reset {
        hash: String,
        mode: GitResetMode,
    },
    Stash,
    DiscardUncommitted,
}

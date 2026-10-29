use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use crate::composer::{FileEntry, SlashCommand};
use crate::git::{
    AgentInvocation, BranchSnapshot, CommitSnapshot, CreatedWorktree, GitCommitDetail, GitDiffSpec,
    GitFileDiff, GitGraphAction, GitGraphFile, GitGraphSnapshot,
};
use crate::model::{Checkpoint, ProviderKind};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ReviewDiffSource {
    LastTurn {
        session_id: Uuid,
        turn_id: Uuid,
        turn_count: usize,
    },
    Uncommitted,
    Unstaged,
    Staged,
    Committed,
    Branch,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffData {
    pub source: ReviewDiffSource,
    pub numstat: String,
    pub patch: String,
    pub complete_context: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WorkingTreeEntry {
    pub relative_path: String,
    #[ts(type = "string")]
    pub absolute_path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub expanded: bool,
    pub depth: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkspaceOperation {
    /// Renew a short native-watch lease and read its in-memory change counters.
    WatchChanges {
        client_id: Uuid,
        #[ts(type = "string")]
        root: PathBuf,
        /// Non-recursive directories visible in Files, relative to root.
        #[ts(type = "string[]")]
        directories: Vec<PathBuf>,
        /// Live Review needs the non-ignored source tree; Files alone does not.
        recursive: bool,
        watch_git: bool,
    },
    ListTree {
        #[ts(type = "string")]
        root: PathBuf,
        #[ts(type = "string[]")]
        expanded_paths: Vec<PathBuf>,
    },
    BrowseDirectory {
        #[ts(type = "string | null")]
        path: Option<PathBuf>,
    },
    ReadTextFile {
        #[ts(type = "string")]
        root: PathBuf,
        #[ts(type = "string")]
        relative_path: PathBuf,
    },
    WriteTextFile {
        #[ts(type = "string")]
        root: PathBuf,
        #[ts(type = "string")]
        relative_path: PathBuf,
        content: String,
    },
    /// Reads one bounded chunk of a file starting at `offset`, so arbitrarily
    /// large files travel as many small messages instead of one that the wire
    /// limit rejects. `total_bytes` on the response carries the current file
    /// length, letting callers drive progress without a separate stat.
    ReadFileChunk {
        #[ts(type = "string")]
        root: PathBuf,
        #[ts(type = "string")]
        relative_path: PathBuf,
        offset: u64,
        /// Bytes requested; the daemon clamps this to its own per-chunk cap.
        length: u64,
    },
    RenamePath {
        #[ts(type = "string")]
        root: PathBuf,
        #[ts(type = "string")]
        from_relative_path: PathBuf,
        #[ts(type = "string")]
        to_relative_path: PathBuf,
    },
    CreateFile {
        #[ts(type = "string")]
        root: PathBuf,
        #[ts(type = "string")]
        relative_path: PathBuf,
    },
    CreateDirectory {
        #[ts(type = "string")]
        root: PathBuf,
        #[ts(type = "string")]
        relative_path: PathBuf,
    },
    DeletePath {
        #[ts(type = "string")]
        root: PathBuf,
        #[ts(type = "string")]
        relative_path: PathBuf,
    },
    ListProjectFiles {
        #[ts(type = "string")]
        root: PathBuf,
        cap: usize,
    },
    DiscoverSlashCommands {
        provider: ProviderKind,
        #[ts(type = "string")]
        project_root: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binary_override: Option<String>,
    },
    CreateProjectlessWorkspace {
        prompt: Option<String>,
    },
    MigrateProjectlessWorkspace {
        #[ts(type = "string")]
        path: PathBuf,
    },
    InspectBranches {
        #[ts(type = "string")]
        cwd: PathBuf,
    },
    CheckoutBranch {
        #[ts(type = "string")]
        cwd: PathBuf,
        branch: String,
        create: bool,
    },
    CreateWorktree {
        #[ts(type = "string")]
        project_path: PathBuf,
        project_id: Uuid,
        session_id: Uuid,
        prompt: String,
        base_branch: Option<String>,
    },
    InspectCommit {
        #[ts(type = "string")]
        cwd: PathBuf,
    },
    GenerateCommitMessage {
        #[ts(type = "string")]
        cwd: PathBuf,
        include_unstaged: bool,
        invocation: AgentInvocation,
    },
    Commit {
        #[ts(type = "string")]
        cwd: PathBuf,
        message: String,
        include_unstaged: bool,
        push: bool,
    },
    Push {
        #[ts(type = "string")]
        cwd: PathBuf,
    },
    CaptureTurnStart {
        #[ts(type = "string")]
        cwd: PathBuf,
        session_id: Uuid,
        turn_count: usize,
    },
    CaptureTurn {
        #[ts(type = "string")]
        cwd: PathBuf,
        session_id: Uuid,
        turn_count: usize,
    },
    CaptureRef {
        #[ts(type = "string")]
        cwd: PathBuf,
        git_ref: String,
    },
    RestoreRef {
        #[ts(type = "string")]
        cwd: PathBuf,
        git_ref: String,
    },
    HasRef {
        #[ts(type = "string")]
        cwd: PathBuf,
        git_ref: String,
    },
    SessionTurnRefs {
        #[ts(type = "string")]
        cwd: PathBuf,
        session_id: Uuid,
    },
    DeleteRef {
        #[ts(type = "string")]
        cwd: PathBuf,
        git_ref: String,
    },
    DeleteTurnRefsAfter {
        #[ts(type = "string")]
        cwd: PathBuf,
        session_id: Uuid,
        retained_turn_count: usize,
        previous_turn_count: usize,
    },
    DeleteSessionRefs {
        #[ts(type = "string")]
        cwd: PathBuf,
        session_id: Uuid,
    },
    CopySessionRefs {
        #[ts(type = "string")]
        cwd: PathBuf,
        source_session_id: Uuid,
        target_session_id: Uuid,
        through_turn_count: usize,
    },
    CollectReviewDiff {
        #[ts(type = "string")]
        cwd: PathBuf,
        source: ReviewDiffSource,
    },
    /// Commit graph for the right-panel Git Graph. `limit` is a page size;
    /// the daemon clamps it.
    InspectGitGraph {
        #[ts(type = "string")]
        cwd: PathBuf,
        limit: u32,
    },
    /// Message body and name-status for one commit. The hash is a full object
    /// id; the daemon rejects anything that is not hex.
    InspectGitCommit {
        #[ts(type = "string")]
        cwd: PathBuf,
        hash: String,
    },
    /// Name-status between two commits, for the Git Graph's compare view.
    InspectGitCompare {
        #[ts(type = "string")]
        cwd: PathBuf,
        from: String,
        to: String,
    },
    /// One file's patch for the Git Graph's inline diff. `old_path` is the
    /// rename source, so the patch pairs both sides.
    InspectGitFileDiff {
        #[ts(type = "string")]
        cwd: PathBuf,
        spec: GitDiffSpec,
        path: String,
        old_path: Option<String>,
    },
    /// Runs one Git Graph context-menu action and answers with the fresh
    /// branch snapshot, since most actions move `HEAD` or a branch.
    RunGitGraphAction {
        #[ts(type = "string")]
        cwd: PathBuf,
        action: GitGraphAction,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkspaceResult {
    Changes {
        state: WorkspaceWatchState,
    },
    Ack,
    WorkingTree {
        entries: Vec<WorkingTreeEntry>,
    },
    Directory {
        #[ts(type = "string")]
        path: PathBuf,
        #[ts(type = "string | null")]
        parent: Option<PathBuf>,
        #[ts(type = "string")]
        home: PathBuf,
        #[ts(type = "string")]
        filesystem_root: PathBuf,
        entries: Vec<WorkingTreeEntry>,
    },
    TextFile {
        content: String,
    },
    FileChunk {
        /// Base64-encoded file bytes at the requested offset, matching the
        /// blob command framing.
        bytes: String,
        /// File length in bytes when the chunk was read.
        total_bytes: u64,
    },
    ProjectFiles {
        entries: Vec<FileEntry>,
    },
    SlashCommands {
        commands: Vec<SlashCommand>,
    },
    ProjectlessWorkspace {
        #[ts(type = "string")]
        cwd: PathBuf,
    },
    Branches {
        snapshot: Option<BranchSnapshot>,
    },
    BranchChanged {
        snapshot: BranchSnapshot,
    },
    WorktreeCreated {
        worktree: CreatedWorktree,
    },
    CommitSnapshot {
        snapshot: CommitSnapshot,
    },
    CommitMessage {
        message: String,
    },
    Checkpoint {
        checkpoint: Checkpoint,
    },
    Bool {
        value: bool,
    },
    TurnRefs {
        turn_counts: Vec<usize>,
    },
    ReviewDiff {
        data: ReviewDiffData,
    },
    GitGraph {
        snapshot: Option<GitGraphSnapshot>,
    },
    GitCommit {
        detail: GitCommitDetail,
    },
    GitCompare {
        files: Vec<GitGraphFile>,
    },
    GitFileDiff {
        diff: GitFileDiff,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceWatchState {
    pub epoch: Uuid,
    pub tree: u64,
    pub content: u64,
    pub worktree: u64,
    pub git: u64,
    pub ready: bool,
    /// Native watching failed or reached its resource budget. Manual refresh
    /// and activation refresh remain available; no full-tree polling fallback.
    pub limited: bool,
}

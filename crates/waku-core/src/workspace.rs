//! Daemon-owned workspace filesystem and Git API.
//!
//! Paths in this module always name resources on the daemon host. A client
//! may display them, but must never reinterpret them against its own machine.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Output;

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;

const MAX_HYDRATED_PATCH_BYTES: usize = 32 * 1024 * 1024;
/// One chunk of a streamed file download, before base64. The 4/3 inflation
/// plus the JSON envelope keeps even a maximum chunk far below the daemon
/// wire's per-message cap, so no whole-file read can ever be assembled into
/// a single oversized response.
const MAX_FILE_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

pub use waku_protocol::workspace::{
    ReviewDiffData, ReviewDiffSource, WorkingTreeEntry, WorkspaceOperation, WorkspaceResult,
};

pub fn execute(operation: WorkspaceOperation) -> anyhow::Result<WorkspaceResult> {
    Ok(match operation {
        WorkspaceOperation::WatchChanges {
            client_id,
            root,
            directories,
            recursive,
            watch_git,
        } => WorkspaceResult::Changes {
            state: crate::workspace_watch::poll(
                client_id,
                root,
                directories,
                recursive,
                watch_git,
            )?,
        },
        WorkspaceOperation::ListTree {
            root,
            expanded_paths,
        } => WorkspaceResult::WorkingTree {
            entries: list_tree(&root, &expanded_paths.into_iter().collect()),
        },
        WorkspaceOperation::BrowseDirectory { path } => {
            let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory is unavailable"))?;
            #[cfg(windows)]
            if path.as_deref() == Some(Path::new("")) {
                return Ok(WorkspaceResult::Directory {
                    path: PathBuf::new(),
                    parent: None,
                    entries: list_logical_drives(),
                    home,
                    filesystem_root: PathBuf::new(),
                });
            }
            #[cfg(windows)]
            let requested = path.as_deref().map(|candidate| {
                let raw = candidate.to_string_lossy();
                let bytes = raw.as_bytes();
                if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                    PathBuf::from(format!("{raw}\\"))
                } else {
                    candidate.to_path_buf()
                }
            });
            #[cfg(not(windows))]
            let requested = path;
            let target = requested.as_deref().unwrap_or(&home);
            let path = strip_windows_verbatim(
                fs::canonicalize(target)
                    .with_context(|| format!("could not open directory {}", target.display()))?,
            );
            if !fs::metadata(&path)?.is_dir() {
                bail!("not a directory: {}", path.display());
            }
            let filesystem_root = path
                .ancestors()
                .last()
                .map(Path::to_owned)
                .unwrap_or_else(|| path.clone());
            #[cfg(windows)]
            let parent = Some(path.parent().map(Path::to_owned).unwrap_or_default());
            #[cfg(not(windows))]
            let parent = path.parent().map(Path::to_owned);
            WorkspaceResult::Directory {
                parent,
                entries: list_directory(&path)?,
                path,
                home,
                filesystem_root,
            }
        }
        WorkspaceOperation::ResolvePath { path } => WorkspaceResult::ResolvedPath {
            path: resolve_workspace_directory(&path)?
                .to_string_lossy()
                .into_owned(),
        },
        WorkspaceOperation::ReadTextFile {
            root,
            relative_path,
        } => WorkspaceResult::TextFile {
            content: fs::read_to_string(resolve_workspace_path(&root, &relative_path)?)?,
        },
        WorkspaceOperation::WriteTextFile {
            root,
            relative_path,
            content,
        } => {
            fs::write(resolve_workspace_path(&root, &relative_path)?, content)?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::ReadFileChunk {
            root,
            relative_path,
            offset,
            length,
        } => {
            let path = resolve_workspace_path(&root, &relative_path)?;
            let metadata = fs::metadata(&path)
                .with_context(|| format!("could not open file {}", path.display()))?;
            if metadata.is_dir() {
                bail!("not a file: {}", path.display());
            }
            let total_bytes = metadata.len();
            let remaining = total_bytes.saturating_sub(offset);
            let wanted = (length as usize)
                .min(MAX_FILE_CHUNK_BYTES)
                .min(remaining as usize);
            let mut buffer = vec![0u8; wanted];
            let mut file = fs::File::open(&path)
                .with_context(|| format!("could not open file {}", path.display()))?;
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(offset))
                .with_context(|| format!("could not seek in file {}", path.display()))?;
            let mut filled = 0;
            while filled < wanted {
                match std::io::Read::read(&mut file, &mut buffer[filled..]) {
                    Ok(0) => break,
                    Ok(read) => filled += read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        return Err(anyhow!(error))
                            .with_context(|| format!("could not read file {}", path.display()));
                    }
                }
            }
            buffer.truncate(filled);
            WorkspaceResult::FileChunk {
                bytes: base64::engine::general_purpose::STANDARD.encode(buffer),
                total_bytes,
            }
        }
        WorkspaceOperation::CheckPathsExist {
            root,
            relative_paths,
        } => WorkspaceResult::ExistingPaths {
            relative_paths: check_paths_exist(&root, relative_paths)?,
        },
        WorkspaceOperation::EnsureDirectory {
            root,
            relative_path,
            overwrite,
        } => {
            ensure_directory(&root, &relative_path, overwrite)?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::WriteFileChunk {
            root,
            relative_path,
            offset,
            bytes,
            overwrite,
        } => {
            write_file_chunk(&root, &relative_path, offset, &bytes, overwrite)?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::RenamePath {
            root,
            from_relative_path,
            to_relative_path,
        } => {
            let from = resolve_workspace_path(&root, &from_relative_path)?;
            let to = resolve_workspace_path(&root, &to_relative_path)?;
            if fs::symlink_metadata(&to).is_ok() {
                bail!(
                    "{} already exists in the destination directory",
                    to.display()
                );
            }
            fs::rename(&from, &to)
                .with_context(|| format!("could not rename {}", from.display()))?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::CreateFile {
            root,
            relative_path,
        } => {
            let path = resolve_workspace_path(&root, &relative_path)?;
            if fs::symlink_metadata(&path).is_ok() {
                bail!("{} already exists", path.display());
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("could not create directory {}", parent.display()))?;
            }
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("could not create file {}", path.display()))?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::CreateDirectory {
            root,
            relative_path,
        } => {
            let path = resolve_workspace_path(&root, &relative_path)?;
            if fs::symlink_metadata(&path).is_ok() {
                bail!("{} already exists", path.display());
            }
            fs::create_dir_all(&path)
                .with_context(|| format!("could not create directory {}", path.display()))?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::DeletePath {
            root,
            relative_path,
        } => {
            let path = resolve_workspace_path(&root, &relative_path)?;
            if fs::symlink_metadata(&path)?.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("could not remove directory {}", path.display()))?;
            } else {
                fs::remove_file(&path)
                    .with_context(|| format!("could not remove file {}", path.display()))?;
            }
            WorkspaceResult::Ack
        }
        WorkspaceOperation::ListProjectFiles { root, cap } => WorkspaceResult::ProjectFiles {
            entries: crate::composer_complete::list_project_files(&root, cap),
        },
        WorkspaceOperation::DiscoverSlashCommands {
            provider,
            project_root,
            binary_override,
            config_dir,
        } => WorkspaceResult::SlashCommands {
            commands: crate::composer_complete::discover_slash_commands_for_account(
                provider,
                &project_root,
                binary_override.as_deref(),
                config_dir.as_deref(),
            ),
        },
        WorkspaceOperation::CreateProjectlessWorkspace { prompt } => {
            WorkspaceResult::ProjectlessWorkspace {
                cwd: crate::projectless::create_workspace(prompt.as_deref())?.cwd,
            }
        }
        WorkspaceOperation::MigrateProjectlessWorkspace { path } => {
            WorkspaceResult::ProjectlessWorkspace {
                cwd: crate::projectless::migrate_workspace(&path)?.cwd,
            }
        }
        WorkspaceOperation::InspectBranches { cwd } => WorkspaceResult::Branches {
            snapshot: crate::git_branch::inspect(&cwd)?,
        },
        WorkspaceOperation::CheckoutBranch {
            cwd,
            branch,
            create,
        } => WorkspaceResult::BranchChanged {
            snapshot: if create {
                crate::git_branch::create_and_checkout(&cwd, &branch)?
            } else {
                crate::git_branch::checkout(&cwd, &branch)?
            },
        },
        WorkspaceOperation::CreateWorktree {
            project_path,
            project_id,
            session_id,
            prompt,
            base_branch,
        } => WorkspaceResult::WorktreeCreated {
            worktree: crate::worktree::create(
                &project_path,
                project_id,
                session_id,
                &prompt,
                base_branch.as_deref(),
            )?,
        },
        WorkspaceOperation::InspectCommit { cwd } => WorkspaceResult::CommitSnapshot {
            snapshot: crate::git_commit::inspect(&cwd)?,
        },
        WorkspaceOperation::GenerateCommitMessage {
            cwd,
            include_unstaged,
            invocation,
        } => WorkspaceResult::CommitMessage {
            message: crate::git_commit::generate_message(&cwd, include_unstaged, &invocation)?,
        },
        WorkspaceOperation::Commit {
            cwd,
            message,
            include_unstaged,
            push,
        } => {
            crate::git_commit::commit(&cwd, &message, include_unstaged)?;
            if push {
                crate::git_commit::push(&cwd)?;
            }
            WorkspaceResult::Ack
        }
        WorkspaceOperation::Push { cwd } => {
            crate::git_commit::push(&cwd)?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::CaptureTurnStart {
            cwd,
            session_id,
            turn_count,
        } => {
            crate::checkpoint::capture_turn_start(&cwd, session_id, turn_count)?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::CaptureTurn {
            cwd,
            session_id,
            turn_count,
        } => WorkspaceResult::Checkpoint {
            checkpoint: crate::checkpoint::capture_turn(&cwd, session_id, turn_count)?,
        },
        WorkspaceOperation::CaptureRef { cwd, git_ref } => {
            crate::checkpoint::capture_ref(&cwd, &git_ref)?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::RestoreRef { cwd, git_ref } => {
            crate::checkpoint::restore_ref(&cwd, &git_ref)?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::HasRef { cwd, git_ref } => WorkspaceResult::Bool {
            value: crate::checkpoint::has_ref(&cwd, &git_ref),
        },
        WorkspaceOperation::SessionTurnRefs { cwd, session_id } => {
            let mut turn_counts = crate::checkpoint::session_turn_refs(&cwd, session_id)
                .into_iter()
                .collect::<Vec<_>>();
            turn_counts.sort_unstable();
            WorkspaceResult::TurnRefs { turn_counts }
        }
        WorkspaceOperation::DeleteRef { cwd, git_ref } => {
            crate::checkpoint::delete_ref(&cwd, &git_ref)?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::DeleteTurnRefsAfter {
            cwd,
            session_id,
            retained_turn_count,
            previous_turn_count,
        } => {
            crate::checkpoint::delete_turn_refs_after(
                &cwd,
                session_id,
                retained_turn_count,
                previous_turn_count,
            )?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::DeleteSessionRefs { cwd, session_id } => {
            crate::checkpoint::delete_all_session_refs(&cwd, session_id)?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::CopySessionRefs {
            cwd,
            source_session_id,
            target_session_id,
            through_turn_count,
        } => {
            crate::checkpoint::copy_session_refs(
                &cwd,
                source_session_id,
                target_session_id,
                through_turn_count,
            )?;
            WorkspaceResult::Ack
        }
        WorkspaceOperation::CollectReviewDiff { cwd, source } => WorkspaceResult::ReviewDiff {
            data: collect_review_diff(&cwd, source)?,
        },
        WorkspaceOperation::InspectGitGraph { cwd, limit } => WorkspaceResult::GitGraph {
            snapshot: crate::git_graph::inspect(&cwd, limit)?,
        },
        WorkspaceOperation::InspectGitCommit { cwd, hash } => WorkspaceResult::GitCommit {
            detail: crate::git_graph::commit_detail(&cwd, &hash)?,
        },
        WorkspaceOperation::InspectGitCompare { cwd, from, to } => WorkspaceResult::GitCompare {
            files: crate::git_graph_ops::compare_files(&cwd, &from, &to)?,
        },
        WorkspaceOperation::InspectGitFileDiff {
            cwd,
            spec,
            path,
            old_path,
        } => WorkspaceResult::GitFileDiff {
            diff: crate::git_graph_ops::file_diff(&cwd, &spec, &path, old_path.as_deref())?,
        },
        WorkspaceOperation::RunGitGraphAction { cwd, action } => WorkspaceResult::Branches {
            snapshot: crate::git_graph_ops::run_action(&cwd, &action)?,
        },
        WorkspaceOperation::ReadAntigravitySwarm { session_id } => {
            WorkspaceResult::AntigravitySwarm {
                snapshot: crate::antigravity_swarm::read_swarm_snapshot(&session_id)?,
            }
        }
    })
}

fn resolve_workspace_path(root: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        bail!("workspace path must be a non-empty relative path");
    }
    let mut resolved = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!("workspace path must be a non-empty relative path");
        };
        resolved.push(part);
    }
    Ok(resolved)
}

/// Resolves `relative` inside `root` while refusing to traverse any existing
/// intermediate parent directory that is a symbolic link.
fn resolve_upload_workspace_path(root: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
    let path = resolve_workspace_path(root, relative)?;
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("refusing to traverse symlink outside workspace");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

/// Removes a regular file or symlink (including directory symlinks on Windows)
/// without traversing into a symlink target.
fn remove_non_dir(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt as _;
        if metadata.file_type().is_symlink_dir() {
            fs::remove_dir(path)
                .with_context(|| format!("could not remove symlink {}", path.display()))?;
            return Ok(());
        }
    }
    #[cfg(not(windows))]
    let _ = metadata;
    fs::remove_file(path).with_context(|| format!("could not remove {}", path.display()))?;
    Ok(())
}

fn check_paths_exist(root: &Path, relative_paths: Vec<PathBuf>) -> anyhow::Result<Vec<PathBuf>> {
    let mut existing = Vec::new();
    for relative_path in relative_paths {
        let path = resolve_workspace_path(root, &relative_path)?;
        if fs::symlink_metadata(&path).is_ok() {
            existing.push(relative_path);
        }
    }
    Ok(existing)
}

fn ensure_directory(root: &Path, relative_path: &Path, overwrite: bool) -> anyhow::Result<()> {
    let path = resolve_upload_workspace_path(root, relative_path)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_dir() => return Ok(()),
        Ok(_) if !overwrite => bail!("{} already exists", path.display()),
        Ok(metadata) => {
            remove_non_dir(&path, &metadata)?;
            fs::create_dir_all(&path)
                .with_context(|| format!("could not create directory {}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&path)
                .with_context(|| format!("could not create directory {}", path.display()))?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn write_file_chunk(
    root: &Path,
    relative_path: &Path,
    offset: u64,
    bytes: &str,
    overwrite: bool,
) -> anyhow::Result<()> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(bytes.trim())
        .context("invalid base64 file chunk")?;
    if decoded.len() > MAX_FILE_CHUNK_BYTES {
        bail!(
            "file chunk of {} bytes exceeds maximum {} bytes",
            decoded.len(),
            MAX_FILE_CHUNK_BYTES
        );
    }
    let path = resolve_upload_workspace_path(root, relative_path)?;
    use std::io::{Seek, SeekFrom, Write};
    if offset == 0 {
        match fs::symlink_metadata(&path) {
            Ok(_) if !overwrite => bail!("{} already exists", path.display()),
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    remove_non_dir(&path, &metadata)?;
                } else if metadata.file_type().is_dir() {
                    fs::remove_dir_all(&path).with_context(|| {
                        format!("could not remove directory {}", path.display())
                    })?;
                } else if !metadata.file_type().is_file() {
                    bail!("not a regular file: {}", path.display());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create directory {}", parent.display()))?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("could not open file {}", path.display()))?;
        file.write_all(&decoded)
            .with_context(|| format!("could not write file {}", path.display()))?;
        return Ok(());
    }

    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("could not open file {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("not a regular file: {}", path.display());
    }
    if metadata.len() != offset {
        bail!(
            "upload offset {offset} does not match current file length {}",
            metadata.len()
        );
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .with_context(|| format!("could not open file {}", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("could not seek in file {}", path.display()))?;
    file.write_all(&decoded)
        .with_context(|| format!("could not write file {}", path.display()))?;
    Ok(())
}

fn list_tree(root: &Path, expanded_paths: &HashSet<PathBuf>) -> Vec<WorkingTreeEntry> {
    fn visit(
        directory: &Path,
        relative_directory: &Path,
        depth: usize,
        expanded_paths: &HashSet<PathBuf>,
        output: &mut Vec<WorkingTreeEntry>,
    ) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        let mut children = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    return None;
                }
                let is_dir = entry.file_type().ok()?.is_dir();
                Some((entry.path(), name, is_dir))
            })
            .collect::<Vec<_>>();
        children.sort_by_key(|(_, name, is_dir)| (!*is_dir, name.to_lowercase()));
        for (absolute_path, name, is_dir) in children {
            let relative_path = relative_directory.join(&name);
            let expanded = is_dir && expanded_paths.contains(&absolute_path);
            output.push(WorkingTreeEntry {
                relative_path: relative_path.to_string_lossy().into_owned(),
                absolute_path: absolute_path.clone(),
                name,
                is_dir,
                expanded,
                depth,
            });
            if expanded {
                visit(
                    &absolute_path,
                    &relative_path,
                    depth + 1,
                    expanded_paths,
                    output,
                );
            }
        }
    }
    let mut output = Vec::new();
    visit(root, Path::new(""), 0, expanded_paths, &mut output);
    output
}

fn list_directory(directory: &Path) -> anyhow::Result<Vec<WorkingTreeEntry>> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("could not read directory {}", directory.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = fs::metadata(entry.path()).ok()?.is_dir();
            Some(WorkingTreeEntry {
                relative_path: name.clone(),
                absolute_path: entry.path(),
                name,
                is_dir,
                expanded: false,
                depth: 0,
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| (!entry.is_dir, entry.name.to_lowercase()));
    Ok(entries)
}

#[derive(Clone, Debug)]
struct DiffRange {
    from: String,
    to: String,
}

fn collect_review_diff(cwd: &Path, source: ReviewDiffSource) -> anyhow::Result<ReviewDiffData> {
    ensure_repository(cwd)?;
    let range = resolve_diff_range(cwd, source)?;
    // Ask Git for both views in one pass. Separate invocations recompute the
    // same diff, which is especially costly for large files and on Windows.
    let mut numstat = diff_output(
        cwd,
        &range,
        &["--numstat", "--patch", "--unified=2147483647"],
    )?;
    let hydrated = if let Some(boundary) = numstat.find("\ndiff --git ") {
        let patch = numstat.split_off(boundary + 1);
        // Drop the separator, retaining numstat's own final newline.
        numstat.truncate(boundary);
        patch
    } else {
        String::new()
    };
    let (patch, complete_context) = if hydrated.len() <= MAX_HYDRATED_PATCH_BYTES {
        (hydrated, true)
    } else {
        (diff_output(cwd, &range, &["--unified=3"])?, false)
    };
    Ok(ReviewDiffData {
        source,
        numstat,
        patch,
        complete_context,
    })
}

fn resolve_diff_range(cwd: &Path, source: ReviewDiffSource) -> anyhow::Result<DiffRange> {
    let head = resolve(cwd, "HEAD").unwrap_or_else(|| EMPTY_TREE.to_owned());
    Ok(match source {
        ReviewDiffSource::LastTurn {
            session_id,
            turn_count,
            ..
        } => {
            if turn_count == 0 {
                bail!("the first checkpoint is a baseline, not a completed turn");
            }
            let diff_base_ref = crate::checkpoint::turn_diff_base_ref(session_id, turn_count);
            let start_ref = crate::checkpoint::turn_start_ref(session_id, turn_count);
            let legacy_ref = crate::checkpoint::checkpoint_ref(session_id, turn_count - 1);
            let to_ref = crate::checkpoint::checkpoint_ref(session_id, turn_count);
            DiffRange {
                from: resolve(cwd, &diff_base_ref)
                    .or_else(|| resolve(cwd, &start_ref))
                    .or_else(|| resolve(cwd, &legacy_ref))
                    .ok_or_else(|| anyhow!("the turn's starting checkpoint is unavailable"))?,
                to: resolve(cwd, &to_ref)
                    .ok_or_else(|| anyhow!("the turn's ending checkpoint is unavailable"))?,
            }
        }
        ReviewDiffSource::Uncommitted => DiffRange {
            from: head,
            to: crate::checkpoint::capture_worktree_commit(cwd)?,
        },
        ReviewDiffSource::Unstaged => DiffRange {
            from: index_tree(cwd)?,
            to: crate::checkpoint::capture_worktree_commit(cwd)?,
        },
        ReviewDiffSource::Staged => DiffRange {
            from: head,
            to: index_tree(cwd)?,
        },
        ReviewDiffSource::Committed => DiffRange {
            from: branch_base(cwd)?,
            to: head,
        },
        ReviewDiffSource::Branch => DiffRange {
            from: branch_base(cwd)?,
            to: crate::checkpoint::capture_worktree_commit(cwd)?,
        },
    })
}

fn branch_base(cwd: &Path) -> anyhow::Result<String> {
    let Some(snapshot) = crate::git_branch::inspect(cwd)? else {
        bail!("the workspace is not a Git repository");
    };
    let Some(head) = resolve(cwd, "HEAD") else {
        return Ok(EMPTY_TREE.to_owned());
    };
    let current = snapshot.current.as_deref();
    let default_branch = snapshot
        .default_branch
        .filter(|branch| current != Some(branch.as_str()))
        .or_else(|| {
            ["main", "master"]
                .into_iter()
                .find(|candidate| {
                    current != Some(*candidate)
                        && snapshot
                            .branches
                            .iter()
                            .any(|branch| branch.name == *candidate)
                })
                .map(str::to_owned)
        });
    let Some(default_branch) = default_branch else {
        return Ok(head);
    };
    let output = git(cwd, ["merge-base", "HEAD", default_branch.as_str()])?;
    let base = output.trim();
    Ok(if base.is_empty() {
        head
    } else {
        base.to_owned()
    })
}

fn index_tree(cwd: &Path) -> anyhow::Result<String> {
    let output = crate::command_env::plain_command("git")
        .args(["write-tree"])
        .current_dir(cwd)
        .output()
        .context("failed to snapshot the Git index")?;
    if output.status.success() {
        let tree = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !tree.is_empty() {
            return Ok(tree);
        }
    }
    if resolve(cwd, "HEAD").is_none() {
        Ok(EMPTY_TREE.to_owned())
    } else {
        bail!("{}", command_error(&output))
    }
}

fn diff_output(cwd: &Path, range: &DiffRange, modes: &[&str]) -> anyhow::Result<String> {
    let output = crate::command_env::plain_command("git")
        .args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-ext-diff",
            "--no-color",
        ])
        .args(modes)
        .arg("--no-renames")
        .arg(&range.from)
        .arg(&range.to)
        .args(["--", "."])
        .current_dir(cwd)
        .output()
        .context("failed to generate Git diff")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        bail!("{}", command_error(&output))
    }
}

fn ensure_repository(cwd: &Path) -> anyhow::Result<()> {
    let output = crate::command_env::plain_command("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .context("failed to inspect Git workspace")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("the workspace is not a Git repository")
    }
}

fn resolve(cwd: &Path, revision: &str) -> Option<String> {
    let output = crate::command_env::plain_command("git")
        .args(["rev-parse", "--verify", &format!("{revision}^{{commit}}")])
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn git<I, S>(cwd: &Path, args: I) -> anyhow::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = crate::command_env::plain_command("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("failed to execute git")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        bail!("{}", command_error(&output))
    }
}

fn command_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        stderr
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use uuid::Uuid;

    fn git_ok(cwd: &Path, args: &[&str]) {
        let output = crate::command_env::plain_command("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", command_error(&output));
    }

    fn repository() -> PathBuf {
        let root = std::env::temp_dir().join(format!("waku-workspace-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        git_ok(&root, &["init", "-b", "main"]);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn baseline() {}\n").unwrap();
        git_ok(&root, &["add", "."]);
        git_ok(
            &root,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "baseline",
            ],
        );
        root
    }

    fn collect(root: &Path, source: ReviewDiffSource) -> ReviewDiffData {
        let WorkspaceResult::ReviewDiff { data } = execute(WorkspaceOperation::CollectReviewDiff {
            cwd: root.to_path_buf(),
            source,
        })
        .unwrap() else {
            panic!("unexpected workspace response")
        };
        data
    }

    fn numstat_summary(numstat: &str) -> (usize, u64, u64) {
        let mut files = 0;
        let mut additions = 0;
        let mut deletions = 0;
        for line in numstat.lines().filter(|line| !line.is_empty()) {
            let mut fields = line.splitn(3, '\t');
            additions += fields
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            deletions += fields
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            assert!(fields.next().is_some(), "numstat row is missing its path");
            files += 1;
        }
        (files, additions, deletions)
    }

    #[test]
    fn directory_browser_lists_an_arbitrary_daemon_directory() {
        let directory =
            std::env::temp_dir().join(format!("waku-directory-browser-{}", Uuid::new_v4()));
        fs::create_dir_all(directory.join("folder")).unwrap();
        fs::create_dir_all(directory.join(".git")).unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();

        let WorkspaceResult::Directory {
            path,
            parent,
            entries,
            ..
        } = execute(WorkspaceOperation::BrowseDirectory {
            path: Some(directory.clone()),
        })
        .unwrap()
        else {
            panic!("unexpected workspace response")
        };

        assert_eq!(
            path,
            strip_windows_verbatim(fs::canonicalize(&directory).unwrap())
        );
        assert_eq!(parent, path.parent().map(Path::to_owned));
        assert_eq!(
            entries
                .iter()
                .map(|entry| (&*entry.name, entry.is_dir))
                .collect::<Vec<_>>(),
            [(".git", true), ("folder", true), ("notes.txt", false)]
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn file_reader_keeps_the_complete_disk_content() {
        let root = std::env::temp_dir().join(format!("waku-editor-file-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let content = "line\n".repeat(10_000);
        fs::write(root.join("large.txt"), &content).unwrap();

        let WorkspaceResult::TextFile { content: restored } =
            execute(WorkspaceOperation::ReadTextFile {
                root: root.clone(),
                relative_path: PathBuf::from("large.txt"),
            })
            .unwrap()
        else {
            panic!("unexpected workspace response")
        };
        assert_eq!(restored, content);
        assert!(
            execute(WorkspaceOperation::ReadTextFile {
                root: root.clone(),
                relative_path: PathBuf::from("missing.txt"),
            })
            .is_err()
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_chunks_round_trip_arbitrary_bytes() {
        let root = std::env::temp_dir().join(format!("waku-binary-read-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(100_000).collect();
        fs::write(root.join("blob.bin"), &bytes).unwrap();

        // Walking the file in chunks of an odd size reassembles it exactly,
        // with the final short chunk and the total length reported throughout.
        let mut offset = 0;
        let mut restored = Vec::new();
        loop {
            let WorkspaceResult::FileChunk {
                bytes: encoded,
                total_bytes,
            } = execute(WorkspaceOperation::ReadFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("blob.bin"),
                offset,
                length: 1021,
            })
            .unwrap()
            else {
                panic!("unexpected workspace response")
            };
            assert_eq!(total_bytes, bytes.len() as u64);
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap();
            if chunk.is_empty() {
                break;
            }
            assert!(chunk.len() <= 1021);
            restored.extend_from_slice(&chunk);
            offset += chunk.len() as u64;
        }
        assert_eq!(restored, bytes);

        // Reads past the end answer with an empty chunk, not an error, and an
        // offset inside the file returns only the remaining bytes.
        let WorkspaceResult::FileChunk { bytes: encoded, .. } =
            execute(WorkspaceOperation::ReadFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("blob.bin"),
                offset: bytes.len() as u64,
                length: 1021,
            })
            .unwrap()
        else {
            panic!("unexpected workspace response")
        };
        assert!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap()
                .is_empty()
        );
        let WorkspaceResult::FileChunk {
            bytes: encoded,
            total_bytes: _,
        } = execute(WorkspaceOperation::ReadFileChunk {
            root: root.clone(),
            relative_path: PathBuf::from("blob.bin"),
            offset: bytes.len() as u64 - 10,
            length: 1021,
        })
        .unwrap()
        else {
            panic!("unexpected workspace response")
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            &bytes[bytes.len() - 10..]
        );
        assert!(
            execute(WorkspaceOperation::ReadFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("missing.bin"),
                offset: 0,
                length: 1021,
            })
            .is_err()
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_chunk_requests_are_capped_per_message() {
        let root = std::env::temp_dir().join(format!("waku-chunk-cap-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("big.bin"), [0u8; 128]).unwrap();

        // However much a caller asks for, one response stays small enough to
        // fit the wire budget after base64.
        let WorkspaceResult::FileChunk { bytes: encoded, .. } =
            execute(WorkspaceOperation::ReadFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("big.bin"),
                offset: 0,
                length: u64::MAX,
            })
            .unwrap()
        else {
            panic!("unexpected workspace response")
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            [0u8; 128]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_mutations_stay_inside_the_workspace() {
        let root = std::env::temp_dir().join(format!("waku-path-mutations-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn main() {}\n").unwrap();

        execute(WorkspaceOperation::RenamePath {
            root: root.clone(),
            from_relative_path: PathBuf::from("src/lib.rs"),
            to_relative_path: PathBuf::from("src/main.rs"),
        })
        .unwrap();
        assert!(root.join("src/main.rs").is_file());
        assert!(!root.join("src/lib.rs").exists());

        execute(WorkspaceOperation::DeletePath {
            root: root.clone(),
            relative_path: PathBuf::from("src/main.rs"),
        })
        .unwrap();
        assert!(!root.join("src/main.rs").exists());

        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join("src/nested/deep.txt"), "deep").unwrap();
        execute(WorkspaceOperation::DeletePath {
            root: root.clone(),
            relative_path: PathBuf::from("src/nested"),
        })
        .unwrap();
        assert!(!root.join("src/nested").exists());

        // Renaming onto an existing entry is refused instead of silently
        // replacing it, and traversal outside the root never reaches the FS.
        fs::write(root.join("occupied.txt"), "occupied").unwrap();
        fs::write(root.join("other.txt"), "other").unwrap();
        assert!(
            execute(WorkspaceOperation::RenamePath {
                root: root.clone(),
                from_relative_path: PathBuf::from("other.txt"),
                to_relative_path: PathBuf::from("occupied.txt"),
            })
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(root.join("occupied.txt")).unwrap(),
            "occupied"
        );
        for operation in [
            WorkspaceOperation::RenamePath {
                root: root.clone(),
                from_relative_path: PathBuf::from("other.txt"),
                to_relative_path: PathBuf::from("../escape.txt"),
            },
            WorkspaceOperation::DeletePath {
                root: root.clone(),
                relative_path: PathBuf::from("../escape.txt"),
            },
            WorkspaceOperation::DeletePath {
                root: root.clone(),
                relative_path: PathBuf::from(""),
            },
        ] {
            assert!(execute(operation).is_err());
        }
        assert!(root.join("other.txt").is_file());
        assert!(!root.parent().unwrap().join("escape.txt").exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn combined_review_output_matches_separate_git_views() {
        let root = repository();
        let clean = collect(&root, ReviewDiffSource::Uncommitted);
        assert!(clean.numstat.is_empty());
        assert!(clean.patch.is_empty());

        fs::write(root.join("src/lib.rs"), "fn changed() {}\n").unwrap();
        fs::write(root.join("binary.dat"), b"\0\x01\x02").unwrap();
        fs::write(root.join("space name.txt"), "diff --git is file content\n").unwrap();
        fs::write(root.join("empty.txt"), "").unwrap();
        let range = resolve_diff_range(&root, ReviewDiffSource::Uncommitted).unwrap();
        let expected_numstat = diff_output(&root, &range, &["--numstat"]).unwrap();
        let expected_patch = diff_output(&root, &range, &["--unified=2147483647"]).unwrap();
        let combined = collect(&root, ReviewDiffSource::Uncommitted);
        assert_eq!(combined.numstat, expected_numstat);
        assert_eq!(combined.patch, expected_patch);
        assert!(combined.complete_context);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_modes_compare_consistent_git_snapshots() {
        let root = repository();
        git_ok(&root, &["switch", "-c", "feature"]);
        fs::write(root.join("src/lib.rs"), "fn committed() {}\n").unwrap();
        git_ok(&root, &["add", "src/lib.rs"]);
        git_ok(
            &root,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "feature",
            ],
        );
        fs::write(
            root.join("src/lib.rs"),
            "fn committed() {}\nfn staged() {}\n",
        )
        .unwrap();
        git_ok(&root, &["add", "src/lib.rs"]);
        fs::write(
            root.join("src/lib.rs"),
            "fn committed() {}\nfn staged() {}\nfn unstaged() {}\n",
        )
        .unwrap();
        fs::write(root.join("new file.txt"), "untracked\n").unwrap();

        let committed = collect(&root, ReviewDiffSource::Committed);
        let staged = collect(&root, ReviewDiffSource::Staged);
        let unstaged = collect(&root, ReviewDiffSource::Unstaged);
        let uncommitted = collect(&root, ReviewDiffSource::Uncommitted);
        let branch = collect(&root, ReviewDiffSource::Branch);

        assert_eq!(numstat_summary(&committed.numstat), (1, 1, 1));
        assert_eq!(numstat_summary(&staged.numstat), (1, 1, 0));
        assert_eq!(
            numstat_summary(&unstaged.numstat),
            (2, 2, 0),
            "unstaged includes untracked files"
        );
        assert_eq!(numstat_summary(&uncommitted.numstat), (2, 3, 0));
        assert_eq!(numstat_summary(&branch.numstat), (2, 4, 1));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn last_turn_uses_captured_checkpoints_not_the_live_worktree() {
        let root = repository();
        let session_id = Uuid::new_v4();
        crate::checkpoint::capture_turn(&root, session_id, 0).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn baseline() {}\nfn from_turn() {}\n",
        )
        .unwrap();
        crate::checkpoint::capture_turn(&root, session_id, 1).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn baseline() {}\nfn from_turn() {}\nfn after_turn() {}\n",
        )
        .unwrap();

        let data = collect(
            &root,
            ReviewDiffSource::LastTurn {
                session_id,
                turn_id: Uuid::new_v4(),
                turn_count: 1,
            },
        );
        assert_eq!(numstat_summary(&data.numstat), (1, 1, 0));
        assert!(data.patch.contains("from_turn"));
        assert!(!data.patch.contains("after_turn"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn last_turn_review_uses_the_branch_aware_diff_base() {
        let root = repository();
        git_ok(&root, &["switch", "-c", "feature"]);
        fs::write(root.join("feature-only.rs"), "fn feature() {}\n").unwrap();
        git_ok(&root, &["add", "feature-only.rs"]);
        git_ok(
            &root,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "feature baseline",
            ],
        );
        git_ok(&root, &["switch", "main"]);
        fs::write(root.join("main-only.rs"), "fn main_only() {}\n").unwrap();
        git_ok(&root, &["add", "main-only.rs"]);
        git_ok(
            &root,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "main baseline",
            ],
        );

        let session_id = Uuid::new_v4();
        crate::checkpoint::capture_turn_start(&root, session_id, 1).unwrap();
        git_ok(&root, &["switch", "feature"]);
        fs::write(
            root.join("src/lib.rs"),
            "fn baseline() {}\nfn from_turn() {}\n",
        )
        .unwrap();
        crate::checkpoint::capture_turn(&root, session_id, 1).unwrap();

        let data = collect(
            &root,
            ReviewDiffSource::LastTurn {
                session_id,
                turn_id: Uuid::new_v4(),
                turn_count: 1,
            },
        );
        assert_eq!(numstat_summary(&data.numstat), (1, 1, 0));
        assert!(data.numstat.ends_with("\tsrc/lib.rs\n"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn strip_windows_verbatim_removes_drive_and_unc_prefixes() {
        assert_eq!(
            strip_windows_verbatim(PathBuf::from(r"\\?\C:\Users\Brainco\repo")),
            PathBuf::from(r"C:\Users\Brainco\repo")
        );
        assert_eq!(
            strip_windows_verbatim(PathBuf::from(r"\\?\UNC\server\share\repo")),
            PathBuf::from(r"\\server\share\repo")
        );
        assert_eq!(
            strip_windows_verbatim(PathBuf::from("/home/user/repo")),
            PathBuf::from("/home/user/repo")
        );
    }

    #[cfg(windows)]
    #[test]
    fn browse_directory_supports_windows_drives_and_bare_drive_letters() {
        let WorkspaceResult::Directory {
            path,
            parent,
            entries,
            filesystem_root,
            ..
        } = execute(WorkspaceOperation::BrowseDirectory {
            path: Some(PathBuf::new()),
        })
        .unwrap()
        else {
            panic!("expected directory result");
        };
        assert!(path.as_os_str().is_empty());
        assert!(parent.is_none());
        assert!(filesystem_root.as_os_str().is_empty());
        assert!(
            entries
                .iter()
                .any(|entry| entry.absolute_path == Path::new(r"C:\")),
            "expected C:\\ in logical drives: {entries:?}"
        );

        let WorkspaceResult::Directory { path, parent, .. } =
            execute(WorkspaceOperation::BrowseDirectory {
                path: Some(PathBuf::from("C:")),
            })
            .unwrap()
        else {
            panic!("expected directory result");
        };
        assert_eq!(path, Path::new(r"C:\"));
        assert_eq!(parent, Some(PathBuf::new()));
    }

    fn try_symlink(original: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            crate::fs_ext::symlink(original, link).unwrap();
            true
        }
        #[cfg(not(unix))]
        {
            crate::fs_ext::symlink(original, link).is_ok()
        }
    }

    fn encode_chunk(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn write_file_chunk_round_trips_multi_chunk_binary_and_empty_files() {
        let root = std::env::temp_dir().join(format!("waku-upload-chunks-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();

        let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(50_000).collect();
        let relative = PathBuf::from("nested/sub/blob.bin");
        let mut offset = 0usize;
        let chunk_size = 7_919usize;
        while offset < bytes.len() {
            let end = (offset + chunk_size).min(bytes.len());
            execute(WorkspaceOperation::WriteFileChunk {
                root: root.clone(),
                relative_path: relative.clone(),
                offset: offset as u64,
                bytes: encode_chunk(&bytes[offset..end]),
                overwrite: false,
            })
            .unwrap();
            offset = end;
        }

        let mut restored = Vec::new();
        let mut read_offset = 0u64;
        loop {
            let WorkspaceResult::FileChunk {
                bytes: encoded,
                total_bytes,
            } = execute(WorkspaceOperation::ReadFileChunk {
                root: root.clone(),
                relative_path: relative.clone(),
                offset: read_offset,
                length: 4_096,
            })
            .unwrap()
            else {
                panic!("unexpected workspace response");
            };
            assert_eq!(total_bytes, bytes.len() as u64);
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap();
            if chunk.is_empty() {
                break;
            }
            restored.extend_from_slice(&chunk);
            read_offset += chunk.len() as u64;
        }
        assert_eq!(restored, bytes);

        execute(WorkspaceOperation::WriteFileChunk {
            root: root.clone(),
            relative_path: PathBuf::from("nested/empty.txt"),
            offset: 0,
            bytes: String::new(),
            overwrite: false,
        })
        .unwrap();
        assert_eq!(fs::read(root.join("nested/empty.txt")).unwrap(), b"");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn write_file_chunk_rejects_oversized_payload_without_mutating_existing_file() {
        let root = std::env::temp_dir().join(format!("waku-upload-cap-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("existing.bin");
        fs::write(&target, b"keep-existing").unwrap();

        let oversized = encode_chunk(&vec![0xAB; MAX_FILE_CHUNK_BYTES + 1]);
        let error = execute(WorkspaceOperation::WriteFileChunk {
            root: root.clone(),
            relative_path: PathBuf::from("existing.bin"),
            offset: 0,
            bytes: oversized.clone(),
            overwrite: true,
        })
        .unwrap_err();
        assert!(error.to_string().contains("exceeds maximum"));
        assert_eq!(fs::read(&target).unwrap(), b"keep-existing");

        let offset_error = execute(WorkspaceOperation::WriteFileChunk {
            root: root.clone(),
            relative_path: PathBuf::from("existing.bin"),
            offset: 13,
            bytes: oversized,
            overwrite: false,
        })
        .unwrap_err();
        assert!(offset_error.to_string().contains("exceeds maximum"));
        assert_eq!(fs::read(&target).unwrap(), b"keep-existing");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn write_file_chunk_enforces_overwrite_and_offset_rules() {
        let root = std::env::temp_dir().join(format!("waku-upload-overwrite-{}", Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("waku-upload-outside-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();

        fs::write(root.join("file.txt"), b"longer-previous-content").unwrap();
        assert!(
            execute(WorkspaceOperation::WriteFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("file.txt"),
                offset: 0,
                bytes: encode_chunk(b"new"),
                overwrite: false,
            })
            .is_err()
        );
        assert_eq!(
            fs::read(root.join("file.txt")).unwrap(),
            b"longer-previous-content"
        );

        execute(WorkspaceOperation::WriteFileChunk {
            root: root.clone(),
            relative_path: PathBuf::from("file.txt"),
            offset: 0,
            bytes: encode_chunk(b"new"),
            overwrite: true,
        })
        .unwrap();
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"new");

        fs::create_dir_all(root.join("dir_target/inner")).unwrap();
        fs::write(root.join("dir_target/inner/a.txt"), "data").unwrap();
        assert!(
            execute(WorkspaceOperation::WriteFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("dir_target"),
                offset: 0,
                bytes: encode_chunk(b"replaced"),
                overwrite: false,
            })
            .is_err()
        );
        assert!(root.join("dir_target/inner/a.txt").is_file());

        execute(WorkspaceOperation::WriteFileChunk {
            root: root.clone(),
            relative_path: PathBuf::from("dir_target"),
            offset: 0,
            bytes: encode_chunk(b"replaced"),
            overwrite: true,
        })
        .unwrap();
        assert_eq!(fs::read(root.join("dir_target")).unwrap(), b"replaced");

        // Later offsets must target an existing regular file at exact length.
        assert!(
            execute(WorkspaceOperation::WriteFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("missing.bin"),
                offset: 5,
                bytes: encode_chunk(b"tail"),
                overwrite: false,
            })
            .is_err()
        );
        assert!(
            execute(WorkspaceOperation::WriteFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("file.txt"),
                offset: 2,
                bytes: encode_chunk(b"tail"),
                overwrite: false,
            })
            .is_err()
        );
        assert!(
            execute(WorkspaceOperation::WriteFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("file.txt"),
                offset: 10,
                bytes: encode_chunk(b"tail"),
                overwrite: false,
            })
            .is_err()
        );
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"new");

        let outside_file = outside.join("secret.txt");
        fs::write(&outside_file, b"untouched").unwrap();
        let file_link = root.join("file_link.txt");
        if try_symlink(&outside_file, &file_link) {
            assert!(
                execute(WorkspaceOperation::WriteFileChunk {
                    root: root.clone(),
                    relative_path: PathBuf::from("file_link.txt"),
                    offset: 0,
                    bytes: encode_chunk(b"local"),
                    overwrite: false,
                })
                .is_err()
            );
            assert!(
                execute(WorkspaceOperation::WriteFileChunk {
                    root: root.clone(),
                    relative_path: PathBuf::from("file_link.txt"),
                    offset: 9,
                    bytes: encode_chunk(b"append"),
                    overwrite: true,
                })
                .is_err()
            );
            execute(WorkspaceOperation::WriteFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("file_link.txt"),
                offset: 0,
                bytes: encode_chunk(b"local"),
                overwrite: true,
            })
            .unwrap();
            assert!(
                !fs::symlink_metadata(&file_link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read(&file_link).unwrap(), b"local");
            assert_eq!(fs::read(&outside_file).unwrap(), b"untouched");
        }

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn ensure_directory_is_idempotent_and_handles_conflicts() {
        let root = std::env::temp_dir().join(format!("waku-ensure-dir-{}", Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("waku-ensure-outside-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();

        execute(WorkspaceOperation::EnsureDirectory {
            root: root.clone(),
            relative_path: PathBuf::from("a/b/c"),
            overwrite: false,
        })
        .unwrap();
        assert!(root.join("a/b/c").is_dir());

        fs::write(root.join("a/b/c/keep.txt"), "keep").unwrap();
        execute(WorkspaceOperation::EnsureDirectory {
            root: root.clone(),
            relative_path: PathBuf::from("a/b/c"),
            overwrite: false,
        })
        .unwrap();
        execute(WorkspaceOperation::EnsureDirectory {
            root: root.clone(),
            relative_path: PathBuf::from("a/b/c"),
            overwrite: true,
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("a/b/c/keep.txt")).unwrap(),
            "keep"
        );

        fs::write(root.join("occupied"), "file-bytes").unwrap();
        assert!(
            execute(WorkspaceOperation::EnsureDirectory {
                root: root.clone(),
                relative_path: PathBuf::from("occupied"),
                overwrite: false,
            })
            .is_err()
        );
        assert!(root.join("occupied").is_file());

        execute(WorkspaceOperation::EnsureDirectory {
            root: root.clone(),
            relative_path: PathBuf::from("occupied"),
            overwrite: true,
        })
        .unwrap();
        assert!(root.join("occupied").is_dir());

        fs::write(outside.join("keep.txt"), "outside").unwrap();
        let dir_link = root.join("dir_link");
        if try_symlink(&outside, &dir_link) {
            assert!(
                execute(WorkspaceOperation::EnsureDirectory {
                    root: root.clone(),
                    relative_path: PathBuf::from("dir_link"),
                    overwrite: false,
                })
                .is_err()
            );
            execute(WorkspaceOperation::EnsureDirectory {
                root: root.clone(),
                relative_path: PathBuf::from("dir_link"),
                overwrite: true,
            })
            .unwrap();
            let meta = fs::symlink_metadata(&dir_link).unwrap();
            assert!(meta.file_type().is_dir());
            assert!(!meta.file_type().is_symlink());
            assert_eq!(
                fs::read_to_string(outside.join("keep.txt")).unwrap(),
                "outside"
            );
        }

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn upload_operations_check_existence_and_block_escapes_and_symlinks() {
        let root = std::env::temp_dir().join(format!("waku-upload-guards-{}", Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("waku-upload-escape-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("existing_dir")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("existing.txt"), "ok").unwrap();
        fs::write(outside.join("outside.txt"), "secret").unwrap();

        let broken_link = root.join("broken_link");
        let has_broken_link = try_symlink(&outside.join("does_not_exist"), &broken_link);

        let mut query = vec![
            PathBuf::from("existing.txt"),
            PathBuf::from("missing.txt"),
            PathBuf::from("existing_dir"),
        ];
        let mut expected = vec![PathBuf::from("existing.txt"), PathBuf::from("existing_dir")];
        if has_broken_link {
            query.push(PathBuf::from("broken_link"));
            expected.push(PathBuf::from("broken_link"));
        }

        let WorkspaceResult::ExistingPaths { relative_paths } =
            execute(WorkspaceOperation::CheckPathsExist {
                root: root.clone(),
                relative_paths: query,
            })
            .unwrap()
        else {
            panic!("unexpected workspace response");
        };
        assert_eq!(relative_paths, expected);

        let absolute_path = if cfg!(windows) {
            PathBuf::from(r"C:\outside\evil.txt")
        } else {
            PathBuf::from("/outside/evil.txt")
        };
        for invalid in [
            PathBuf::from("../outside"),
            PathBuf::from("a/../../outside"),
            PathBuf::new(),
            absolute_path,
        ] {
            assert!(
                execute(WorkspaceOperation::CheckPathsExist {
                    root: root.clone(),
                    relative_paths: vec![invalid.clone()],
                })
                .is_err()
            );
            assert!(
                execute(WorkspaceOperation::EnsureDirectory {
                    root: root.clone(),
                    relative_path: invalid.clone(),
                    overwrite: true,
                })
                .is_err()
            );
            assert!(
                execute(WorkspaceOperation::WriteFileChunk {
                    root: root.clone(),
                    relative_path: invalid,
                    offset: 0,
                    bytes: encode_chunk(b"x"),
                    overwrite: true,
                })
                .is_err()
            );
        }

        let linked_dir = root.join("linked_dir");
        if try_symlink(&outside, &linked_dir) {
            let dir_err = execute(WorkspaceOperation::EnsureDirectory {
                root: root.clone(),
                relative_path: PathBuf::from("linked_dir/sub"),
                overwrite: true,
            })
            .unwrap_err();
            assert!(dir_err.to_string().contains("symlink"));
            assert!(!outside.join("sub").exists());

            let write_err = execute(WorkspaceOperation::WriteFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("linked_dir/nested/pwn.txt"),
                offset: 0,
                bytes: encode_chunk(b"pwn"),
                overwrite: true,
            })
            .unwrap_err();
            assert!(write_err.to_string().contains("symlink"));
            assert!(!outside.join("nested").exists());

            let append_err = execute(WorkspaceOperation::WriteFileChunk {
                root: root.clone(),
                relative_path: PathBuf::from("linked_dir/outside.txt"),
                offset: 6,
                bytes: encode_chunk(b"!"),
                overwrite: true,
            })
            .unwrap_err();
            assert!(append_err.to_string().contains("symlink"));
            assert_eq!(fs::read(outside.join("outside.txt")).unwrap(), b"secret");
        }

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn resolve_path_handles_directories_files_tilde_and_missing_paths() {
        let root = std::env::temp_dir().join(format!("waku-resolve-path-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("nested")).unwrap();
        let file = root.join("nested").join("readme.md");
        fs::write(&file, "hello").unwrap();

        let expected_nested =
            strip_windows_verbatim(fs::canonicalize(root.join("nested")).unwrap())
                .to_string_lossy()
                .into_owned();

        let WorkspaceResult::ResolvedPath { path: dir_resolved } =
            execute(WorkspaceOperation::ResolvePath {
                path: root.join("nested").to_string_lossy().into_owned(),
            })
            .unwrap()
        else {
            panic!("expected ResolvedPath");
        };
        assert_eq!(dir_resolved, expected_nested);
        assert!(!dir_resolved.starts_with(r"\\?\"));

        let WorkspaceResult::ResolvedPath {
            path: file_resolved,
        } = execute(WorkspaceOperation::ResolvePath {
            path: file.to_string_lossy().into_owned(),
        })
        .unwrap()
        else {
            panic!("expected ResolvedPath");
        };
        assert_eq!(file_resolved, expected_nested);

        if let Some(home) = dirs::home_dir() {
            let expected_home = strip_windows_verbatim(fs::canonicalize(&home).unwrap())
                .to_string_lossy()
                .into_owned();
            let WorkspaceResult::ResolvedPath {
                path: tilde_resolved,
            } = execute(WorkspaceOperation::ResolvePath { path: "~".into() }).unwrap()
            else {
                panic!("expected ResolvedPath");
            };
            assert_eq!(tilde_resolved, expected_home);
        }

        let missing = root.join("does-not-exist");
        let missing_err = execute(WorkspaceOperation::ResolvePath {
            path: missing.to_string_lossy().into_owned(),
        })
        .unwrap_err();
        assert!(
            missing_err.to_string().contains("does not exist"),
            "unexpected error: {missing_err}"
        );
        assert!(execute(WorkspaceOperation::ResolvePath { path: "   ".into() }).is_err());

        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(windows)]
fn list_logical_drives() -> Vec<WorkingTreeEntry> {
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26_u8)
        .filter(|bit| mask & (1_u32 << bit) != 0)
        .map(|bit| {
            let drive = format!("{}:\\", (b'A' + bit) as char);
            WorkingTreeEntry {
                relative_path: drive.clone(),
                absolute_path: PathBuf::from(&drive),
                name: drive,
                is_dir: true,
                expanded: false,
                depth: 0,
            }
        })
        .collect()
}

fn strip_windows_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

/// Resolve a user-supplied workspace path on this daemon host:
/// - expands a leading `~` or `~/` (`~\` on Windows) to the host's home dir;
/// - checks that the path exists;
/// - if it names a regular file, resolves to its parent directory;
/// - canonicalizes and strips any Windows `\\?\` verbatim prefix.
pub fn resolve_workspace_directory(raw: &str) -> anyhow::Result<PathBuf> {
    if raw.trim().is_empty() {
        bail!("workspace path must not be empty");
    }
    let expanded = expand_leading_tilde(raw)?;
    let metadata = fs::metadata(&expanded).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow!("workspace path does not exist: {}", expanded.display())
        } else {
            anyhow!(
                "could not access workspace path {}: {error}",
                expanded.display()
            )
        }
    })?;
    let target = if metadata.is_dir() {
        expanded
    } else if metadata.is_file() {
        expanded
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        bail!(
            "workspace path is not a directory or file: {}",
            expanded.display()
        );
    };
    let canonical = fs::canonicalize(&target)
        .with_context(|| format!("could not resolve workspace path: {}", target.display()))?;
    Ok(strip_windows_verbatim(canonical))
}

fn expand_leading_tilde(raw: &str) -> anyhow::Result<PathBuf> {
    if raw == "~" {
        return dirs::home_dir().ok_or_else(|| anyhow!("home directory is unavailable"));
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory is unavailable"))?;
        return Ok(home.join(rest));
    }
    #[cfg(windows)]
    {
        let bytes = raw.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return Ok(PathBuf::from(format!("{raw}\\")));
        }
    }
    Ok(PathBuf::from(raw))
}

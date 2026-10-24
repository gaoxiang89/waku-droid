//! Git Graph diffs and context-menu actions. Every entry point runs `git`
//! and must be called off the UI thread.
//!
//! Arguments arrive from the client, so each one is validated before it
//! reaches `git`: commits must be full hex object ids, ref names must pass
//! `check-ref-format`, and nothing may start with `-` where `git` would read
//! it as an option.

use std::path::Path;
use std::process::Output;

use anyhow::{Context as _, anyhow, bail};

use waku_protocol::git::{BranchSnapshot, GitGraphFile};
pub use waku_protocol::git::{GitDiffSpec, GitFileDiff, GitGraphAction, GitResetMode};

use crate::git_graph::{command_error, git, git_stdout, is_commit_hash, parse_name_status_z};

/// Lines kept from one file's patch. The inline diff is a preview; a longer
/// patch belongs in the review panel.
const MAX_PATCH_LINES: usize = 4_000;
const MAX_PATCH_BYTES: usize = 512 * 1024;
/// Characters of combined `git` output shown when an action fails.
const MAX_ERROR_CHARS: usize = 1_200;

pub fn compare_files(cwd: &Path, from: &str, to: &str) -> anyhow::Result<Vec<GitGraphFile>> {
    ensure_hash(from)?;
    ensure_hash(to)?;
    let raw = git_stdout(
        cwd,
        &[
            "diff",
            "--name-status",
            "-z",
            "-M",
            "--no-ext-diff",
            from,
            to,
        ],
    )?;
    Ok(parse_name_status_z(&raw))
}

pub fn file_diff(
    cwd: &Path,
    spec: &GitDiffSpec,
    path: &str,
    old_path: Option<&str>,
) -> anyhow::Result<GitFileDiff> {
    if path.is_empty() {
        bail!("missing file path");
    }
    let mut args: Vec<&str> = Vec::new();
    match spec {
        GitDiffSpec::Uncommitted => {
            if is_untracked(cwd, path)? {
                // `--no-index` exits 1 when the sides differ, which they
                // always do for a new file.
                let output = git(
                    cwd,
                    &[
                        "diff",
                        "--no-color",
                        "--no-ext-diff",
                        "--no-index",
                        "--",
                        "/dev/null",
                        path,
                    ],
                )?;
                if !matches!(output.status.code(), Some(0 | 1)) {
                    bail!("{}", command_error(&output));
                }
                return Ok(clip_patch(&String::from_utf8_lossy(&output.stdout)));
            }
            args.extend(["diff", "--no-color", "--no-ext-diff", "-M"]);
            if has_head(cwd) {
                args.push("HEAD");
            } else {
                args.push("--cached");
            }
        }
        GitDiffSpec::Commit { hash } => {
            ensure_hash(hash)?;
            args.extend([
                "diff-tree",
                "-p",
                "-M",
                "--root",
                "--no-commit-id",
                "-m",
                "--first-parent",
                "--no-color",
                "--no-ext-diff",
                hash.as_str(),
            ]);
        }
        GitDiffSpec::Compare { from, to } => {
            ensure_hash(from)?;
            ensure_hash(to)?;
            args.extend([
                "diff",
                "--no-color",
                "--no-ext-diff",
                "-M",
                from.as_str(),
                to.as_str(),
            ]);
        }
    }
    args.push("--");
    // Rename detection only pairs the two sides when both are in the pathspec.
    if let Some(old_path) = old_path.filter(|old| !old.is_empty() && *old != path) {
        args.push(old_path);
    }
    args.push(path);
    Ok(clip_patch(&git_stdout(cwd, &args)?))
}

pub fn run_action(cwd: &Path, action: &GitGraphAction) -> anyhow::Result<Option<BranchSnapshot>> {
    match action {
        GitGraphAction::CheckoutCommit { hash } => {
            ensure_hash(hash)?;
            run(cwd, &["switch", "--detach", hash])?;
        }
        GitGraphAction::CheckoutBranch { name } => {
            ensure_branch_name(cwd, name)?;
            run(cwd, &["switch", "--", name])?;
        }
        GitGraphAction::CheckoutRemoteBranch { name } => {
            let (remote, branch) = split_remote_branch(cwd, name)?;
            ensure_branch_name(cwd, &branch)?;
            if has_ref(cwd, &format!("refs/heads/{branch}")) {
                run(cwd, &["switch", "--", &branch])?;
            } else {
                let upstream = format!("{remote}/{branch}");
                run(cwd, &["switch", "--track", "-c", &branch, &upstream])?;
            }
        }
        GitGraphAction::CreateBranch {
            name,
            start,
            checkout,
        } => {
            let name = name.trim();
            ensure_branch_name(cwd, name)?;
            ensure_rev(cwd, start)?;
            if *checkout {
                run(cwd, &["switch", "-c", name, start])?;
            } else {
                run(cwd, &["branch", "--", name, start])?;
            }
        }
        GitGraphAction::RenameBranch { from, to } => {
            let to = to.trim();
            ensure_branch_name(cwd, from)?;
            ensure_branch_name(cwd, to)?;
            run(cwd, &["branch", "-m", "--", from, to])?;
        }
        GitGraphAction::DeleteBranch { name, force } => {
            ensure_branch_name(cwd, name)?;
            run(
                cwd,
                &["branch", if *force { "-D" } else { "-d" }, "--", name],
            )?;
        }
        GitGraphAction::DeleteRemoteBranch { name } => {
            let (remote, branch) = split_remote_branch(cwd, name)?;
            ensure_branch_name(cwd, &branch)?;
            run(cwd, &["push", &remote, "--delete", &branch])?;
        }
        GitGraphAction::PushBranch { name } => {
            ensure_branch_name(cwd, name)?;
            let remote = push_remote(cwd, Some(name))?;
            run(cwd, &["push", "--set-upstream", &remote, name])?;
        }
        GitGraphAction::CreateTag {
            name,
            target,
            message,
        } => {
            let name = name.trim();
            ensure_tag_name(cwd, name)?;
            ensure_rev(cwd, target)?;
            match message
                .as_deref()
                .map(str::trim)
                .filter(|message| !message.is_empty())
            {
                Some(message) => run(cwd, &["tag", "-a", "-m", message, "--", name, target])?,
                None => run(cwd, &["tag", "--", name, target])?,
            }
        }
        GitGraphAction::DeleteTag { name } => {
            ensure_tag_name(cwd, name)?;
            run(cwd, &["tag", "-d", "--", name])?;
        }
        GitGraphAction::PushTag { name } => {
            ensure_tag_name(cwd, name)?;
            let remote = push_remote(cwd, None)?;
            run(cwd, &["push", &remote, &format!("refs/tags/{name}")])?;
        }
        GitGraphAction::Merge { rev } => {
            ensure_rev(cwd, rev)?;
            run(cwd, &["merge", "--no-edit", rev])?;
        }
        GitGraphAction::Rebase { onto } => {
            ensure_rev(cwd, onto)?;
            run(cwd, &["rebase", onto])?;
        }
        GitGraphAction::CherryPick { hash } => {
            ensure_hash(hash)?;
            if is_merge(cwd, hash)? {
                run(cwd, &["cherry-pick", "-m", "1", hash])?;
            } else {
                run(cwd, &["cherry-pick", hash])?;
            }
        }
        GitGraphAction::Revert { hash } => {
            ensure_hash(hash)?;
            if is_merge(cwd, hash)? {
                run(cwd, &["revert", "--no-edit", "-m", "1", hash])?;
            } else {
                run(cwd, &["revert", "--no-edit", hash])?;
            }
        }
        GitGraphAction::Reset { hash, mode } => {
            ensure_hash(hash)?;
            let mode = match mode {
                GitResetMode::Soft => "--soft",
                GitResetMode::Mixed => "--mixed",
                GitResetMode::Hard => "--hard",
            };
            run(cwd, &["reset", mode, hash])?;
        }
        GitGraphAction::Stash => {
            run(cwd, &["stash", "push", "--include-untracked"])?;
        }
        GitGraphAction::DiscardUncommitted => {
            if has_head(cwd) {
                run(cwd, &["reset", "--hard", "HEAD"])?;
            } else {
                run(
                    cwd,
                    &["rm", "-r", "--cached", "--quiet", "--ignore-unmatch", "."],
                )?;
            }
            run(cwd, &["clean", "-fd"])?;
        }
    }
    crate::git_branch::inspect(cwd)
}

/// Runs a mutating `git` command. Editors are disabled so a merge, revert, or
/// rebase can never wait on a terminal nobody is watching.
fn run(cwd: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = crate::command_env::plain_command("git")
        .arg("--no-pager")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .output()
        .context("failed to execute git")?;
    if !output.status.success() {
        bail!("{}", action_error(&output));
    }
    Ok(())
}

/// Conflict reports go to stdout, so a failed merge or cherry-pick needs both
/// streams to explain itself.
fn action_error(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = [stderr.trim(), stdout.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if combined.is_empty() {
        return command_error(output);
    }
    if combined.chars().count() <= MAX_ERROR_CHARS {
        return combined;
    }
    let mut clipped = combined.chars().take(MAX_ERROR_CHARS).collect::<String>();
    clipped.push('…');
    clipped
}

fn clip_patch(patch: &str) -> GitFileDiff {
    let mut end = 0;
    for (lines, line) in patch.split_inclusive('\n').enumerate() {
        if lines == MAX_PATCH_LINES || end + line.len() > MAX_PATCH_BYTES {
            return GitFileDiff {
                patch: patch[..end].to_owned(),
                truncated: true,
            };
        }
        end += line.len();
    }
    GitFileDiff {
        patch: patch.to_owned(),
        truncated: false,
    }
}

fn ensure_hash(hash: &str) -> anyhow::Result<()> {
    if is_commit_hash(hash) {
        Ok(())
    } else {
        bail!("invalid commit id")
    }
}

fn ensure_branch_name(cwd: &Path, name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("enter a branch name");
    }
    if name.starts_with('-') {
        bail!("a branch name cannot start with “-”");
    }
    check_ref_format(cwd, &format!("refs/heads/{name}"))
}

fn ensure_tag_name(cwd: &Path, name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("enter a tag name");
    }
    if name.starts_with('-') {
        bail!("a tag name cannot start with “-”");
    }
    check_ref_format(cwd, &format!("refs/tags/{name}"))
}

fn check_ref_format(cwd: &Path, full_name: &str) -> anyhow::Result<()> {
    let output = git(cwd, &["check-ref-format", full_name])?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("invalid ref name: {full_name}")
    }
}

/// A commit id or any name that resolves to a commit.
fn ensure_rev(cwd: &Path, rev: &str) -> anyhow::Result<()> {
    if rev.is_empty() || rev.starts_with('-') {
        bail!("invalid revision");
    }
    let commit = format!("{rev}^{{commit}}");
    let output = git(cwd, &["rev-parse", "--verify", "--quiet", &commit])?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("unknown revision: {rev}")
    }
}

fn has_ref(cwd: &Path, full_name: &str) -> bool {
    git(cwd, &["show-ref", "--verify", "--quiet", full_name])
        .is_ok_and(|output| output.status.success())
}

fn has_head(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .is_ok_and(|output| output.status.success())
}

fn is_untracked(cwd: &Path, path: &str) -> anyhow::Result<bool> {
    let output = git(cwd, &["ls-files", "--error-unmatch", "--", path])?;
    if output.status.success() {
        return Ok(false);
    }
    Ok(cwd.join(path).is_file())
}

fn is_merge(cwd: &Path, hash: &str) -> anyhow::Result<bool> {
    let parents = git_stdout(cwd, &["rev-list", "--parents", "-n", "1", hash])?;
    Ok(parents.split_whitespace().count() > 2)
}

fn remotes(cwd: &Path) -> anyhow::Result<Vec<String>> {
    Ok(git_stdout(cwd, &["remote"])?
        .lines()
        .map(str::trim)
        .filter(|remote| !remote.is_empty())
        .map(str::to_owned)
        .collect())
}

/// `origin/feature/x` → (`origin`, `feature/x`). Remote names may contain `/`
/// themselves, so the longest configured remote that prefixes the name wins.
fn split_remote_branch(cwd: &Path, name: &str) -> anyhow::Result<(String, String)> {
    split_with_remotes(&remotes(cwd)?, name)
        .ok_or_else(|| anyhow!("“{name}” does not belong to a configured remote"))
}

fn split_with_remotes(remotes: &[String], name: &str) -> Option<(String, String)> {
    remotes
        .iter()
        .filter_map(|remote| {
            let branch = name.strip_prefix(remote.as_str())?.strip_prefix('/')?;
            (!branch.is_empty()).then(|| (remote.clone(), branch.to_owned()))
        })
        .max_by_key(|(remote, _)| remote.len())
}

/// The branch's configured remote, else `origin`, else the only remote.
fn push_remote(cwd: &Path, branch: Option<&str>) -> anyhow::Result<String> {
    if let Some(branch) = branch
        && let Ok(remote) = git_stdout(
            cwd,
            &["config", "--get", &format!("branch.{branch}.remote")],
        )
    {
        let remote = remote.trim();
        if !remote.is_empty() && remote != "." {
            return Ok(remote.to_owned());
        }
    }
    let remotes = remotes(cwd)?;
    if remotes.iter().any(|remote| remote == "origin") {
        return Ok("origin".to_owned());
    }
    match remotes.as_slice() {
        [only] => Ok(only.clone()),
        [] => bail!("this repository has no remote"),
        _ => bail!("choose an upstream for this branch first; there is no “origin” remote"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = git(cwd, args).unwrap();
        assert!(output.status.success(), "{}", command_error(&output));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repository() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("waku-graph-ops-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        run_git(&root, &["init", "-b", "main"]);
        run_git(&root, &["config", "user.name", "Waku Tests"]);
        run_git(&root, &["config", "user.email", "waku@example.com"]);
        run_git(&root, &["config", "commit.gpgsign", "false"]);
        root
    }

    fn commit(root: &Path, file: &str, content: &str, message: &str) -> String {
        fs::write(root.join(file), content).unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", message]);
        run_git(root, &["rev-parse", "HEAD"])
    }

    #[test]
    fn remote_branch_split_prefers_the_longest_remote() {
        let remotes = vec!["origin".to_owned(), "origin/mirror".to_owned()];
        assert_eq!(
            split_with_remotes(&remotes, "origin/mirror/main"),
            Some(("origin/mirror".to_owned(), "main".to_owned()))
        );
        assert_eq!(
            split_with_remotes(&remotes, "origin/feature/x"),
            Some(("origin".to_owned(), "feature/x".to_owned()))
        );
        assert_eq!(split_with_remotes(&remotes, "upstream/main"), None);
        assert_eq!(split_with_remotes(&remotes, "origin/"), None);
    }

    #[test]
    fn clip_patch_cuts_on_a_line_boundary() {
        let patch = "a\n".repeat(MAX_PATCH_LINES + 10);
        let clipped = clip_patch(&patch);
        assert!(clipped.truncated);
        assert_eq!(clipped.patch.lines().count(), MAX_PATCH_LINES);
        assert!(!clip_patch("x\ny\n").truncated);
    }

    #[test]
    fn diffs_cover_commits_comparisons_and_the_working_tree() {
        let root = repository();
        let first = commit(&root, "a.txt", "one\n", "first");
        let second = commit(&root, "a.txt", "one\ntwo\n", "second");
        fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(root.join("new.txt"), "fresh\n").unwrap();

        let root_commit = file_diff(
            &root,
            &GitDiffSpec::Commit {
                hash: first.clone(),
            },
            "a.txt",
            None,
        );
        let commit_diff = file_diff(
            &root,
            &GitDiffSpec::Commit {
                hash: second.clone(),
            },
            "a.txt",
            None,
        );
        let compare = file_diff(
            &root,
            &GitDiffSpec::Compare {
                from: first.clone(),
                to: second.clone(),
            },
            "a.txt",
            None,
        );
        let worktree = file_diff(&root, &GitDiffSpec::Uncommitted, "a.txt", None);
        let untracked = file_diff(&root, &GitDiffSpec::Uncommitted, "new.txt", None);
        let files = compare_files(&root, &first, &second);
        let _ = fs::remove_dir_all(&root);

        assert!(root_commit.unwrap().patch.contains("+one"));
        assert!(commit_diff.unwrap().patch.contains("+two"));
        assert!(compare.unwrap().patch.contains("+two"));
        assert!(worktree.unwrap().patch.contains("+three"));
        assert!(untracked.unwrap().patch.contains("+fresh"));
        let files = files.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");
    }

    #[test]
    fn actions_create_tags_and_branches_and_reset() {
        let root = repository();
        let first = commit(&root, "a.txt", "one\n", "first");
        let second = commit(&root, "a.txt", "one\ntwo\n", "second");

        let tag = run_action(
            &root,
            &GitGraphAction::CreateTag {
                name: "v1".into(),
                target: first.clone(),
                message: Some("release".into()),
            },
        );
        let branch = run_action(
            &root,
            &GitGraphAction::CreateBranch {
                name: "topic".into(),
                start: first.clone(),
                checkout: false,
            },
        );
        let bad = run_action(
            &root,
            &GitGraphAction::CreateBranch {
                name: "--force".into(),
                start: first.clone(),
                checkout: false,
            },
        );
        let revert = run_action(
            &root,
            &GitGraphAction::Revert {
                hash: second.clone(),
            },
        );
        let reverted = fs::read_to_string(root.join("a.txt")).unwrap();
        let reset = run_action(
            &root,
            &GitGraphAction::Reset {
                hash: first.clone(),
                mode: GitResetMode::Hard,
            },
        );
        let head = run_git(&root, &["rev-parse", "HEAD"]);
        let tagged = run_git(&root, &["rev-parse", "v1^{commit}"]);
        let topic = run_git(&root, &["rev-parse", "topic"]);
        let _ = fs::remove_dir_all(&root);

        tag.unwrap();
        branch.unwrap();
        assert!(bad.is_err());
        let snapshot = revert.unwrap().unwrap();
        assert_eq!(snapshot.current.as_deref(), Some("main"));
        assert_eq!(reverted.replace("\r\n", "\n"), "one\n");
        reset.unwrap();
        assert_eq!(head, first);
        assert_eq!(tagged, first);
        assert_eq!(topic, first);
    }
}

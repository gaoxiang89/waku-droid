//! Daemon-owned commit graph. Every entry point runs `git` and must be called
//! off the UI thread. [`layout_graph`] is pure and is what the panel paints.

use std::collections::HashMap;
use std::path::Path;
use std::process::Output;

use anyhow::{Context as _, anyhow, bail};

pub use waku_protocol::git::{
    GitCommitDetail, GitGraphCommit, GitGraphEdge, GitGraphFile, GitGraphFileStatus, GitGraphLane,
    GitGraphRef, GitGraphRefKind, GitGraphRow, GitGraphSnapshot,
};

const DEFAULT_LIMIT: usize = 400;
const MAX_LIMIT: usize = 4_000;
const MAX_UNCOMMITTED_FILES: usize = 500;
const MAX_COMMIT_FILES: usize = 500;
const MAX_BODY_CHARS: usize = 16_000;
const LANE_COLOR_COUNT: u8 = 8;

pub fn inspect(cwd: &Path, limit: u32) -> anyhow::Result<Option<GitGraphSnapshot>> {
    if !inside_work_tree(cwd)? {
        return Ok(None);
    }
    let limit = clamped_limit(limit);
    let head = git_stdout(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .ok()
        .map(|head| head.trim().to_owned())
        .filter(|head| is_commit_hash(head));
    let mut commits = parse_log(&read_log(cwd, limit, head.is_some())?)?;
    let truncated = commits.len() > limit;
    if truncated {
        commits.truncate(limit);
    }
    let head_name = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let refs = git_stdout(
        cwd,
        &[
            "for-each-ref",
            "--format=%(objectname)%00%(*objectname)%00%(objecttype)%00%(refname)%00%(HEAD)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )
    .unwrap_or_default();
    let head = head.unwrap_or_default();
    attach_refs(&mut commits, &refs, &head, head_name.trim());
    let head_row = commits
        .iter()
        .position(|commit| commit.hash == head)
        .map(|index| index as u32);
    let rows = layout_graph(&commits);
    let lane_count = rows
        .iter()
        .map(|row| row.lanes.len())
        .max()
        .unwrap_or(0)
        .min(u16::MAX as usize) as u16;
    let uncommitted = git_stdout(cwd, &["status", "--porcelain=v1", "-z"])
        .map(|status| parse_status_z(&status))
        .unwrap_or_default();
    Ok(Some(GitGraphSnapshot {
        commits,
        rows,
        uncommitted,
        head_row,
        lane_count,
        truncated,
        limit: limit as u32,
    }))
}

pub fn commit_detail(cwd: &Path, hash: &str) -> anyhow::Result<GitCommitDetail> {
    if !is_commit_hash(hash) {
        bail!("invalid commit id");
    }
    let body = git_stdout(cwd, &["show", "-s", "--format=%B", hash])?;
    let body = trim_body(&body);
    let files_raw = git_stdout(
        cwd,
        &[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-m",
            "--first-parent",
            "-z",
            hash,
        ],
    )?;
    Ok(GitCommitDetail {
        hash: hash.to_owned(),
        body,
        files: parse_name_status_z(&files_raw),
    })
}

fn clamped_limit(limit: u32) -> usize {
    let limit = limit as usize;
    if limit == 0 {
        DEFAULT_LIMIT
    } else {
        limit.min(MAX_LIMIT)
    }
}

pub(crate) fn is_commit_hash(hash: &str) -> bool {
    matches!(hash.len(), 40 | 64) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn trim_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= MAX_BODY_CHARS {
        return trimmed.to_owned();
    }
    let mut end = 0;
    for (index, (offset, _)) in trimmed.char_indices().enumerate() {
        if index == MAX_BODY_CHARS {
            break;
        }
        end = offset;
    }
    // `end` is the start of the last kept character. Step to its end.
    let end = trimmed[end..]
        .chars()
        .next()
        .map(|ch| end + ch.len_utf8())
        .unwrap_or(end);
    trimmed[..end].to_owned()
}

fn inside_work_tree(cwd: &Path) -> anyhow::Result<bool> {
    let output = git(cwd, &["rev-parse", "--is-inside-work-tree"])?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "true")
}

fn read_log(cwd: &Path, limit: usize, has_head: bool) -> anyhow::Result<String> {
    // One commit past the page is how the panel knows history continues.
    let fetch = limit.saturating_add(1).to_string();
    // Not `--all`: that also walks `refs/waku/*` checkpoints, stash, and notes,
    // which would bury the user's history under hidden bookkeeping commits.
    let mut args = vec![
        "log",
        "--date-order",
        "-n",
        &fetch,
        "--pretty=format:%H%x1f%P%x1f%an%x1f%at%x1f%s%x1e",
        "--branches",
        "--remotes",
        "--tags",
    ];
    if has_head {
        args.push("HEAD");
    }
    args.push("--");
    let output = git(cwd, &args)?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = command_error(&output);
    if stderr.contains("does not have any commits") {
        return Ok(String::new());
    }
    Err(anyhow!(stderr))
}

/// Split `git log --pretty=format:` output. Field separator is `\x1f`, record
/// separator is `\x1e`. A subject is one line, so neither byte occurs in it.
pub(crate) fn parse_log(raw: &str) -> anyhow::Result<Vec<GitGraphCommit>> {
    let mut commits = Vec::new();
    for record in raw.split('\u{1e}') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let mut fields = record.split('\u{1f}');
        let hash = fields.next().unwrap_or("").trim().to_owned();
        let parents = fields.next().unwrap_or("");
        let author = fields.next().unwrap_or("").to_owned();
        let authored_at = fields.next().unwrap_or("");
        let subject = fields.next().unwrap_or("").to_owned();
        if !is_commit_hash(&hash) {
            bail!("git log returned an unreadable commit");
        }
        commits.push(GitGraphCommit {
            hash,
            parents: parents.split_whitespace().map(str::to_owned).collect(),
            author,
            authored_at: authored_at.parse().unwrap_or(0),
            subject,
            refs: Vec::new(),
        });
    }
    Ok(commits)
}

pub(crate) fn attach_refs(
    commits: &mut [GitGraphCommit],
    refs_raw: &str,
    head: &str,
    head_name: &str,
) {
    let detached = head_name == "HEAD";
    let mut by_commit: HashMap<&str, Vec<GitGraphRef>> = HashMap::new();
    for line in refs_raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\0');
        let object = fields.next().unwrap_or("");
        let peeled = fields.next().unwrap_or("");
        let object_type = fields.next().unwrap_or("");
        let name = fields.next().unwrap_or("");
        let head_mark = fields.next().unwrap_or("");
        let commit = if object_type == "tag" && !peeled.is_empty() {
            peeled
        } else {
            object
        };
        let Some(git_ref) = classify_ref(name, head_mark.trim() == "*") else {
            continue;
        };
        by_commit.entry(commit).or_default().push(git_ref);
    }
    for commit in commits.iter_mut() {
        let mut refs = by_commit.remove(commit.hash.as_str()).unwrap_or_default();
        if detached && !head.is_empty() && commit.hash == head {
            refs.push(GitGraphRef {
                kind: GitGraphRefKind::Head,
                name: "HEAD".to_owned(),
                current: true,
            });
        }
        refs.sort_by(|left, right| {
            ref_rank(left)
                .cmp(&ref_rank(right))
                .then(left.name.cmp(&right.name))
        });
        commit.refs = refs;
    }
}

fn classify_ref(name: &str, current: bool) -> Option<GitGraphRef> {
    let (kind, short) = if let Some(branch) = name.strip_prefix("refs/heads/") {
        (GitGraphRefKind::Branch, branch)
    } else if let Some(remote) = name.strip_prefix("refs/remotes/") {
        if remote.ends_with("/HEAD") {
            return None;
        }
        (GitGraphRefKind::Remote, remote)
    } else {
        (GitGraphRefKind::Tag, name.strip_prefix("refs/tags/")?)
    };
    if short.is_empty() {
        return None;
    }
    Some(GitGraphRef {
        kind,
        name: short.to_owned(),
        current: current && kind == GitGraphRefKind::Branch,
    })
}

fn ref_rank(git_ref: &GitGraphRef) -> u8 {
    match (git_ref.current, git_ref.kind) {
        (true, _) => 0,
        (_, GitGraphRefKind::Head) => 1,
        (_, GitGraphRefKind::Branch) => 2,
        (_, GitGraphRefKind::Remote) => 3,
        (_, GitGraphRefKind::Tag) => 4,
    }
}

pub(crate) fn parse_status_z(raw: &str) -> Vec<GitGraphFile> {
    let mut files = Vec::new();
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() && files.len() < MAX_UNCOMMITTED_FILES {
        let Some(end) = bytes[index..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let header = &raw[index..index + end];
        index += end + 1;
        if header.len() < 3 {
            continue;
        }
        let status = &header[..2];
        let path = header[3..].to_owned();
        let renamed = status.contains('R') || status.contains('C');
        // With `-z`, git reverses the `ORIG -> PATH` order: the record is
        // `XY PATH\0ORIG\0`, so the header holds the new path.
        let old_path = if renamed {
            let Some(next) = bytes[index..].iter().position(|byte| *byte == 0) else {
                break;
            };
            let old = raw[index..index + next].to_owned();
            index += next + 1;
            Some(old)
        } else {
            None
        };
        if path.is_empty() {
            continue;
        }
        files.push(GitGraphFile {
            path,
            old_path,
            status: status_from_xy(status),
        });
    }
    files
}

fn status_from_xy(status: &str) -> GitGraphFileStatus {
    if status.contains('?') {
        return GitGraphFileStatus::Untracked;
    }
    let code = status.chars().find(|ch| *ch != ' ').unwrap_or('M');
    match code {
        'A' => GitGraphFileStatus::Added,
        'D' => GitGraphFileStatus::Deleted,
        'R' => GitGraphFileStatus::Renamed,
        'C' => GitGraphFileStatus::Copied,
        'T' => GitGraphFileStatus::TypeChanged,
        '?' => GitGraphFileStatus::Untracked,
        _ => GitGraphFileStatus::Modified,
    }
}

pub(crate) fn parse_name_status_z(raw: &str) -> Vec<GitGraphFile> {
    let mut files = Vec::new();
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() && files.len() < MAX_COMMIT_FILES {
        let Some(end) = bytes[index..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let status = &raw[index..index + end];
        index += end + 1;
        if status.is_empty() {
            continue;
        }
        let code = status.chars().next().unwrap_or('M');
        let renamed = code == 'R' || code == 'C';
        let Some(path_end) = bytes[index..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let first = raw[index..index + path_end].to_owned();
        index += path_end + 1;
        let (path, old_path) = if renamed {
            let Some(new_end) = bytes[index..].iter().position(|byte| *byte == 0) else {
                break;
            };
            let new_path = raw[index..index + new_end].to_owned();
            index += new_end + 1;
            (new_path, Some(first))
        } else {
            (first, None)
        };
        if path.is_empty() {
            continue;
        }
        files.push(GitGraphFile {
            path,
            old_path,
            status: match code {
                'A' => GitGraphFileStatus::Added,
                'D' => GitGraphFileStatus::Deleted,
                'R' => GitGraphFileStatus::Renamed,
                'C' => GitGraphFileStatus::Copied,
                'T' => GitGraphFileStatus::TypeChanged,
                _ => GitGraphFileStatus::Modified,
            },
        });
    }
    files
}

#[derive(Clone)]
struct ActiveLane {
    hash: String,
    color: u8,
}

/// Lane assignment for a newest-first commit list.
///
/// A column remembers the commit it expects next. The first parent keeps that
/// column; further parents take a free column. When a parent is already
/// expected on another column, the two join and one column frees up. A freed
/// column stays a hole until something reuses it, so no line ever has to jump
/// sideways between rows. Colors stay with the column, so a straight line does
/// not change hue.
pub fn layout_graph(commits: &[GitGraphCommit]) -> Vec<GitGraphRow> {
    let mut active: Vec<Option<ActiveLane>> = Vec::new();
    let mut next_color: u8 = 0;
    let mut rows = Vec::with_capacity(commits.len());

    for commit in commits {
        let existing = find_lane(&active, &commit.hash);
        let fresh = existing.is_none();
        let node = existing.unwrap_or_else(|| {
            let color = take_color(&mut next_color);
            let slot = active
                .iter()
                .position(Option::is_none)
                .unwrap_or(active.len());
            if slot == active.len() {
                active.push(None);
            }
            active[slot] = Some(ActiveLane {
                hash: commit.hash.clone(),
                color,
            });
            slot
        });
        let node_color = active[node].as_ref().map_or(0, |lane| lane.color);
        let mut lanes: Vec<GitGraphLane> = active
            .iter()
            .enumerate()
            .map(|(index, lane)| match lane {
                Some(lane) => GitGraphLane {
                    color: lane.color,
                    up: index != node || !fresh,
                    down: index != node,
                    node: index == node,
                },
                None => empty_lane(0),
            })
            .collect();

        let mut edges = Vec::new();
        let mut next = active.clone();
        match commit.parents.split_first() {
            None => next[node] = None,
            Some((first, rest)) => {
                route_first_parent(&mut next, &mut lanes, &mut edges, node, node_color, first);
                for parent in rest {
                    route_extra_parent(
                        &mut next,
                        &mut lanes,
                        &mut edges,
                        node,
                        &mut next_color,
                        parent,
                    );
                }
            }
        }
        while matches!(next.last(), Some(None)) {
            next.pop();
        }

        rows.push(GitGraphRow {
            node_lane: node as u16,
            lanes,
            edges,
        });
        active = next;
    }

    rows
}

fn find_lane(lanes: &[Option<ActiveLane>], hash: &str) -> Option<usize> {
    lanes
        .iter()
        .position(|lane| lane.as_ref().is_some_and(|lane| lane.hash == hash))
}

fn take_color(next_color: &mut u8) -> u8 {
    let color = *next_color;
    *next_color = (color + 1) % LANE_COLOR_COUNT;
    color
}

fn empty_lane(color: u8) -> GitGraphLane {
    GitGraphLane {
        color,
        up: false,
        down: false,
        node: false,
    }
}

fn route_first_parent(
    next: &mut [Option<ActiveLane>],
    lanes: &mut [GitGraphLane],
    edges: &mut Vec<GitGraphEdge>,
    node: usize,
    node_color: u8,
    parent: &str,
) {
    let continue_here = |next: &mut [Option<ActiveLane>], lanes: &mut [GitGraphLane]| {
        next[node] = Some(ActiveLane {
            hash: parent.to_owned(),
            color: node_color,
        });
        lanes[node].down = true;
    };
    match find_lane(next, parent) {
        None => continue_here(next, lanes),
        Some(existing) if existing < node => {
            edges.push(GitGraphEdge {
                from: node as u16,
                to: existing as u16,
                color: node_color,
            });
            next[node] = None;
        }
        Some(existing) => {
            // The parent waits further right. Pull it into this column so the
            // left-most line stays straight.
            let color = next[existing].as_ref().map_or(0, |lane| lane.color);
            edges.push(GitGraphEdge {
                from: existing as u16,
                to: node as u16,
                color,
            });
            lanes[existing].down = false;
            next[existing] = None;
            continue_here(next, lanes);
        }
    }
}

fn route_extra_parent(
    next: &mut Vec<Option<ActiveLane>>,
    lanes: &mut Vec<GitGraphLane>,
    edges: &mut Vec<GitGraphEdge>,
    node: usize,
    next_color: &mut u8,
    parent: &str,
) {
    if let Some(existing) = find_lane(next, parent) {
        edges.push(GitGraphEdge {
            from: node as u16,
            to: existing as u16,
            color: next[existing].as_ref().map_or(0, |lane| lane.color),
        });
        return;
    }
    let color = take_color(next_color);
    // Only a column that is empty for the whole row can take the new line;
    // one freed in this row still paints its upper half.
    let slot = (0..next.len())
        .find(|&index| next[index].is_none() && !lanes[index].up && !lanes[index].node)
        .unwrap_or_else(|| {
            next.push(None);
            lanes.push(empty_lane(color));
            next.len() - 1
        });
    lanes[slot].color = color;
    next[slot] = Some(ActiveLane {
        hash: parent.to_owned(),
        color,
    });
    edges.push(GitGraphEdge {
        from: node as u16,
        to: slot as u16,
        color,
    });
}
pub(crate) fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<Output> {
    crate::command_env::plain_command("git")
        .arg("--no-pager")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("failed to execute git")
}

pub(crate) fn git_stdout(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = git(cwd, args)?;
    if !output.status.success() {
        bail!("{}", command_error(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn command_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        stderr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample(hash: &str, parents: &[&str]) -> GitGraphCommit {
        GitGraphCommit {
            hash: format!("{hash:0<40}"),
            parents: parents
                .iter()
                .map(|parent| format!("{parent:0<40}"))
                .collect(),
            author: "Ada".into(),
            authored_at: 0,
            subject: hash.into(),
            refs: Vec::new(),
        }
    }

    fn lane(color: u8, up: bool, down: bool, node: bool) -> GitGraphLane {
        GitGraphLane {
            color,
            up,
            down,
            node,
        }
    }

    #[test]
    fn a_straight_line_stays_on_one_lane() {
        let commits = vec![sample("a", &["b"]), sample("b", &["c"]), sample("c", &[])];
        let rows = layout_graph(&commits);
        assert_eq!(rows[0].node_lane, 0);
        assert_eq!(rows[0].lanes, vec![lane(0, false, true, true)]);
        assert!(rows[0].edges.is_empty());
        assert_eq!(rows[1].lanes, vec![lane(0, true, true, true)]);
        assert_eq!(rows[2].lanes, vec![lane(0, true, false, true)]);
        assert!(rows[2].edges.is_empty());
    }

    #[test]
    fn a_side_branch_joins_its_parent_lane() {
        // A and B both parent C. Newest first: A, B, C.
        let commits = vec![sample("a", &["c"]), sample("b", &["c"]), sample("c", &[])];
        let rows = layout_graph(&commits);
        assert_eq!(rows[0].lanes, vec![lane(0, false, true, true)]);
        assert_eq!(rows[1].node_lane, 1);
        assert_eq!(
            rows[1].lanes,
            vec![lane(0, true, true, false), lane(1, false, false, true)]
        );
        assert_eq!(
            rows[1].edges,
            vec![GitGraphEdge {
                from: 1,
                to: 0,
                color: 1
            }]
        );
        assert_eq!(rows[2].lanes, vec![lane(0, true, false, true)]);
        assert!(rows[2].edges.is_empty());
    }

    #[test]
    fn a_merge_keeps_the_first_parent_and_curves_the_second_in() {
        // M parents A and B. Then B, A, and their shared parent C.
        let commits = vec![
            sample("m", &["a", "b"]),
            sample("b", &["c"]),
            sample("a", &["c"]),
            sample("c", &[]),
        ];
        let rows = layout_graph(&commits);
        assert_eq!(rows[0].node_lane, 0);
        assert_eq!(
            rows[0].lanes,
            vec![lane(0, false, true, true), lane(1, false, false, false)]
        );
        assert_eq!(
            rows[0].edges,
            vec![GitGraphEdge {
                from: 0,
                to: 1,
                color: 1
            }]
        );
        assert_eq!(
            rows[1].lanes,
            vec![lane(0, true, true, false), lane(1, true, true, true)]
        );
        assert_eq!(
            rows[2].lanes,
            vec![lane(0, true, true, true), lane(1, true, false, false)]
        );
        assert_eq!(
            rows[2].edges,
            vec![GitGraphEdge {
                from: 1,
                to: 0,
                color: 1
            }]
        );
        assert_eq!(rows[3].lanes, vec![lane(0, true, false, true)]);
    }

    #[test]
    fn an_octopus_merge_opens_one_lane_per_extra_parent() {
        let commits = vec![sample("m", &["a", "b", "d"])];
        let rows = layout_graph(&commits);
        assert_eq!(rows[0].lanes.len(), 3);
        assert!(rows[0].lanes[0].node && rows[0].lanes[0].down);
        assert!(!rows[0].lanes[1].node && !rows[0].lanes[1].up);
        assert!(!rows[0].lanes[2].node && !rows[0].lanes[2].up);
        assert_eq!(
            rows[0].edges,
            vec![
                GitGraphEdge {
                    from: 0,
                    to: 1,
                    color: 1
                },
                GitGraphEdge {
                    from: 0,
                    to: 2,
                    color: 2
                },
            ]
        );
    }

    #[test]
    fn an_empty_history_has_no_rows() {
        assert!(layout_graph(&[]).is_empty());
    }

    #[test]
    fn log_records_keep_subjects_and_parents() {
        let hash_a = "a".repeat(40);
        let hash_b = "b".repeat(40);
        let raw = format!("{hash_a}\u{1f}{hash_b}\u{1f}Ada\u{1f}1700000000\u{1f}first\u{1e}");
        let commits = parse_log(&raw).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].hash, hash_a);
        assert_eq!(commits[0].parents, vec![hash_b]);
        assert_eq!(commits[0].author, "Ada");
        assert_eq!(commits[0].authored_at, 1_700_000_000);
        assert_eq!(commits[0].subject, "first");
    }

    #[test]
    fn refs_distinguish_the_current_branch_remotes_and_tags() {
        let mut commits = vec![sample("a", &[])];
        let hash = commits[0].hash.clone();
        let tag_object = "c".repeat(40);
        let raw = format!(
            "{hash}\0\0commit\0refs/heads/main\0*\n\
             {hash}\0\0commit\0refs/remotes/origin/main\0 \n\
             {hash}\0\0commit\0refs/remotes/origin/HEAD\0 \n\
             {tag_object}\0{hash}\0tag\0refs/tags/v1\0 \n"
        );
        attach_refs(&mut commits, &raw, &hash, "main");
        let refs = &commits[0].refs;
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].name, "main");
        assert!(refs[0].current);
        assert_eq!(refs[0].kind, GitGraphRefKind::Branch);
        assert_eq!(refs[1].kind, GitGraphRefKind::Remote);
        assert_eq!(refs[1].name, "origin/main");
        assert_eq!(refs[2].kind, GitGraphRefKind::Tag);
        assert_eq!(refs[2].name, "v1");
    }

    #[test]
    fn a_detached_head_is_labeled_on_its_commit() {
        let mut commits = vec![sample("d", &[])];
        let hash = commits[0].hash.clone();
        attach_refs(&mut commits, "", &hash, "HEAD");
        assert_eq!(commits[0].refs.len(), 1);
        assert_eq!(commits[0].refs[0].kind, GitGraphRefKind::Head);
        assert!(commits[0].refs[0].current);
    }

    #[test]
    fn a_closing_lane_leaves_its_neighbors_in_place() {
        // Three tips on lanes 0..2. `pb` ends lane 1; lane 2 must not slide
        // left, and the next fresh tip reuses the hole.
        let commits = vec![
            sample("a", &["pa"]),
            sample("b", &["pb"]),
            sample("c", &["pc"]),
            sample("pb", &[]),
            sample("d", &["pd"]),
            sample("pc", &[]),
        ];
        let rows = layout_graph(&commits);
        assert_eq!(rows[3].node_lane, 1);
        assert_eq!(rows[3].lanes[2], lane(2, true, true, false));
        assert_eq!(rows[4].node_lane, 1);
        assert_eq!(rows[4].lanes[1], lane(3, false, true, true));
        assert_eq!(rows[4].lanes[2], lane(2, true, true, false));
        assert_eq!(rows[5].node_lane, 2);
        assert!(rows[5].lanes[2].up);
    }

    #[test]
    fn a_merge_beside_an_open_lane_does_not_shift_it() {
        let commits = vec![
            sample("k", &["pk"]),
            sample("j", &["pj"]),
            sample("pk", &["x", "y"]),
        ];
        let rows = layout_graph(&commits);
        assert_eq!(rows[2].node_lane, 0);
        assert_eq!(rows[2].lanes[1], lane(1, true, true, false));
        assert_eq!(rows[2].lanes.len(), 3);
        assert_eq!(
            rows[2].edges,
            vec![GitGraphEdge {
                from: 0,
                to: 2,
                color: 2
            }]
        );
    }

    #[test]
    fn status_z_reads_a_rename_as_new_then_old() {
        // `git status -z` reverses `ORIG -> PATH`: the new path comes first.
        let raw = "R  new.rs\0old.rs\0?? untracked.rs\0";
        let files = parse_status_z(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].status, GitGraphFileStatus::Renamed);
        assert_eq!(files[0].old_path.as_deref(), Some("old.rs"));
        assert_eq!(files[0].path, "new.rs");
        assert_eq!(files[1].status, GitGraphFileStatus::Untracked);
        assert_eq!(files[1].path, "untracked.rs");
    }

    #[test]
    fn name_status_z_reads_add_and_rename() {
        let raw = "A\0README.md\0R100\0old.rs\0new.rs\0";
        let files = parse_name_status_z(raw);
        assert_eq!(files[0].status, GitGraphFileStatus::Added);
        assert_eq!(files[0].path, "README.md");
        assert_eq!(files[1].status, GitGraphFileStatus::Renamed);
        assert_eq!(files[1].old_path.as_deref(), Some("old.rs"));
        assert_eq!(files[1].path, "new.rs");
    }

    #[test]
    fn inspect_reads_a_repository_without_commits() {
        let root = std::env::temp_dir().join(format!("waku-graph-empty-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        run_git(&root, &["init", "-b", "main"]);
        fs::write(root.join("new.txt"), "wip\n").unwrap();
        let snapshot = inspect(&root, 10);
        let _ = fs::remove_dir_all(&root);
        let snapshot = snapshot.unwrap().unwrap();
        assert!(snapshot.commits.is_empty());
        assert_eq!(snapshot.head_row, None);
        assert!(
            snapshot
                .uncommitted
                .iter()
                .any(|file| file.path == "new.txt")
        );
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = git(cwd, args).unwrap();
        assert!(output.status.success(), "{}", command_error(&output));
    }

    #[test]
    fn inspect_reads_a_real_repository() {
        let root = std::env::temp_dir().join(format!("waku-graph-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        run_git(&root, &["init", "-b", "main"]);
        fs::write(root.join("README.md"), "main\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(
            &root,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "initial",
            ],
        );
        run_git(&root, &["branch", "feature"]);
        fs::write(root.join("dirty.txt"), "wip\n").unwrap();

        // A Waku checkpoint ref must not surface as history.
        run_git(
            &root,
            &["update-ref", "refs/waku/session-test-turn-1", "HEAD"],
        );
        run_git(
            &root,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "checkpoint",
            ],
        );
        run_git(
            &root,
            &["update-ref", "refs/waku/session-test-turn-2", "HEAD"],
        );
        run_git(&root, &["reset", "--soft", "HEAD~1"]);

        let snapshot = inspect(&root, 10).unwrap().unwrap();
        assert_eq!(snapshot.commits.len(), 1);
        assert_eq!(snapshot.head_row, Some(0));
        assert_eq!(snapshot.lane_count, 1);
        assert_eq!(snapshot.rows.len(), 1);
        assert!(snapshot.rows[0].lanes[0].node);
        assert!(
            snapshot.commits[0]
                .refs
                .iter()
                .any(|git_ref| git_ref.name == "main" && git_ref.current)
        );
        assert!(
            snapshot.commits[0]
                .refs
                .iter()
                .any(|git_ref| git_ref.name == "feature" && !git_ref.current)
        );
        assert!(
            snapshot
                .uncommitted
                .iter()
                .any(|file| file.path == "dirty.txt")
        );

        let detail = commit_detail(&root, &snapshot.commits[0].hash).unwrap();
        assert!(detail.body.contains("initial"));
        assert!(
            detail
                .files
                .iter()
                .any(|file| file.path == "README.md" && file.status == GitGraphFileStatus::Added)
        );

        // A child of the repository is still inside the work tree. The
        // negative case has to live beside it.
        let missing =
            std::env::temp_dir().join(format!("waku-not-a-repo-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&missing).unwrap();
        let outside = inspect(&missing, 10);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&missing);
        assert!(outside.unwrap().is_none());
    }
}

//! Native filesystem events on the daemon host. Clients renew a lease and read
//! counters; idle polls never walk the disk or run Git. Watchers are bounded,
//! shared by workspace, and retire when no visible client needs them.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Event, EventKind, RecursiveMode, Watcher, event::ModifyKind};
use parking_lot::Mutex;
use waku_protocol::workspace::WorkspaceWatchState;

const MAX_WORKSPACES: usize = 4;
const MAX_DIRECTORIES: usize = 2048;
const MAX_ENTRIES: usize = 100_000;
const LEASE: Duration = Duration::from_secs(30);
const DEBOUNCE: Duration = Duration::from_millis(250);
const MAX_BATCH_DELAY: Duration = Duration::from_secs(1);
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Default, PartialEq)]
struct Config {
    directories: Vec<PathBuf>,
    recursive: bool,
    watch_git: bool,
}

struct Control {
    clients: HashMap<uuid::Uuid, (Config, Instant)>,
    config: Config,
    last_poll: Instant,
    state: WorkspaceWatchState,
}

struct Lease {
    control: Mutex<Control>,
    stop: AtomicBool,
}

pub(super) fn poll(
    client_id: uuid::Uuid,
    root: PathBuf,
    mut directories: Vec<PathBuf>,
    recursive: bool,
    watch_git: bool,
) -> anyhow::Result<WorkspaceWatchState> {
    anyhow::ensure!(
        root.is_absolute(),
        "workspace watch requires an absolute root"
    );
    anyhow::ensure!(
        directories.len() <= MAX_DIRECTORIES,
        "too many visible directories"
    );
    anyhow::ensure!(
        directories.iter().all(|path| path
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))),
        "invalid watch directory"
    );
    directories.sort();
    directories.dedup();
    let config = Config {
        directories,
        recursive,
        watch_git,
    };
    static WATCHES: OnceLock<Mutex<HashMap<PathBuf, Arc<Lease>>>> = OnceLock::new();
    let mut watches = WATCHES.get_or_init(Default::default).lock();
    watches.retain(|_, lease| !lease.stop.load(Ordering::Relaxed));
    if let Some(lease) = watches.get(&root) {
        let mut control = lease.control.lock();
        // A lease may already have been retired by the worker since retain.
        if !lease.stop.load(Ordering::Relaxed) {
            control
                .clients
                .retain(|_, (_, last)| last.elapsed() < LEASE);
            anyhow::ensure!(
                control.clients.contains_key(&client_id) || control.clients.len() < 16,
                "too many workspace watch clients"
            );
            control.clients.insert(client_id, (config, Instant::now()));
            control.config = merged_config(&control.clients);
            control.last_poll = Instant::now();
            return Ok(control.state.clone());
        }
    }
    // Refuse excess active roots rather than evicting another visible client's
    // watch and causing both clients to repeatedly rebuild their subscriptions.
    anyhow::ensure!(
        watches.len() < MAX_WORKSPACES,
        "workspace watch capacity reached"
    );
    let lease = Arc::new(Lease {
        control: Mutex::new(Control {
            clients: HashMap::from([(client_id, (config.clone(), Instant::now()))]),
            config,
            last_poll: Instant::now(),
            state: WorkspaceWatchState {
                epoch: uuid::Uuid::new_v4(),
                tree: 0,
                content: 0,
                worktree: 0,
                git: 0,
                ready: false,
                limited: false,
            },
        }),
        stop: AtomicBool::new(false),
    });
    let worker = lease.clone();
    let worker_root = root.clone();
    std::thread::Builder::new()
        .name("waku-workspace-watch".into())
        .spawn(move || {
            if let Err(error) = run(&worker_root, &worker) {
                eprintln!(
                    "workspace watch unavailable for {}: {error}",
                    worker_root.display()
                );
                let mut control = worker.control.lock();
                control.state.limited = true;
                control.state.ready = true;
                drop(control);
                // A missing backend/root must not restart a failed watcher each
                // second. Keep its degraded status until the lease expires.
                while worker.control.lock().last_poll.elapsed() < LEASE {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            worker.stop.store(true, Ordering::Relaxed);
        })?;
    let state = lease.control.lock().state.clone();
    watches.insert(root, lease);
    Ok(state)
}

fn merged_config(clients: &HashMap<uuid::Uuid, (Config, Instant)>) -> Config {
    let mut merged = Config::default();
    for (config, _) in clients.values() {
        merged.recursive |= config.recursive;
        merged.watch_git |= config.watch_git;
        merged
            .directories
            .extend(config.directories.iter().cloned());
    }
    merged.directories.sort();
    merged.directories.dedup();
    merged.directories.truncate(MAX_DIRECTORIES);
    merged
}

#[derive(Default)]
struct Dirty {
    tree: bool,
    content: bool,
    worktree: bool,
    git: bool,
}

impl Dirty {
    fn any(&self) -> bool {
        self.tree || self.content || self.worktree || self.git
    }
    fn all() -> Self {
        Self {
            tree: true,
            content: true,
            worktree: true,
            git: true,
        }
    }
    fn publish(&mut self, state: &mut WorkspaceWatchState) {
        state.tree = state.tree.wrapping_add(self.tree as u64);
        state.content = state.content.wrapping_add(self.content as u64);
        state.worktree = state.worktree.wrapping_add(self.worktree as u64);
        state.git = state.git.wrapping_add(self.git as u64);
        *self = Self::default();
    }
}

struct Scope {
    root: PathBuf,
    git_dirs: Vec<PathBuf>,
    watched: HashSet<PathBuf>,
    ignores: HashMap<PathBuf, Gitignore>,
    tracked: HashSet<PathBuf>,
}

impl Scope {
    fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        if self.tracked.contains(path) {
            return false;
        }
        for ancestor in path.parent().into_iter().flat_map(Path::ancestors) {
            if let Some(ignore) = self.ignores.get(ancestor) {
                let matched = ignore.matched_path_or_any_parents(path, is_dir);
                if !matched.is_none() {
                    return matched.is_ignore();
                }
            }
        }
        false
    }

    fn git_path(&self, path: &Path) -> Option<bool> {
        self.git_dirs.iter().find_map(|root| {
            path.strip_prefix(root).ok().map(|relative| {
                if relative.extension().is_some_and(|ext| ext == "lock") {
                    return false;
                }
                matches!(
                    relative.to_str(),
                    Some("HEAD" | "index" | "packed-refs" | "config")
                ) || relative == Path::new("info/exclude")
                    || relative.starts_with("refs/heads")
                    || relative.starts_with("refs/remotes")
                    || relative.starts_with("refs/tags")
            })
        })
    }

    /// Called by the worker, never by the OS callback or the UI thread.
    fn event(&self, event: Event, dirty: &mut Dirty) -> bool {
        let topology = matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Remove(_)
                | EventKind::Modify(ModifyKind::Name(_))
                | EventKind::Any
        );
        let mut rescan = event.need_rescan();
        if rescan {
            *dirty = Dirty::all();
        }
        for path in event.paths {
            if let Some(relevant) = self.git_path(&path) {
                dirty.git |= relevant;
                rescan |=
                    relevant && (topology || path.file_name().is_some_and(|name| name == "index"));
                continue;
            }
            let Ok(relative) = path.strip_prefix(&self.root) else {
                continue;
            };
            if relative == Path::new(".git") && topology && self.git_dirs.is_empty() {
                *dirty = Dirty::all();
                rescan = true;
            }
            if relative.components().any(|part| part.as_os_str() == ".git") {
                continue;
            }
            // Directory mtime echoes from ignored children carry no additional
            // information and can otherwise create a refresh feedback loop.
            if !topology && self.watched.contains(&path) {
                continue;
            }
            let is_dir = self.watched.contains(&path) || (topology && path.is_dir());
            let ignored = self.ignored(&path, is_dir);
            dirty.tree |= topology;
            dirty.content = true;
            dirty.worktree |= !ignored;
            // New ignored build directories do not expand Review's watch
            // scope. An already watched directory still needs removal/re-arm.
            rescan |= topology && is_dir && (!ignored || self.watched.contains(&path));
            rescan |= path
                .file_name()
                .is_some_and(|name| name == ".gitignore" || name == ".ignore");
        }
        rescan
    }

    fn configure(&mut self, config: &Config, watcher: &mut impl Watcher) -> bool {
        let started = Instant::now();
        if config.watch_git && self.git_dirs.is_empty() {
            self.git_dirs = git_directories(&self.root);
        }
        let mut wanted = HashSet::from([self.root.clone()]);
        self.ignores.clear();
        self.tracked.clear();
        let mut limited = false;
        if config.recursive {
            let walker = ignore::WalkBuilder::new(&self.root)
                .hidden(false)
                .follow_links(false)
                .require_git(false)
                .filter_entry(|entry| entry.file_name() != ".git")
                .build();
            for (count, entry) in walker.enumerate() {
                if count >= MAX_ENTRIES
                    || wanted.len() >= MAX_DIRECTORIES
                    || started.elapsed() > Duration::from_secs(2)
                {
                    limited = true;
                    break;
                }
                match entry {
                    Ok(entry) if entry.file_type().is_some_and(|kind| kind.is_dir()) => {
                        wanted.insert(entry.into_path());
                    }
                    Err(_) => limited = true,
                    _ => {}
                }
            }
            // Gitignore does not exclude already tracked files. Keep their
            // parents watched too, even if an ignore rule was added later.
            if let Ok(mut child) = crate::command_env::plain_command("git")
                .args(["ls-files", "-z"])
                .current_dir(&self.root)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                let stdout = child.stdout.take().expect("piped git stdout");
                for (count, path) in BufReader::new(stdout).split(0).enumerate() {
                    if count >= MAX_ENTRIES || wanted.len() >= MAX_DIRECTORIES {
                        limited = true;
                        let _ = child.kill();
                        break;
                    }
                    let Ok(path) = path else {
                        limited = true;
                        break;
                    };
                    if path.is_empty() {
                        continue;
                    }
                    let path = self.root.join(String::from_utf8_lossy(&path).as_ref());
                    if let Some(parent) = path.parent() {
                        wanted.insert(parent.to_owned());
                    }
                    self.tracked.insert(path);
                }
                let _ = child.wait();
            }
        }
        for directory in &config.directories {
            if wanted.len() >= MAX_DIRECTORIES {
                limited = true;
                break;
            }
            if let Ok(path) = self.root.join(directory).canonicalize()
                && path.starts_with(&self.root)
                && path.is_dir()
            {
                wanted.insert(path);
            }
        }
        // Build each directory's ignore rules once, including ancestors of
        // explicitly expanded directories. Event filtering then touches memory.
        for directory in &wanted {
            for ancestor in directory.ancestors() {
                self.ignores.entry(ancestor.to_owned()).or_insert_with(|| {
                    let mut builder = GitignoreBuilder::new(ancestor);
                    if ancestor == self.root {
                        for git_dir in &self.git_dirs {
                            builder.add(git_dir.join("info/exclude"));
                        }
                    }
                    builder.add(ancestor.join(".gitignore"));
                    builder.add(ancestor.join(".ignore"));
                    builder.build().unwrap_or_else(|_| Gitignore::empty())
                });
            }
        }
        if config.watch_git {
            for root in &self.git_dirs {
                wanted.insert(root.clone());
                wanted.insert(root.join("info"));
                wanted.insert(root.join("refs"));
                for name in ["heads", "remotes", "tags"] {
                    for entry in ignore::WalkBuilder::new(root.join("refs").join(name))
                        .hidden(false)
                        .follow_links(false)
                        .build()
                        .flatten()
                    {
                        if wanted.len() >= MAX_DIRECTORIES {
                            limited = true;
                            break;
                        }
                        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                            wanted.insert(entry.into_path());
                        }
                    }
                }
            }
        }
        // Keep the parent alive so deleting and recreating the workspace can
        // recover without retaining stale directory handles.
        if let Some(parent) = self.root.parent() {
            wanted.insert(parent.to_owned());
        }
        for path in self.watched.difference(&wanted) {
            let _ = watcher.unwatch(path);
        }
        let mut installed = self
            .watched
            .intersection(&wanted)
            .cloned()
            .collect::<HashSet<_>>();
        for path in wanted.difference(&self.watched) {
            if !path.is_dir() {
                continue;
            }
            match watcher.watch(path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    installed.insert(path.clone());
                }
                Err(_) => limited = true,
            }
        }
        self.watched = installed;
        limited
    }
}

fn git_directories(root: &Path) -> Vec<PathBuf> {
    let Ok(output) = crate::command_env::plain_command("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])
        .current_dir(root)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut dirs = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|path| Path::new(path).canonicalize().ok())
        .collect::<Vec<_>>();
    // Specific worktree Git directory must match before the shared parent.
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    dirs.dedup();
    dirs
}

fn run(root: &Path, lease: &Lease) -> anyhow::Result<()> {
    let root = root.canonicalize()?;
    let (tx, rx) = bounded(256);
    let overflow = Arc::new(AtomicBool::new(false));
    let callback_overflow = overflow.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        if event
            .as_ref()
            .is_ok_and(|event| matches!(event.kind, EventKind::Access(_)))
        {
            return;
        }
        if tx.try_send(event).is_err() {
            callback_overflow.store(true, Ordering::Relaxed);
        }
    })?;
    let mut scope = Scope {
        git_dirs: git_directories(&root),
        root,
        watched: HashSet::new(),
        ignores: HashMap::new(),
        tracked: HashSet::new(),
    };
    let mut config = lease.control.lock().config.clone();
    let limited = scope.configure(&config, &mut watcher);
    {
        let mut control = lease.control.lock();
        control.state.ready = true;
        control.state.limited = limited;
    }
    let mut dirty = Dirty::default();
    let mut first_event = Instant::now();
    let mut last_event = first_event;
    let mut last_scan = first_event;
    let mut rescan = false;
    let mut reconcile_review = false;
    loop {
        {
            let mut control = lease.control.lock();
            if control.last_poll.elapsed() >= LEASE || lease.stop.load(Ordering::Relaxed) {
                break;
            }
            let previous_clients = control.clients.len();
            control
                .clients
                .retain(|_, (_, last)| last.elapsed() < LEASE);
            if control.clients.len() != previous_clients {
                control.config = merged_config(&control.clients);
            }
            if config != control.config {
                config = control.config.clone();
                rescan = true;
                reconcile_review = true;
            }
        }
        let mut events = receive_batch(
            &rx,
            if dirty.any() {
                DEBOUNCE
            } else {
                Duration::from_secs(1)
            },
        );
        if overflow.swap(false, Ordering::Relaxed) {
            if !dirty.any() {
                first_event = Instant::now();
            }
            dirty = Dirty::all();
            rescan = true;
            reconcile_review = true;
            last_event = Instant::now();
        }
        for event in events.drain(..) {
            let was_dirty = dirty.any();
            match event {
                Ok(event) => {
                    let changed = scope.event(event.clone(), &mut dirty);
                    if changed {
                        reconcile_review |= dirty.worktree || dirty.git;
                    }
                    // A directory handle can become invalid on atomic rename
                    // or deletion. Remove it before the next reconciliation,
                    // even if a new directory already occupies the same path.
                    if matches!(
                        event.kind,
                        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
                    ) {
                        let removed = scope
                            .watched
                            .iter()
                            .filter(|watched| {
                                event.paths.iter().any(|path| watched.starts_with(path))
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        for path in removed {
                            let _ = watcher.unwatch(&path);
                            scope.watched.remove(&path);
                            rescan = true;
                        }
                    }
                    rescan |= changed;
                }
                Err(_) => {
                    dirty = Dirty::all();
                    rescan = true;
                    reconcile_review = true;
                    lease.control.lock().state.limited = true;
                }
            }
            if dirty.any() {
                if !was_dirty {
                    first_event = Instant::now();
                }
                last_event = Instant::now();
            }
        }
        if rescan && last_scan.elapsed() >= RESCAN_INTERVAL {
            let limited = scope.configure(&config, &mut watcher);
            lease.control.lock().state.limited = limited;
            // Reconcile the short registration window exactly once.
            dirty.tree = true;
            dirty.content = true;
            dirty.worktree |= reconcile_review;
            dirty.git |= reconcile_review;
            reconcile_review = false;
            rescan = false;
            last_scan = Instant::now();
        }
        if dirty.any()
            && (last_event.elapsed() >= DEBOUNCE || first_event.elapsed() >= MAX_BATCH_DELAY)
        {
            dirty.publish(&mut lease.control.lock().state);
        }
    }
    Ok(())
}

fn receive_batch(
    rx: &Receiver<notify::Result<Event>>,
    wait: Duration,
) -> Vec<notify::Result<Event>> {
    let mut events = Vec::new();
    if let Ok(event) = rx.recv_timeout(wait) {
        events.push(event);
    }
    events.extend(rx.try_iter().take(255));
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("waku-watch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("src/deep")).unwrap();
            std::fs::create_dir_all(root.join("target/cache")).unwrap();
            std::fs::create_dir_all(root.join(".git/objects")).unwrap();
            std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn scope(&self) -> Scope {
            Scope {
                root: self.0.clone(),
                git_dirs: vec![self.0.join(".git")],
                watched: HashSet::new(),
                ignores: HashMap::new(),
                tracked: HashSet::new(),
            }
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn file_tree_only_watches_visible_directories_and_review_excludes_build_output() {
        let temp = Temp::new();
        let mut scope = temp.scope();
        let mut watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
        let mut config = Config {
            directories: vec![PathBuf::from("src")],
            ..Config::default()
        };
        assert!(!scope.configure(&config, &mut watcher));
        assert_eq!(scope.watched.len(), 3);
        assert!(!scope.watched.contains(&temp.0.join("src/deep")));
        config.recursive = true;
        scope.configure(&config, &mut watcher);
        assert!(scope.watched.contains(&temp.0.join("src/deep")));
        assert!(!scope.watched.contains(&temp.0.join("target")));
        assert!(!scope.watched.contains(&temp.0.join(".git/objects")));
        // Explicitly expanding ignored output watches that directory only.
        config.directories.push(PathBuf::from("target"));
        scope.configure(&config, &mut watcher);
        assert!(scope.watched.contains(&temp.0.join("target")));
        assert!(!scope.watched.contains(&temp.0.join("target/cache")));
    }

    #[test]
    fn git_snapshot_noise_and_ignored_writes_do_not_refresh_review() {
        let temp = Temp::new();
        let mut scope = temp.scope();
        let mut watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
        scope.configure(&Config::default(), &mut watcher);
        let changed = |path: &str| {
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                .add_path(temp.0.join(path))
        };
        for path in [
            ".git",
            ".git/index.lock",
            ".git/objects/abc",
            ".git/waku-checkpoint-index-123",
            ".git/refs/waku/turn",
        ] {
            let mut dirty = Dirty::default();
            assert!(!scope.event(changed(path), &mut dirty));
            assert!(!dirty.any(), "unexpected invalidation for {path}");
        }
        let mut dirty = Dirty::default();
        scope.event(changed("debug.log"), &mut dirty);
        assert!(dirty.content);
        assert!(!dirty.worktree);
        scope.tracked.insert(temp.0.join("debug.log"));
        scope.event(changed("debug.log"), &mut dirty);
        assert!(dirty.worktree);
        let mut dirty = Dirty::default();
        scope.event(changed(".git/HEAD"), &mut dirty);
        assert!(dirty.git);
        assert!(!dirty.worktree);
    }

    #[test]
    fn new_directory_requests_one_reconciliation_and_batches_are_bounded() {
        let temp = Temp::new();
        let scope = temp.scope();
        let mut dirty = Dirty::default();
        assert!(scope.event(
            Event::new(EventKind::Create(CreateKind::Folder)).add_path(temp.0.join("src")),
            &mut dirty
        ));
        assert!(dirty.tree && dirty.worktree);
        let (tx, rx) = bounded(1024);
        for _ in 0..1000 {
            tx.send(Ok(Event::new(EventKind::Any))).unwrap();
        }
        assert_eq!(receive_batch(&rx, Duration::ZERO).len(), 256);
        assert_eq!(rx.len(), 744);
    }

    #[test]
    fn multiple_clients_share_the_union_of_their_interests() {
        let clients = HashMap::from([
            (
                uuid::Uuid::new_v4(),
                (
                    Config {
                        directories: vec!["src".into()],
                        ..Config::default()
                    },
                    Instant::now(),
                ),
            ),
            (
                uuid::Uuid::new_v4(),
                (
                    Config {
                        directories: vec!["src".into(), "target".into()],
                        recursive: true,
                        watch_git: true,
                    },
                    Instant::now(),
                ),
            ),
        ]);
        let config = merged_config(&clients);
        assert!(config.recursive && config.watch_git);
        assert_eq!(
            config.directories,
            vec![PathBuf::from("src"), PathBuf::from("target")]
        );
    }

    fn wait_for(lease: &Lease, predicate: impl Fn(&WorkspaceWatchState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if predicate(&lease.control.lock().state) {
                return;
            }
            assert!(Instant::now() < deadline, "native watch did not converge");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn native_watch_detects_edits_atomic_saves_and_deletes_then_releases_its_lease() {
        let temp = Temp::new();
        let file = temp.0.join("src/file.txt");
        std::fs::write(&file, "before").unwrap();
        let config = Config {
            directories: vec![PathBuf::from("src")],
            ..Config::default()
        };
        let lease = Arc::new(Lease {
            control: Mutex::new(Control {
                clients: HashMap::from([(uuid::Uuid::new_v4(), (config.clone(), Instant::now()))]),
                config,
                last_poll: Instant::now(),
                state: WorkspaceWatchState {
                    epoch: uuid::Uuid::new_v4(),
                    tree: 0,
                    content: 0,
                    worktree: 0,
                    git: 0,
                    ready: false,
                    limited: false,
                },
            }),
            stop: AtomicBool::new(false),
        });
        let worker_lease = lease.clone();
        let root = temp.0.clone();
        let worker = std::thread::spawn(move || run(&root, &worker_lease).unwrap());
        wait_for(&lease, |state| state.ready);
        std::fs::write(&file, "edited").unwrap();
        wait_for(&lease, |state| state.content > 0 && state.worktree > 0);
        let before = lease.control.lock().state.clone();
        let replacement = temp.0.join("src/new.txt");
        std::fs::write(&replacement, "atomic").unwrap();
        std::fs::remove_file(&file).unwrap();
        std::fs::rename(&replacement, &file).unwrap();
        wait_for(&lease, |state| state.tree > before.tree);
        let before = lease.control.lock().state.clone();
        std::fs::remove_file(&file).unwrap();
        wait_for(&lease, |state| state.tree > before.tree);
        // No filesystem activity means no new invalidation, even while a
        // visible client continues asking for revisions.
        let before = lease.control.lock().state.clone();
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(lease.control.lock().state, before);

        for index in 0..500 {
            std::fs::write(
                temp.0
                    .join("target/cache")
                    .join(format!("artifact-{index}")),
                "build",
            )
            .unwrap();
        }
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            lease.control.lock().state,
            before,
            "ignored build burst invalidated panels"
        );

        // A recreated directory needs a new OS handle, even though its path
        // is unchanged. Allow the bounded reconciliation pass to register it.
        std::fs::remove_dir_all(temp.0.join("src")).unwrap();
        std::fs::create_dir(temp.0.join("src")).unwrap();
        std::thread::sleep(RESCAN_INTERVAL + Duration::from_secs(1));
        let before = lease.control.lock().state.clone();
        std::fs::write(&file, "after recreation").unwrap();
        wait_for(&lease, |state| state.content > before.content);
        lease.control.lock().last_poll = Instant::now() - LEASE;
        worker.join().unwrap();
    }
}

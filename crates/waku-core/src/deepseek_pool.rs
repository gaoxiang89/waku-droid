//! Daemon-wide pool of resident `dsh web` processes.
//!
//! A Harness Host owns many sessions and every session supplies its own cwd,
//! so all Waku tasks using the same executable share one process. Starts and
//! final teardown are serialized so a new task never races a dying Host over
//! Harness's persistent session store.

use std::collections::HashMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use crate::deepseek_session::DeepSeekServer;

const SERVER_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

trait PooledHost {
    fn is_alive(&self) -> bool;
    fn shutdown(&self, timeout: Duration);
}

impl PooledHost for DeepSeekServer {
    fn is_alive(&self) -> bool {
        DeepSeekServer::is_alive(self)
    }

    fn shutdown(&self, timeout: Duration) {
        DeepSeekServer::shutdown(self, timeout);
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_unpoisoned<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Normalize a binary path so symlink and Windows casing/verbatim-prefix
/// variants resolve to the same pool slot.
fn canonical_pool_key(binary: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(binary).unwrap_or_else(|_| binary.to_path_buf());
    normalize_pool_key(&resolved)
}

fn normalize_pool_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        normalize_windows_pool_key(&path.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

#[cfg(any(windows, test))]
fn normalize_windows_pool_key(path: &str) -> PathBuf {
    let stripped = crate::opencode_api::strip_windows_verbatim_prefix(path);
    PathBuf::from(stripped.replace('/', "\\").to_lowercase())
}

pub(crate) struct PooledDeepSeekServer(PooledHandle<DeepSeekServer>);

impl Clone for PooledDeepSeekServer {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Deref for PooledDeepSeekServer {
    type Target = DeepSeekServer;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct PooledHandle<H: PooledHost> {
    inner: Arc<PoolInner<H>>,
}

impl<H: PooledHost> Clone for PooledHandle<H> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct PoolInner<H: PooledHost> {
    server: H,
    slot: Weak<PoolSlot<H>>,
}

struct StoppingGuard<'a, H: PooledHost> {
    slot: &'a PoolSlot<H>,
}

impl<'a, H: PooledHost> StoppingGuard<'a, H> {
    fn new(slot: &'a PoolSlot<H>) -> Self {
        Self { slot }
    }
}

impl<H: PooledHost> Drop for StoppingGuard<'_, H> {
    fn drop(&mut self) {
        let mut state = lock_unpoisoned(&self.slot.state);
        *state = PoolState::Vacant;
        self.slot.changed.notify_all();
    }
}

struct StartingGuard<'a, H: PooledHost> {
    slot: &'a PoolSlot<H>,
    armed: bool,
}

impl<'a, H: PooledHost> StartingGuard<'a, H> {
    fn new(slot: &'a PoolSlot<H>) -> Self {
        Self { slot, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<H: PooledHost> Drop for StartingGuard<'_, H> {
    fn drop(&mut self) {
        if self.armed {
            let mut state = lock_unpoisoned(&self.slot.state);
            *state = PoolState::Vacant;
            self.slot.changed.notify_all();
        }
    }
}

impl<H: PooledHost> Drop for PoolInner<H> {
    fn drop(&mut self) {
        let stopping_slot = self.slot.upgrade().and_then(|slot| {
            let current = {
                let mut state = lock_unpoisoned(&slot.state);
                let current = match &*state {
                    PoolState::Running(server) => std::ptr::eq(server.as_ptr(), self),
                    PoolState::Vacant | PoolState::Starting | PoolState::Stopping => false,
                };
                if current {
                    *state = PoolState::Stopping;
                }
                current
            };
            current.then_some(slot)
        });
        // Release `slot.state` before blocking on process shutdown so waiters
        // observe `PoolState::Stopping` without stalling on the mutex, and use
        // an RAII guard so even a panic during shutdown returns to `Vacant`.
        let _guard = stopping_slot.as_deref().map(StoppingGuard::new);
        self.server.shutdown(SERVER_EXIT_TIMEOUT);
    }
}

impl<H: PooledHost> Deref for PooledHandle<H> {
    type Target = H;

    fn deref(&self) -> &Self::Target {
        &self.inner.server
    }
}

enum PoolState<H: PooledHost> {
    Vacant,
    Starting,
    Running(Weak<PoolInner<H>>),
    Stopping,
}

struct PoolSlot<H: PooledHost> {
    state: Mutex<PoolState<H>>,
    changed: Condvar,
}

impl<H: PooledHost> Default for PoolSlot<H> {
    fn default() -> Self {
        Self {
            state: Mutex::new(PoolState::Vacant),
            changed: Condvar::new(),
        }
    }
}

struct HostPool<H: PooledHost> {
    slots: Mutex<HashMap<PathBuf, Arc<PoolSlot<H>>>>,
}

impl<H: PooledHost> Default for HostPool<H> {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

impl<H: PooledHost> HostPool<H> {
    fn slot_for(&self, binary: &Path) -> Arc<PoolSlot<H>> {
        let key = canonical_pool_key(binary);
        let mut slots = lock_unpoisoned(&self.slots);
        Arc::clone(slots.entry(key).or_default())
    }

    fn acquire_with_start(
        &self,
        binary: &Path,
        start: impl FnOnce() -> anyhow::Result<H>,
    ) -> anyhow::Result<PooledHandle<H>> {
        let slot = self.slot_for(binary);

        let superseded = loop {
            let mut state = lock_unpoisoned(&slot.state);
            if let PoolState::Running(server) = &*state {
                if let Some(inner) = server.upgrade() {
                    if inner.server.is_alive() {
                        return Ok(PooledHandle { inner });
                    }
                    *state = PoolState::Starting;
                    break Some(inner);
                }
                state = wait_unpoisoned(&slot.changed, state);
                drop(state);
                continue;
            }

            match &*state {
                PoolState::Vacant => {
                    *state = PoolState::Starting;
                    break None;
                }
                PoolState::Starting | PoolState::Stopping => {
                    state = wait_unpoisoned(&slot.changed, state);
                    drop(state);
                }
                PoolState::Running(_) => unreachable!(),
            }
        };
        let mut starting = StartingGuard::new(&slot);
        drop(superseded);

        let started = start();
        let mut state = lock_unpoisoned(&slot.state);
        match started {
            Ok(server) => {
                let inner = Arc::new(PoolInner {
                    server,
                    slot: Arc::downgrade(&slot),
                });
                *state = PoolState::Running(Arc::downgrade(&inner));
                starting.disarm();
                slot.changed.notify_all();
                Ok(PooledHandle { inner })
            }
            Err(error) => {
                *state = PoolState::Vacant;
                starting.disarm();
                slot.changed.notify_all();
                Err(error)
            }
        }
    }
}

fn pool() -> &'static HostPool<DeepSeekServer> {
    static POOL: OnceLock<HostPool<DeepSeekServer>> = OnceLock::new();
    POOL.get_or_init(HostPool::default)
}

/// Returns the resident Harness Host for `binary`, starting it when needed.
/// Blocking by design; driver construction already runs off the UI thread.
pub(crate) fn acquire(binary: &Path) -> anyhow::Result<PooledDeepSeekServer> {
    acquire_with_start(binary, || DeepSeekServer::start(binary))
}

fn acquire_with_start(
    binary: &Path,
    start: impl FnOnce() -> anyhow::Result<DeepSeekServer>,
) -> anyhow::Result<PooledDeepSeekServer> {
    pool()
        .acquire_with_start(binary, start)
        .map(PooledDeepSeekServer)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use anyhow::anyhow;
    use uuid::Uuid;

    use super::*;

    struct FakeHost {
        id: usize,
        alive: Arc<AtomicBool>,
        shutdowns: Arc<AtomicUsize>,
        on_shutdown: Option<Mutex<Box<dyn FnMut() + Send>>>,
    }

    impl FakeHost {
        fn new(id: usize) -> (Self, Arc<AtomicBool>, Arc<AtomicUsize>) {
            let alive = Arc::new(AtomicBool::new(true));
            let shutdowns = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    id,
                    alive: Arc::clone(&alive),
                    shutdowns: Arc::clone(&shutdowns),
                    on_shutdown: None,
                },
                alive,
                shutdowns,
            )
        }

        fn with_shutdown_hook(
            id: usize,
            hook: impl FnMut() + Send + 'static,
        ) -> (Self, Arc<AtomicBool>, Arc<AtomicUsize>) {
            let (mut host, alive, shutdowns) = Self::new(id);
            host.on_shutdown = Some(Mutex::new(Box::new(hook)));
            (host, alive, shutdowns)
        }
    }

    impl PooledHost for FakeHost {
        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::Acquire)
        }

        fn shutdown(&self, _timeout: Duration) {
            self.shutdowns.fetch_add(1, Ordering::AcqRel);
            if let Some(hook) = self.on_shutdown.as_ref() {
                (lock_unpoisoned(hook))();
            }
            self.alive.store(false, Ordering::Release);
        }
    }

    #[test]
    fn reuses_live_host_and_shuts_down_after_last_lease_drops() {
        let pool = HostPool::<FakeHost>::default();
        let binary = Path::new("/fake/dsh");
        let starts = AtomicUsize::new(0);
        let (host, _alive, shutdowns) = FakeHost::new(1);
        let mut host = Some(host);

        let first = pool
            .acquire_with_start(binary, || {
                starts.fetch_add(1, Ordering::SeqCst);
                Ok(host.take().unwrap())
            })
            .unwrap();
        let second = pool
            .acquire_with_start(binary, || {
                starts.fetch_add(1, Ordering::SeqCst);
                unreachable!("live host should be reused")
            })
            .unwrap();

        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(first.id, 1);
        assert_eq!(second.id, 1);

        drop(first);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
        drop(second);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dead_host_is_replaced_and_old_drop_does_not_clear_new_slot() {
        let pool = HostPool::<FakeHost>::default();
        let binary = Path::new("/fake/dsh-dead");
        let (host1, alive1, shutdowns1) = FakeHost::new(1);
        let (host2, _alive2, shutdowns2) = FakeHost::new(2);

        let first = pool.acquire_with_start(binary, || Ok(host1)).unwrap();
        alive1.store(false, Ordering::Release);

        let second = pool.acquire_with_start(binary, || Ok(host2)).unwrap();
        assert_eq!(second.id, 2);

        // Dropping the superseded handle shuts down host1 without disturbing host2.
        drop(first);
        assert_eq!(shutdowns1.load(Ordering::SeqCst), 1);
        assert_eq!(shutdowns2.load(Ordering::SeqCst), 0);

        let third = pool
            .acquire_with_start(binary, || unreachable!("host2 should still be live"))
            .unwrap();
        assert_eq!(third.id, 2);

        drop(second);
        drop(third);
        assert_eq!(shutdowns2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_waiters_wake_and_share_the_started_host() {
        let pool = Arc::new(HostPool::<FakeHost>::default());
        let binary = PathBuf::from("/fake/dsh-concurrent");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let starts = Arc::new(AtomicUsize::new(0));

        let starter_pool = Arc::clone(&pool);
        let starter_bin = binary.clone();
        let starter_starts = Arc::clone(&starts);
        let starter = thread::spawn(move || {
            starter_pool
                .acquire_with_start(&starter_bin, || {
                    starter_starts.fetch_add(1, Ordering::SeqCst);
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(FakeHost::new(7).0)
                })
                .unwrap()
        });

        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let waiter_pool = Arc::clone(&pool);
        let waiter_bin = binary.clone();
        let waiter_starts = Arc::clone(&starts);
        let waiter = thread::spawn(move || {
            waiter_pool
                .acquire_with_start(&waiter_bin, || {
                    waiter_starts.fetch_add(1, Ordering::SeqCst);
                    Ok(FakeHost::new(8).0)
                })
                .unwrap()
        });

        // Give the waiter a moment to park on the condvar in `Starting`.
        thread::sleep(Duration::from_millis(50));
        release_tx.send(()).unwrap();

        let h1 = starter.join().unwrap();
        let h2 = waiter.join().unwrap();
        assert_eq!(h1.id, 7);
        assert_eq!(h2.id, 7);
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_start_returns_slot_to_vacant_and_wakes_waiter() {
        let pool = Arc::new(HostPool::<FakeHost>::default());
        let binary = PathBuf::from("/fake/dsh-fail");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let starter_pool = Arc::clone(&pool);
        let starter_bin = binary.clone();
        let starter = thread::spawn(move || {
            starter_pool.acquire_with_start(&starter_bin, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Err(anyhow!("boom"))
            })
        });

        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let waiter_pool = Arc::clone(&pool);
        let waiter_bin = binary.clone();
        let waiter = thread::spawn(move || {
            waiter_pool
                .acquire_with_start(&waiter_bin, || Ok(FakeHost::new(9).0))
                .unwrap()
        });

        thread::sleep(Duration::from_millis(50));
        release_tx.send(()).unwrap();

        assert!(starter.join().unwrap().is_err());
        let recovered = waiter.join().unwrap();
        assert_eq!(recovered.id, 9);
    }

    #[test]
    fn panic_during_start_resets_starting_state_and_wakes_waiter() {
        let pool = Arc::new(HostPool::<FakeHost>::default());
        let binary = PathBuf::from("/fake/dsh-panic");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let starter_pool = Arc::clone(&pool);
        let starter_bin = binary.clone();
        let starter = thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = starter_pool.acquire_with_start(&starter_bin, || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    panic!("simulated startup panic");
                });
            }))
        });

        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let waiter_pool = Arc::clone(&pool);
        let waiter_bin = binary.clone();
        let waiter = thread::spawn(move || {
            waiter_pool
                .acquire_with_start(&waiter_bin, || Ok(FakeHost::new(10).0))
                .unwrap()
        });

        thread::sleep(Duration::from_millis(50));
        release_tx.send(()).unwrap();

        assert!(starter.join().unwrap().is_err());
        let recovered = waiter.join().unwrap();
        assert_eq!(recovered.id, 10);
    }

    #[test]
    fn shutdown_releases_slot_mutex_while_stopping_and_wakes_waiter() {
        let pool = Arc::new(HostPool::<FakeHost>::default());
        let binary = PathBuf::from("/fake/dsh-stopping");
        let (shutdown_entered_tx, shutdown_entered_rx) = mpsc::channel();
        let (shutdown_release_tx, shutdown_release_rx) = mpsc::channel();

        let (host1, _alive1, _shutdowns1) = FakeHost::with_shutdown_hook(1, move || {
            shutdown_entered_tx.send(()).unwrap();
            shutdown_release_rx.recv().unwrap();
        });
        let first = pool.acquire_with_start(&binary, || Ok(host1)).unwrap();
        let slot = pool.slot_for(&binary);

        let dropper = thread::spawn(move || drop(first));
        shutdown_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();

        // Locking `slot.state` must succeed immediately during `shutdown()`
        // and observe `PoolState::Stopping` rather than blocking on the mutex.
        {
            let guard = slot
                .state
                .try_lock()
                .expect("slot.state must be unlocked during shutdown()");
            assert!(matches!(&*guard, PoolState::Stopping));
        }

        let waiter_pool = Arc::clone(&pool);
        let waiter_bin = binary.clone();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let handle = waiter_pool
                .acquire_with_start(&waiter_bin, || Ok(FakeHost::new(2).0))
                .unwrap();
            acquired_tx.send(handle.id).unwrap();
            handle
        });

        thread::sleep(Duration::from_millis(50));
        assert!(acquired_rx.try_recv().is_err());

        shutdown_release_tx.send(()).unwrap();
        dropper.join().unwrap();
        assert_eq!(
            acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            2
        );
        let _ = waiter.join().unwrap();
    }

    #[test]
    fn handles_poisoned_slot_and_pool_mutexes_without_panicking() {
        let pool = Arc::new(HostPool::<FakeHost>::default());
        let binary = PathBuf::from("/fake/dsh-poison");
        let slot = pool.slot_for(&binary);

        let poison_slot = Arc::clone(&slot);
        let _ = thread::spawn(move || {
            let _guard = poison_slot.state.lock().unwrap();
            panic!("poison slot");
        })
        .join();
        assert!(slot.state.is_poisoned());

        let poison_pool = Arc::clone(&pool);
        let _ = thread::spawn(move || {
            let _guard = poison_pool.slots.lock().unwrap();
            panic!("poison pool");
        })
        .join();
        assert!(pool.slots.is_poisoned());

        let handle = pool
            .acquire_with_start(&binary, || Ok(FakeHost::new(42).0))
            .unwrap();
        assert_eq!(handle.id, 42);
        drop(handle);
    }

    #[test]
    fn canonical_keys_deduplicate_path_and_windows_casing_variants() {
        assert_eq!(
            normalize_windows_pool_key(r"\\?\C:\Users\Alice\dsh.CMD"),
            normalize_windows_pool_key(r"c:/users/alice/dsh.cmd")
        );
        assert_eq!(
            normalize_windows_pool_key(r"\\?\UNC\server\share\Bin\DSH.exe"),
            normalize_windows_pool_key(r"\\SERVER\share\bin\dsh.exe")
        );

        let root = std::env::temp_dir().join(format!("waku-pool-key-{}", Uuid::new_v4()));
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let file = root.join("dsh-fake");
        std::fs::write(&file, b"stub").unwrap();
        let dotdot = sub.join("..").join("dsh-fake");

        let pool = HostPool::<FakeHost>::default();
        let first = pool
            .acquire_with_start(&file, || Ok(FakeHost::new(11).0))
            .unwrap();
        let second = pool
            .acquire_with_start(&dotdot, || unreachable!("canonical alias should share slot"))
            .unwrap();
        assert_eq!(first.id, second.id);

        #[cfg(windows)]
        {
            let upper = PathBuf::from(file.to_string_lossy().to_uppercase());
            let third = pool
                .acquire_with_start(&upper, || unreachable!("Windows casing variant should share slot"))
                .unwrap();
            assert_eq!(first.id, third.id);
        }

        let link = root.join("dsh-link");
        if crate::fs_ext::symlink(&file, &link).is_ok() {
            let linked = pool
                .acquire_with_start(&link, || unreachable!("symlink should share slot"))
                .unwrap();
            assert_eq!(first.id, linked.id);
        }

        drop(second);
        drop(first);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires an installed DeepSeek Harness"]
    fn sessions_using_one_binary_share_the_resident_host() {
        let binary =
            crate::command_env::find_executable("dsh").expect("DeepSeek Harness is not installed");
        let first = acquire(&binary).expect("the first Harness lease should start");
        let second = acquire(&binary).expect("the second Harness lease should attach");

        assert_eq!(first.port, second.port);
        drop(first);
        assert!(second.is_alive());
    }
}

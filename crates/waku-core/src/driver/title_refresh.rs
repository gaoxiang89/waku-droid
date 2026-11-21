//! Provider title refresh helpers.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::driver::DriverEventSender;
use crate::model::DriverEvent;

const IDLE: u8 = 0;
const RUNNING: u8 = 1;
const RESOLVED: u8 = 2;

#[derive(Default)]
struct NativeTitleRefreshInner {
    state: Arc<AtomicU8>,
    cancel: Arc<(Mutex<bool>, Condvar)>,
}

impl Drop for NativeTitleRefreshInner {
    fn drop(&mut self) {
        let (lock, condvar) = &*self.cancel;
        let mut cancelled = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *cancelled = true;
        condvar.notify_all();
    }
}

/// Runs a provider metadata lookup away from the UI and provider reader
/// threads. A missed lookup becomes eligible again on the next completed turn;
/// a resolved title is final for this live driver. Dropping the last handle
/// cooperatively cancels any sleeping or in-flight lookup immediately.
#[derive(Clone, Default)]
pub(super) struct NativeTitleRefresh {
    inner: Arc<NativeTitleRefreshInner>,
}

fn is_cancelled(cancel: &(Mutex<bool>, Condvar)) -> bool {
    *cancel
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_for_delay_or_cancel(cancel: &(Mutex<bool>, Condvar), delay: Duration) -> bool {
    let (lock, condvar) = cancel;
    let cancelled = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *cancelled {
        return true;
    }
    if delay.is_zero() {
        return false;
    }
    let (cancelled, _) = condvar
        .wait_timeout_while(cancelled, delay, |cancelled| !*cancelled)
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *cancelled
}

impl NativeTitleRefresh {
    pub(super) fn start(
        &self,
        thread_name: &'static str,
        delays: Vec<Duration>,
        events: DriverEventSender,
        lookup: impl Fn() -> anyhow::Result<Option<String>> + Send + 'static,
    ) {
        if self
            .inner
            .state
            .compare_exchange(IDLE, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let state = self.inner.state.clone();
        let cancel = self.inner.cancel.clone();
        let result = thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                for delay in delays {
                    if wait_for_delay_or_cancel(&cancel, delay) {
                        state.store(IDLE, Ordering::Release);
                        return;
                    }
                    let title = lookup()
                        .ok()
                        .flatten()
                        .map(|title| title.trim().to_owned())
                        .filter(|title| !title.is_empty());
                    if is_cancelled(&cancel) {
                        state.store(IDLE, Ordering::Release);
                        return;
                    }
                    if let Some(title) = title {
                        let _ = events.send(DriverEvent::AutoTitleUpdated(Some(title)));
                        state.store(RESOLVED, Ordering::Release);
                        return;
                    }
                }
                state.store(IDLE, Ordering::Release);
            });

        if result.is_err() {
            self.inner.state.store(IDLE, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DriverEvent;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Instant;

    /// Blank and failed lookups are misses, not answers, so the schedule keeps
    /// going and the driver is left free to try again on its next turn.
    #[test]
    fn a_schedule_walks_past_misses_and_stops_on_the_first_real_title() {
        let (events, event_rx) = crate::driver::test_event_channel();
        let refresh = NativeTitleRefresh::default();
        let attempts = Arc::new(AtomicUsize::new(0));
        let lookups = attempts.clone();

        refresh.start(
            "waku-title-test",
            vec![Duration::ZERO; 4],
            events,
            move || match lookups.fetch_add(1, Ordering::AcqRel) {
                0 => Err(anyhow::anyhow!("the native store is not written yet")),
                1 => Ok(None),
                2 => Ok(Some("   ".into())),
                _ => Ok(Some("  Fix the title poll  ".into())),
            },
        );

        let event = event_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a resolved title must reach the driver's events");
        assert!(matches!(
            event,
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Fix the title poll"
        ));
        assert_eq!(attempts.load(Ordering::Acquire), 4);

        // Resolved is final: a later turn must not re-announce the same title.
        refresh.start(
            "waku-title-test",
            vec![Duration::ZERO],
            {
                let (events, _) = crate::driver::test_event_channel();
                events
            },
            || Ok(Some("Should never run".into())),
        );
        assert_eq!(attempts.load(Ordering::Acquire), 4);
    }

    /// A session whose provider never wrote a title — Claude skips prompts
    /// under ten characters entirely — must leave the refresh re-armable
    /// rather than latching, so a later prompt still gets a look.
    #[test]
    fn a_schedule_that_runs_dry_can_be_started_again() {
        let (events, event_rx) = crate::driver::test_event_channel();
        let refresh = NativeTitleRefresh::default();
        let attempts = Arc::new(AtomicUsize::new(0));

        for _ in 0..2 {
            let lookups = attempts.clone();
            refresh.start(
                "waku-title-test",
                vec![Duration::ZERO, Duration::ZERO],
                events.clone(),
                move || {
                    lookups.fetch_add(1, Ordering::AcqRel);
                    Ok(None)
                },
            );
            // The dry run returns the state to IDLE only after its last
            // lookup, so wait on the state itself — an attempt count can be
            // final while the thread is still RUNNING, and the next start
            // would silently no-op.
            for _ in 0..400 {
                if refresh.inner.state.load(Ordering::Acquire) == IDLE {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(refresh.inner.state.load(Ordering::Acquire), IDLE);
        }

        assert_eq!(attempts.load(Ordering::Acquire), 4);
        assert!(
            event_rx.try_recv().is_err(),
            "a dry schedule must not announce a title"
        );
    }

    #[test]
    fn dropping_refresh_interrupts_sleeping_worker_immediately() {
        let (events, event_rx) = crate::driver::test_event_channel();
        let refresh = NativeTitleRefresh::default();
        let state = refresh.inner.state.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let lookups = attempts.clone();

        refresh.start(
            "waku-title-cancel-test",
            vec![Duration::from_secs(60)],
            events,
            move || {
                lookups.fetch_add(1, Ordering::AcqRel);
                Ok(Some("Should never run".into()))
            },
        );
        assert_eq!(state.load(Ordering::Acquire), RUNNING);

        let started = Instant::now();
        drop(refresh);

        for _ in 0..200 {
            if state.load(Ordering::Acquire) == IDLE {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(state.load(Ordering::Acquire), IDLE);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "sleep must be interrupted immediately on drop"
        );
        assert_eq!(attempts.load(Ordering::Acquire), 0);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn cloned_handle_keeps_schedule_alive_until_last_handle_drops() {
        let (events, event_rx) = crate::driver::test_event_channel();
        let primary = NativeTitleRefresh::default();
        let transient = primary.clone();

        transient.start(
            "waku-title-clone-test",
            vec![Duration::from_millis(30)],
            events,
            || Ok(Some("Survived transient clone drop".into())),
        );
        // Dropping a per-prompt clone must not cancel the driver's schedule.
        drop(transient);

        let event = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("primary handle must keep the title refresh alive");
        assert!(matches!(
            event,
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Survived transient clone drop"
        ));
        drop(primary);
    }

    #[test]
    fn dropping_during_in_flight_lookup_suppresses_late_title_event() {
        let (events, event_rx) = crate::driver::test_event_channel();
        let refresh = NativeTitleRefresh::default();
        let state = refresh.inner.state.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);

        refresh.start(
            "waku-title-inflight-test",
            vec![Duration::ZERO],
            events,
            move || {
                let _ = entered_tx.send(());
                let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5));
                Ok(Some("Stale title after drop".into()))
            },
        );

        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("lookup should start");
        drop(refresh);
        let _ = release_tx.send(());

        for _ in 0..200 {
            if state.load(Ordering::Acquire) == IDLE {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(state.load(Ordering::Acquire), IDLE);
        assert!(
            event_rx.try_recv().is_err(),
            "a lookup finishing after drop must not emit a title"
        );
    }
}

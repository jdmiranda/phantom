//! In-process supervisor with restart policies.
//!
//! Provides an Erlang/OTP-inspired in-process supervisor for tokio tasks.
//! Each child runs in its own `tokio::task` and is given a child
//! [`CancellationToken`] derived from the supervisor's root token. When a
//! child completes (cleanly or by panic), [`Supervisor::reap`] applies the
//! configured [`RestartPolicy`] to decide whether to respawn.
//!
//! # Concurrency notes
//!
//! - The factory closure is `Fn + Send + Sync + 'static` so it can be
//!   re-invoked for restarts. Capture an `Arc<...>` for shared state.
//! - Dropping a [`tokio::task::JoinHandle`] does *not* cancel the task; we
//!   always cancel via the [`CancellationToken`] before awaiting.
//! - [`Supervisor::reap`] is non-blocking; it polls JoinHandles via
//!   `is_finished` and only `await`s ones that have already completed.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Identifier for a supervised child task.
pub type ChildId = u64;

/// Restart policy applied when a supervised child exits.
#[derive(Debug, Clone, Copy)]
pub enum RestartPolicy {
    /// Always restart the child, regardless of exit reason.
    Permanent,
    /// Restart the child only if it panicked. Clean exits are not restarted.
    Transient,
    /// Never restart the child.
    Temporary,
    /// Restart, but cap at `max` restarts within the rolling `within` window.
    /// Exceeding the cap escalates: the child is dropped and an error logged.
    OneForOne {
        /// Maximum number of restarts allowed in the rolling window.
        max: u32,
        /// Rolling window duration for the rate limit.
        within: Duration,
    },
}

/// Factory closure that builds the future a child task will run. The
/// supervisor passes a [`CancellationToken`] the future should observe via
/// `tokio::select!` for graceful shutdown. Stored as a trait object so
/// [`Supervisor`] does not have to be generic over the closure type.
type Factory = Arc<
    dyn Fn(CancellationToken) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync
        + 'static,
>;

struct ChildHandle {
    policy: RestartPolicy,
    factory: Factory,
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
    restart_count: u32,
    restart_window: VecDeque<Instant>,
}

/// In-process supervisor managing a set of child tokio tasks under
/// configurable restart policies.
pub struct Supervisor {
    next_id: ChildId,
    children: HashMap<ChildId, ChildHandle>,
    root_token: CancellationToken,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// Create a new supervisor with a fresh root cancellation token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_id: 0,
            children: HashMap::new(),
            root_token: CancellationToken::new(),
        }
    }

    /// Spawn a child task under supervision.
    ///
    /// `factory` is invoked once at spawn time and again for each restart.
    /// The closure receives a [`CancellationToken`] derived from the
    /// supervisor's root token; it should `tokio::select!` on
    /// `token.cancelled()` for graceful shutdown.
    pub fn spawn<F, Fut>(&mut self, policy: RestartPolicy, factory: F) -> ChildId
    where
        F: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let factory: Factory = Arc::new(move |token| Box::pin(factory(token)));

        let id = self.next_id;
        self.next_id += 1;

        let cancel = self.root_token.child_token();
        let join = tokio::spawn((factory)(cancel.clone()));

        let handle = ChildHandle {
            policy,
            factory,
            cancel,
            join: Some(join),
            restart_count: 0,
            restart_window: VecDeque::new(),
        };
        self.children.insert(id, handle);
        id
    }

    /// Cancel a specific child by id. The task will exit at its next
    /// cancellation checkpoint; call [`Self::reap`] afterwards to apply
    /// the restart policy.
    pub fn cancel(&mut self, id: ChildId) {
        if let Some(child) = self.children.get(&id) {
            child.cancel.cancel();
        }
    }

    /// Cancel all children and await their completion.
    pub async fn shutdown(&mut self) {
        self.root_token.cancel();
        // Drain children, awaiting each JoinHandle. We drop them after
        // awaiting so respawn never happens during shutdown.
        let ids: Vec<ChildId> = self.children.keys().copied().collect();
        for id in ids {
            if let Some(mut child) = self.children.remove(&id)
                && let Some(join) = child.join.take()
            {
                let _ = join.await;
            }
        }
    }

    /// Number of currently-running children (those still holding a
    /// JoinHandle, regardless of whether the underlying task has finished).
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.children.len()
    }

    /// Reap finished children and apply restart policies.
    ///
    /// For each child whose JoinHandle reports `is_finished()`, we look up
    /// the policy:
    /// - [`RestartPolicy::Permanent`]: always respawn.
    /// - [`RestartPolicy::Transient`]: respawn only if the task panicked.
    /// - [`RestartPolicy::Temporary`]: drop the child.
    /// - [`RestartPolicy::OneForOne`]: respawn but enforce the rate limit;
    ///   on exceed, drop and log.
    ///
    /// This is non-blocking: it only polls `is_finished` and never awaits
    /// a still-running task. Call from a periodic tick or after a
    /// `tokio::select!` readiness branch.
    pub fn reap(&mut self) {
        let ids: Vec<ChildId> = self.children.keys().copied().collect();
        for id in ids {
            let Some(child) = self.children.get_mut(&id) else {
                continue;
            };
            let Some(join) = child.join.as_ref() else {
                continue;
            };
            if !join.is_finished() {
                continue;
            }

            // Take the JoinHandle and inspect it via try_join (non-blocking
            // since is_finished is true). `JoinHandle` is itself a Future,
            // but once finished, polling it again completes immediately.
            let join = child.join.take().expect("checked above");
            let panicked = matches!(
                join.now_or_never_result(),
                Some(Err(e)) if e.is_panic()
            );

            // If shutdown is in progress (root cancelled), do not respawn.
            if self.root_token.is_cancelled() {
                self.children.remove(&id);
                continue;
            }

            let policy = child.policy;
            let should_respawn = match policy {
                RestartPolicy::Permanent => true,
                RestartPolicy::Transient => panicked,
                RestartPolicy::Temporary => false,
                RestartPolicy::OneForOne { max, within } => {
                    let now = Instant::now();
                    // Drop entries outside the window.
                    while let Some(front) = child.restart_window.front() {
                        if now.duration_since(*front) > within {
                            child.restart_window.pop_front();
                        } else {
                            break;
                        }
                    }
                    if child.restart_window.len() as u32 >= max {
                        log::error!(
                            "supervisor: child {id} exceeded restart limit \
                             ({max} within {within:?}); escalating",
                        );
                        false
                    } else {
                        true
                    }
                }
            };

            if !should_respawn {
                self.children.remove(&id);
                continue;
            }

            // Respawn: fresh cancel token, re-invoke factory, store handle.
            let new_cancel = self.root_token.child_token();
            let new_join = tokio::spawn((child.factory)(new_cancel.clone()));
            child.cancel = new_cancel;
            child.join = Some(new_join);
            child.restart_count += 1;
            if matches!(policy, RestartPolicy::OneForOne { .. }) {
                child.restart_window.push_back(Instant::now());
            }
        }
    }
}

/// Tiny helper trait so we can probe a finished JoinHandle without awaiting
/// in a fully async context. We only call this after `is_finished()` is
/// true, so the future is guaranteed ready.
trait NowOrNeverResult {
    type Output;
    fn now_or_never_result(self) -> Option<Self::Output>;
}

impl<T: Send + 'static> NowOrNeverResult for JoinHandle<T> {
    type Output = Result<T, tokio::task::JoinError>;
    fn now_or_never_result(mut self) -> Option<Self::Output> {
        use std::future::Future;
        use std::task::{Context, Poll};

        // A no-op waker: we already know the future is ready.
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match std::pin::Pin::new(&mut self).poll(&mut cx) {
            Poll::Ready(r) => Some(r),
            Poll::Pending => None,
        }
    }
}

fn noop_waker() -> std::task::Waker {
    use std::task::{RawWaker, RawWakerVTable, Waker};
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    // SAFETY: the vtable functions are no-ops and ignore the data pointer.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration as TDuration, sleep};

    /// Yield enough times for spawned tasks to make progress / observe
    /// cancellation. We poll a condition with bounded retries instead of
    /// using long sleeps that slow the test suite.
    async fn wait_until<F: Fn() -> bool>(cond: F) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            sleep(TDuration::from_millis(5)).await;
        }
        panic!("wait_until timed out");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawned_task_runs_until_cancelled() {
        let started = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicUsize::new(0));
        let mut sup = Supervisor::new();

        let s = started.clone();
        let st = stopped.clone();
        let id = sup.spawn(RestartPolicy::Temporary, move |token| {
            let s = s.clone();
            let st = st.clone();
            async move {
                s.fetch_add(1, Ordering::SeqCst);
                token.cancelled().await;
                st.fetch_add(1, Ordering::SeqCst);
            }
        });

        wait_until(|| started.load(Ordering::SeqCst) == 1).await;
        assert_eq!(stopped.load(Ordering::SeqCst), 0);
        assert_eq!(sup.running_count(), 1);

        sup.cancel(id);
        wait_until(|| stopped.load(Ordering::SeqCst) == 1).await;

        // Reap the finished, Temporary child — it should not respawn.
        sup.reap();
        assert_eq!(sup.running_count(), 0);
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permanent_policy_respawns_after_clean_exit() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut sup = Supervisor::new();

        let s = starts.clone();
        let _id = sup.spawn(RestartPolicy::Permanent, move |_token| {
            let s = s.clone();
            async move {
                s.fetch_add(1, Ordering::SeqCst);
                // Clean exit immediately.
            }
        });

        // Wait for at least one run, then keep reaping to drive restarts.
        wait_until(|| starts.load(Ordering::SeqCst) >= 1).await;
        for _ in 0..20 {
            sup.reap();
            sleep(TDuration::from_millis(5)).await;
            if starts.load(Ordering::SeqCst) >= 3 {
                break;
            }
        }
        assert!(
            starts.load(Ordering::SeqCst) >= 3,
            "permanent should respawn; got {} starts",
            starts.load(Ordering::SeqCst)
        );

        sup.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn temporary_policy_does_not_respawn() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut sup = Supervisor::new();

        let s = starts.clone();
        sup.spawn(RestartPolicy::Temporary, move |_token| {
            let s = s.clone();
            async move {
                s.fetch_add(1, Ordering::SeqCst);
            }
        });

        wait_until(|| starts.load(Ordering::SeqCst) == 1).await;
        // Reap multiple times: count must stay at 1.
        for _ in 0..5 {
            sup.reap();
            sleep(TDuration::from_millis(5)).await;
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(sup.running_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_for_one_rate_limit_escalates() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut sup = Supervisor::new();

        let s = starts.clone();
        sup.spawn(
            RestartPolicy::OneForOne {
                max: 3,
                within: Duration::from_secs(60),
            },
            move |_token| {
                let s = s.clone();
                async move {
                    s.fetch_add(1, Ordering::SeqCst);
                    // Clean exit; will trigger respawn under OneForOne up to max.
                }
            },
        );

        // Drive restarts. Initial run + 3 restarts == 4 total starts, then escalation.
        for _ in 0..50 {
            sup.reap();
            sleep(TDuration::from_millis(5)).await;
            if sup.running_count() == 0 && starts.load(Ordering::SeqCst) >= 4 {
                break;
            }
        }

        assert_eq!(
            starts.load(Ordering::SeqCst),
            4,
            "expected initial + 3 restarts before escalation",
        );
        assert_eq!(sup.running_count(), 0, "child should be dropped on escalation");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_cancels_all_children() {
        let stopped = Arc::new(AtomicUsize::new(0));
        let mut sup = Supervisor::new();

        for _ in 0..3 {
            let st = stopped.clone();
            sup.spawn(RestartPolicy::Permanent, move |token| {
                let st = st.clone();
                async move {
                    token.cancelled().await;
                    st.fetch_add(1, Ordering::SeqCst);
                }
            });
        }

        assert_eq!(sup.running_count(), 3);
        sup.shutdown().await;

        assert_eq!(stopped.load(Ordering::SeqCst), 3);
        assert_eq!(sup.running_count(), 0);
    }
}

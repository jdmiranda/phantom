use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

/// Unique job identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(pub u64);

/// Job priority levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobPriority {
    Background = 0,
    Low = 1,
    Normal = 2,
    High = 3,
}

/// The work a job performs.
pub trait JobPayload: Send {
    fn run(&mut self, ctx: &JobContext) -> JobResult;
    fn describe(&self) -> &str;
}

/// Context passed to running jobs.
pub struct JobContext {
    pub job_id: JobId,
    pub cancelled: Arc<AtomicBool>,
}

impl JobContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// Result of a completed job.
pub enum JobResult {
    Done(String),
    Err(String),
    Cancelled,
}

/// Handle returned when submitting a job. Allows cancellation.
pub struct JobHandle {
    pub id: JobId,
    cancel: Arc<AtomicBool>,
}

impl JobHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Status of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// The worker pool.
pub struct JobPool {
    next_id: AtomicU64,
    sender: mpsc::Sender<JobEnvelope>,
    results: Mutex<Vec<(JobId, JobResult)>>,
    result_rx: Mutex<mpsc::Receiver<(JobId, JobResult)>>,
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

struct JobEnvelope {
    id: JobId,
    #[allow(dead_code)]
    priority: JobPriority,
    payload: Box<dyn JobPayload>,
    cancel: Arc<AtomicBool>,
}

impl JobPool {
    /// Create and start a pool with `worker_count` threads.
    pub fn start_up(worker_count: usize) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<JobEnvelope>();
        let (result_tx, result_rx) = mpsc::channel::<(JobId, JobResult)>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::with_capacity(worker_count);
        for i in 0..worker_count {
            let rx = Arc::clone(&job_rx);
            let tx = result_tx.clone();
            let shut = Arc::clone(&shutdown);

            let handle = thread::Builder::new()
                .name(format!("phantom-worker-{i}"))
                .spawn(move || {
                    worker_loop(rx, tx, shut);
                })
                .expect("failed to spawn worker thread");
            workers.push(handle);
        }

        Self {
            next_id: AtomicU64::new(1),
            sender: job_tx,
            results: Mutex::new(Vec::new()),
            result_rx: Mutex::new(result_rx),
            workers,
            shutdown,
        }
    }

    /// Submit a job to the pool. Returns a handle for cancellation.
    pub fn submit(&self, priority: JobPriority, payload: Box<dyn JobPayload>) -> JobHandle {
        let id = JobId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = JobHandle {
            id,
            cancel: Arc::clone(&cancel),
        };

        let envelope = JobEnvelope {
            id,
            priority,
            payload,
            cancel,
        };
        let _ = self.sender.send(envelope);
        handle
    }

    /// Drain completed results from worker threads. Call once per frame.
    pub fn drain_completed(&self) -> Vec<(JobId, JobResult)> {
        // Collect from the channel into a local vec first, then merge under
        // a single results lock — avoids nested locking and lost results.
        let mut incoming = Vec::new();
        if let Ok(rx) = self.result_rx.lock() {
            while let Ok(result) = rx.try_recv() {
                incoming.push(result);
            }
        }
        if let Ok(mut results) = self.results.lock() {
            results.extend(incoming);
            std::mem::take(&mut *results)
        } else {
            incoming
        }
    }

    /// Signal shutdown and join all worker threads (5s timeout per worker).
    pub fn shut_down(self) {
        self.shutdown.store(true, Ordering::SeqCst);
        drop(self.sender);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        for worker in self.workers {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                log::warn!("JobPool shutdown timed out — abandoning remaining workers");
                break;
            }
            // Park this thread and check periodically if the worker finished.
            while !worker.is_finished() {
                if std::time::Instant::now() >= deadline {
                    log::warn!("JobPool worker did not exit within deadline");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

fn worker_loop(
    rx: Arc<Mutex<mpsc::Receiver<JobEnvelope>>>,
    tx: mpsc::Sender<(JobId, JobResult)>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let envelope = {
            let Ok(rx) = rx.lock() else { break };
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(env) => env,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };

        let ctx = JobContext {
            job_id: envelope.id,
            cancelled: Arc::clone(&envelope.cancel),
        };

        if ctx.is_cancelled() {
            let _ = tx.send((envelope.id, JobResult::Cancelled));
            continue;
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut payload = envelope.payload;
            payload.run(&ctx)
        }));

        // Re-check cancellation after execution — covers the race where
        // cancel() fires between the pre-check and run().
        let job_result = if ctx.is_cancelled() {
            JobResult::Cancelled
        } else {
            match result {
                Ok(r) => r,
                Err(_) => JobResult::Err("job panicked".into()),
            }
        };

        let _ = tx.send((envelope.id, job_result));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoJob(String);
    impl JobPayload for EchoJob {
        fn run(&mut self, _ctx: &JobContext) -> JobResult {
            JobResult::Done(self.0.clone())
        }
        fn describe(&self) -> &str {
            "echo"
        }
    }

    struct SlowJob;
    impl JobPayload for SlowJob {
        fn run(&mut self, ctx: &JobContext) -> JobResult {
            for _ in 0..100 {
                if ctx.is_cancelled() {
                    return JobResult::Cancelled;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            JobResult::Done("done".into())
        }
        fn describe(&self) -> &str {
            "slow"
        }
    }

    struct PanicJob;
    impl JobPayload for PanicJob {
        fn run(&mut self, _ctx: &JobContext) -> JobResult {
            panic!("intentional panic");
        }
        fn describe(&self) -> &str {
            "panic"
        }
    }

    #[test]
    fn submit_and_drain() {
        let pool = JobPool::start_up(2);
        let _h = pool.submit(JobPriority::Normal, Box::new(EchoJob("hello".into())));
        std::thread::sleep(std::time::Duration::from_millis(100));
        let results = pool.drain_completed();
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].1, JobResult::Done(ref s) if s == "hello"));
        pool.shut_down();
    }

    #[test]
    fn cancel_before_run() {
        let pool = JobPool::start_up(1);
        let _slow = pool.submit(JobPriority::Normal, Box::new(SlowJob));
        let handle = pool.submit(JobPriority::Normal, Box::new(EchoJob("cancelled".into())));
        handle.cancel();
        std::thread::sleep(std::time::Duration::from_millis(200));
        pool.shut_down();
    }

    #[test]
    fn worker_panic_doesnt_kill_pool() {
        let pool = JobPool::start_up(2);
        let _p = pool.submit(JobPriority::Normal, Box::new(PanicJob));
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _h = pool.submit(JobPriority::Normal, Box::new(EchoJob("after_panic".into())));
        std::thread::sleep(std::time::Duration::from_millis(100));
        let results = pool.drain_completed();
        assert!(!results.is_empty());
        pool.shut_down();
    }

    #[test]
    fn shutdown_joins_workers() {
        let pool = JobPool::start_up(4);
        for i in 0..10 {
            pool.submit(JobPriority::Normal, Box::new(EchoJob(format!("job-{i}"))));
        }
        pool.shut_down();
    }

    #[test]
    fn stress_many_jobs() {
        let pool = JobPool::start_up(4);
        for i in 0..100 {
            pool.submit(JobPriority::Normal, Box::new(EchoJob(format!("{i}"))));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        let results = pool.drain_completed();
        assert_eq!(results.len(), 100);
        pool.shut_down();
    }
}

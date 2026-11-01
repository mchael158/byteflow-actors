//! Bounded [`byteflow::HostAwaitBridge`] backed by a fixed host worker pool.
//!
//! Jobs sit in an MPMC bounded queue (`Mutex` + `Condvar` + `VecDeque`). Any
//! idle worker may take the next job — `worker_threads > 1` is true
//! parallelism, not a mutex around a single `mpsc::Receiver`.
//!
//! `submit` never blocks a Byteflow worker: the job is accepted or rejected
//! immediately. Completions settle through [`byteflow::finish_host_await`].

use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};

use byteflow::{
    finish_host_await, FlowId, HostAwaitBridge, HostAwaitCompleter, HostAwaitOp, HostAwaitRequest,
};

use crate::metrics::IoMetrics;
use crate::{IoDriver, IoDriverError, IoInitError};

/// Cooperative cancellation flag observed by host drivers during HostAwait work.
///
/// The flag alone does not close sockets or abort OS calls. Drivers must poll
/// [`CancellationToken::is_cancelled`] between blocking steps and apply their
/// own deadlines.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Returns `true` after the runtime cancelled this ticket or the bridge
    /// received [`HostAwaitBridge::cancel`].
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Mark this token cancelled (host policy / tests).
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct AwaitKey {
    flow: FlowId,
    op: HostAwaitOp,
    ticket: u64,
}

struct Job {
    request: HostAwaitRequest,
    completer: HostAwaitCompleter,
    token: CancellationToken,
    key: AwaitKey,
}

struct QueueState {
    jobs: VecDeque<Job>,
    capacity: usize,
    closed: bool,
}

/// Multi-producer / multi-consumer bounded job queue.
struct JobQueue {
    state: Mutex<QueueState>,
    has_job: Condvar,
}

impl JobQueue {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                jobs: VecDeque::with_capacity(capacity),
                capacity,
                closed: false,
            }),
            has_job: Condvar::new(),
        }
    }

    fn try_push(&self, job: Job) -> Result<(), QueueSubmitError> {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.closed {
            return Err(QueueSubmitError::Closed(Box::new(job)));
        }
        if state.jobs.len() >= state.capacity {
            return Err(QueueSubmitError::Full(Box::new(job)));
        }
        state.jobs.push_back(job);
        self.has_job.notify_one();
        Ok(())
    }

    fn pop_wait(&self) -> Option<Job> {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        loop {
            if let Some(job) = state.jobs.pop_front() {
                return Some(job);
            }
            if state.closed {
                return None;
            }
            state = match self.has_job.wait(state) {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    fn close(&self) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.closed = true;
        self.has_job.notify_all();
    }
}

struct WorkerPool {
    queue: Arc<JobQueue>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPool")
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl WorkerPool {
    fn new(
        driver: Arc<dyn IoDriver>,
        worker_count: usize,
        queue_capacity: usize,
        thread_name: &str,
        cancellations: Arc<Mutex<HashMap<AwaitKey, Weak<AtomicBool>>>>,
        metrics: Arc<IoMetrics>,
    ) -> Result<Self, IoInitError> {
        if worker_count == 0 {
            return Err(IoInitError::ZeroWorkers);
        }
        if queue_capacity == 0 {
            return Err(IoInitError::ZeroQueueCapacity);
        }

        let queue = Arc::new(JobQueue::new(queue_capacity));
        let mut workers = Vec::with_capacity(worker_count);

        for index in 0..worker_count {
            let worker_queue = Arc::clone(&queue);
            let worker_driver = Arc::clone(&driver);
            let worker_cancellations = Arc::clone(&cancellations);
            let worker_metrics = Arc::clone(&metrics);
            let name = format!("{thread_name}-{index}");

            let spawn_result = thread::Builder::new().name(name).spawn(move || {
                worker_loop(
                    worker_queue,
                    worker_driver,
                    worker_cancellations,
                    worker_metrics,
                );
            });

            match spawn_result {
                Ok(handle) => workers.push(handle),
                Err(error) => {
                    queue.close();
                    for handle in workers {
                        let _ = handle.join();
                    }
                    return Err(IoInitError::ThreadSpawn(error.to_string()));
                }
            }
        }

        Ok(Self {
            queue,
            workers: Mutex::new(workers),
            closed: AtomicBool::new(false),
        })
    }

    fn try_send(&self, job: Job) -> Result<(), QueueSubmitError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(QueueSubmitError::Closed(Box::new(job)));
        }
        self.queue.try_push(job)
    }

    fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }

        self.queue.close();

        let workers = match self.workers.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };

        let current = thread::current().id();
        for worker in workers {
            // Do not self-join when shutdown is invoked from a worker thread.
            if worker.thread().id() != current {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.queue.close();
        let workers = match self.workers.get_mut() {
            Ok(workers) => std::mem::take(workers),
            Err(poisoned) => std::mem::take(poisoned.into_inner()),
        };
        let current = thread::current().id();
        for worker in workers {
            if worker.thread().id() != current {
                let _ = worker.join();
            }
        }
    }
}

enum QueueSubmitError {
    Full(Box<Job>),
    Closed(Box<Job>),
}

/// Bounded host worker pool implementing Byteflow’s [`HostAwaitBridge`].
///
/// Suitable as [`byteflow::RuntimeConfig::host_await`]. Queue saturation and
/// shutdown fail the outstanding completer immediately instead of blocking a
/// scheduler worker.
pub struct WorkerHostAwaitBridge {
    pool: WorkerPool,
    cancellations: Arc<Mutex<HashMap<AwaitKey, Weak<AtomicBool>>>>,
    metrics: Arc<IoMetrics>,
}

impl std::fmt::Debug for WorkerHostAwaitBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerHostAwaitBridge")
            .field("pool", &self.pool)
            .finish_non_exhaustive()
    }
}

impl WorkerHostAwaitBridge {
    /// Spawn `worker_count` host threads behind a channel of `queue_capacity`.
    ///
    /// # Errors
    ///
    /// Returns [`IoInitError`] when either capacity is zero or thread creation
    /// fails. On spawn failure, already-started workers are joined before the
    /// error is returned.
    pub fn new(
        driver: Arc<dyn IoDriver>,
        worker_count: usize,
        queue_capacity: usize,
        thread_name: impl AsRef<str>,
    ) -> Result<Self, IoInitError> {
        let cancellations = Arc::new(Mutex::new(HashMap::new()));
        let metrics = Arc::new(IoMetrics::default());
        let pool = WorkerPool::new(
            driver,
            worker_count,
            queue_capacity,
            thread_name.as_ref(),
            Arc::clone(&cancellations),
            Arc::clone(&metrics),
        )?;

        Ok(Self {
            pool,
            cancellations,
            metrics,
        })
    }

    /// Shared metrics for this bridge (submit / full / settle counters).
    pub fn metrics(&self) -> &Arc<IoMetrics> {
        &self.metrics
    }

    /// Refuse new submissions, drain accepted work, and join worker threads.
    pub fn shutdown(&self) {
        self.pool.shutdown();
    }

    fn remove_cancellation(&self, key: AwaitKey) {
        if let Ok(mut entries) = self.cancellations.lock() {
            entries.remove(&key);
        }
    }
}

impl HostAwaitBridge for WorkerHostAwaitBridge {
    fn submit(&self, request: HostAwaitRequest, completer: HostAwaitCompleter) {
        let key = AwaitKey {
            flow: request.flow,
            op: request.op,
            ticket: completer.ticket(),
        };
        let token = CancellationToken::default();

        match self.cancellations.lock() {
            Ok(mut entries) => {
                entries.insert(key, Arc::downgrade(&token.cancelled));
            }
            Err(_) => {
                let _ = completer.fail("byteflow-io cancellation registry is poisoned");
                return;
            }
        }

        let job = Job {
            request,
            completer,
            token,
            key,
        };

        match self.pool.try_send(job) {
            Ok(()) => {
                self.metrics.record_submitted();
            }
            Err(QueueSubmitError::Full(job)) => {
                self.metrics.record_full();
                self.remove_cancellation(job.key);
                let _ = job.completer.fail("byteflow-io host-await queue is full");
            }
            Err(QueueSubmitError::Closed(job)) => {
                self.metrics.record_closed();
                self.remove_cancellation(job.key);
                let _ = job.completer.fail("byteflow-io is shutting down");
            }
        }
    }

    fn cancel(&self, flow: FlowId, op: HostAwaitOp, ticket: u64) {
        let key = AwaitKey { flow, op, ticket };
        let token = match self.cancellations.lock() {
            Ok(mut entries) => entries.remove(&key).and_then(|weak| weak.upgrade()),
            Err(_) => None,
        };
        if let Some(inner) = token {
            CancellationToken { cancelled: inner }.cancel();
        }
    }
}

fn worker_loop(
    queue: Arc<JobQueue>,
    driver: Arc<dyn IoDriver>,
    cancellations: Arc<Mutex<HashMap<AwaitKey, Weak<AtomicBool>>>>,
    metrics: Arc<IoMetrics>,
) {
    while let Some(job) = queue.pop_wait() {
        let cancelled = job.token.is_cancelled() || job.completer.is_cancelled();
        let result = if cancelled {
            Err(IoDriverError::cancelled(
                "HostAwait operation cancelled before execution",
            ))
        } else {
            match catch_unwind(AssertUnwindSafe(|| {
                driver.execute_host_await(job.request.op, job.request.args.clone(), &job.token)
            })) {
                Ok(result) => result,
                Err(_) => Err(IoDriverError::other("host I/O driver panicked")),
            }
        };

        match &result {
            Ok(_) => metrics.record_ok(),
            Err(_) if cancelled => {}
            Err(_) => metrics.record_err(),
        }

        // Settle through the core helper so cancel/stale races match Byteflow
        // (`Stale` when the ticket was already finalized).
        let _ = finish_host_await(job.completer, result.map_err(|error| error.to_string()));

        if let Ok(mut entries) = cancellations.lock() {
            entries.remove(&job.key);
        }
    }
}

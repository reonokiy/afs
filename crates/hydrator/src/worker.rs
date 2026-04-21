use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, info, warn};

use crate::queue::{self, HydrationQueue, HydrationTask};

/// Result sent back to waiters.
type WaiterResult = Result<Bytes, Arc<anyhow::Error>>;

/// Callback to fetch a blob by OID. Implemented by the BlobStore.
pub type FetchFn = Arc<dyn Fn(String) -> tokio::task::JoinHandle<anyhow::Result<Bytes>> + Send + Sync>;

/// The hydrator service: manages a priority queue and concurrent workers.
pub struct Hydrator {
    queue: Arc<Mutex<HydrationQueue>>,
    /// In-flight dedup: oid → list of waiters.
    inflight: Arc<DashMap<String, Vec<oneshot::Sender<WaiterResult>>>>,
    /// Channel to wake workers when new work arrives.
    work_tx: mpsc::Sender<()>,
    stop_tx: Option<mpsc::Sender<()>>,
}

impl Hydrator {
    /// Create and start the hydrator with the given number of workers.
    pub fn start(workers: usize, fetch_fn: FetchFn) -> Self {
        let queue = Arc::new(Mutex::new(HydrationQueue::new()));
        let inflight: Arc<DashMap<String, Vec<oneshot::Sender<WaiterResult>>>> =
            Arc::new(DashMap::new());
        let (work_tx, work_rx) = mpsc::channel::<()>(64);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);

        let work_rx = Arc::new(Mutex::new(work_rx));
        let stop_rx = Arc::new(Mutex::new(stop_rx));

        for i in 0..workers {
            let queue = queue.clone();
            let inflight = inflight.clone();
            let work_rx = work_rx.clone();
            let stop_rx = stop_rx.clone();
            let fetch_fn = fetch_fn.clone();

            tokio::spawn(async move {
                loop {
                    // Wait for work signal or stop
                    {
                        let mut rx = work_rx.lock().await;
                        let mut stop = stop_rx.lock().await;
                        tokio::select! {
                            _ = rx.recv() => {}
                            _ = stop.recv() => {
                                debug!(worker = i, "hydrator worker stopping");
                                return;
                            }
                        }
                    }

                    // Drain queue
                    loop {
                        let task = {
                            let mut q = queue.lock().await;
                            q.pop()
                        };

                        let task = match task {
                            Some(t) => t,
                            None => break,
                        };

                        let oid = task.oid.clone();

                        // Fetch the blob
                        let handle = (fetch_fn)(oid.clone());
                        let result = match handle.await {
                            Ok(Ok(data)) => Ok(data),
                            Ok(Err(e)) => {
                                warn!(%oid, error = %e, "hydration failed");
                                Err(Arc::new(e))
                            }
                            Err(e) => {
                                warn!(%oid, error = %e, "hydration task panicked");
                                Err(Arc::new(anyhow::anyhow!("task panicked: {}", e)))
                            }
                        };

                        // Notify all waiters
                        if let Some((_, waiters)) = inflight.remove(&oid) {
                            for tx in waiters {
                                let _ = tx.send(result.clone());
                            }
                        }
                    }
                }
            });
        }

        info!(workers, "hydrator started");

        Self {
            queue,
            inflight,
            work_tx,
            stop_tx: Some(stop_tx),
        }
    }

    /// Enqueue a background hydration task (fire-and-forget prefetch).
    pub async fn enqueue(&self, task: HydrationTask) {
        let oid = task.oid.clone();
        // Skip if already in-flight
        if self.inflight.contains_key(&oid) {
            return;
        }
        {
            let mut q = self.queue.lock().await;
            q.push(task);
        }
        let _ = self.work_tx.try_send(());
    }

    /// Ensure a blob is hydrated. If not in-flight, enqueue it at highest priority.
    /// Returns the blob data when ready.
    pub async fn ensure_hydrated(&self, oid: &str, path: &str) -> anyhow::Result<Bytes> {
        let (tx, rx) = oneshot::channel();

        // Add ourselves as a waiter
        let first = {
            let mut entry = self.inflight.entry(oid.to_string()).or_default();
            let is_first = entry.is_empty();
            entry.push(tx);
            is_first
        };

        // If we're the first waiter, enqueue at explicit-read priority
        if first {
            let task = HydrationTask {
                oid: oid.to_string(),
                path: path.to_string(),
                priority: queue::PRIORITY_EXPLICIT_READ,
                reason: "explicit read",
                enqueued_at: Instant::now(),
            };
            {
                let mut q = self.queue.lock().await;
                q.push(task);
            }
            let _ = self.work_tx.try_send(());
        }

        // Wait for result
        match rx.await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(e)) => Err(anyhow::anyhow!("hydration failed: {}", e)),
            Err(_) => Err(anyhow::anyhow!("hydrator dropped")),
        }
    }

    /// Get the current queue depth.
    pub async fn queue_depth(&self) -> usize {
        self.queue.lock().await.len()
    }
}

impl Drop for Hydrator {
    fn drop(&mut self) {
        // Signal workers to stop
        self.stop_tx.take();
    }
}

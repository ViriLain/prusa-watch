//! A bounded, ordered I/O worker with observable overload and owned shutdown.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

#[derive(Default)]
pub struct WorkerStats {
    pub completed: AtomicU64,
    pub dropped: AtomicU64,
    pub errors: AtomicU64,
    pub last_error: Mutex<Option<String>>,
}

pub struct Worker<T> {
    sender: Mutex<Option<mpsc::SyncSender<T>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    stopping: Arc<AtomicBool>,
    pub stats: Arc<WorkerStats>,
}

impl<T: Send + 'static> Worker<T> {
    pub fn new(name: &str, capacity: usize, mut process: impl FnMut(T) -> Result<(), String> + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let stopping = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(WorkerStats::default());
        let stop = stopping.clone();
        let counts = stats.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while let Ok(task) = receiver.recv() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    match process(task) {
                        Ok(()) => {
                            counts.completed.fetch_add(1, Ordering::Relaxed);
                            *counts.last_error.lock().unwrap() = None;
                        }
                        Err(error) => {
                            counts.errors.fetch_add(1, Ordering::Relaxed);
                            *counts.last_error.lock().unwrap() = Some(error.clone());
                            tracing::error!("Background I/O failed: {error}");
                        }
                    }
                }
            })
            .expect("spawn I/O worker");
        Self {
            sender: Mutex::new(Some(sender)),
            thread: Mutex::new(Some(thread)),
            stopping,
            stats,
        }
    }

    pub fn submit(&self, task: T) -> bool {
        let sender = self.sender.lock().unwrap();
        if sender.as_ref().is_some_and(|sender| sender.try_send(task).is_ok()) {
            return true;
        }
        self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("Background I/O queue full or stopped; task dropped");
        false
    }

    /// Wait until earlier work finishes. Used for checkpoints at clean shutdown and in tests.
    pub fn flush(&self, barrier: impl FnOnce(mpsc::Sender<()>) -> T) -> Result<(), String> {
        let (done, received) = mpsc::channel();
        let sender = self.sender.lock().unwrap();
        sender
            .as_ref()
            .ok_or("worker stopped")?
            .send(barrier(done))
            .map_err(|e| e.to_string())?;
        drop(sender);
        received.recv().map_err(|e| e.to_string())
    }
}

impl<T> Worker<T> {
    /// Discard queued work; the currently running operation finishes within its own I/O timeout.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.sender.lock().unwrap().take();
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.stop();
    }
}

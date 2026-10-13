//! Ordered recording and coalesced atomic checkpoints off the monitor's control lock.
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use crate::config::RecordingConfig;
use crate::recording::{FrameRecord, Recorder};
use crate::worker::Worker;

pub struct Record {
    pub now: f64,
    pub frame_num: i64,
    pub progress: Option<f64>,
    pub confidences: Vec<f64>,
    pub ewm: f64,
    pub baseline: f64,
    pub short_mean: f64,
    pub score: f64,
    pub verdict: String,
    pub inference_ms: f64,
    pub jpeg: Arc<Vec<u8>>,
}

enum Task {
    Start(RecordingConfig, Option<i64>, Option<String>, f64),
    Record(RecordingConfig, Record),
    Failure(PathBuf, Arc<Vec<u8>>),
    Wake,
    Flush(mpsc::Sender<()>),
}

type Checkpoints = Arc<Mutex<Option<Vec<(PathBuf, Vec<u8>)>>>>;

pub struct Storage {
    worker: Worker<Task>,
    latest: Checkpoints,
}

pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)
}

impl Storage {
    pub fn new(cfg: RecordingConfig, state_dir: &std::path::Path) -> Self {
        Self::with_writer(cfg, state_dir, atomic_write)
    }

    fn with_writer(
        cfg: RecordingConfig,
        state_dir: &std::path::Path,
        mut write: impl FnMut(&std::path::Path, &[u8]) -> std::io::Result<()> + Send + 'static,
    ) -> Self {
        let mut recorder = Recorder::new(cfg, state_dir);
        let latest: Checkpoints = Arc::new(Mutex::new(None));
        let pending = latest.clone();
        let worker = Worker::new("storage", 16, move |task| {
            let before = recorder.errors;
            let barrier = match task {
                Task::Start(cfg, job, name, now) => {
                    recorder.cfg = cfg;
                    recorder.start_job(job, name.as_deref(), now);
                    None
                }
                Task::Record(cfg, r) => {
                    recorder.cfg = cfg;
                    recorder.record(&FrameRecord {
                        now: r.now,
                        frame_num: r.frame_num,
                        progress: r.progress,
                        confidences: &r.confidences,
                        ewm: r.ewm,
                        baseline: r.baseline,
                        short_mean: r.short_mean,
                        score: r.score,
                        verdict: &r.verdict,
                        inference_ms: r.inference_ms,
                        annotated_jpeg: Some(&r.jpeg),
                    });
                    None
                }
                Task::Failure(path, bytes) => {
                    recorder.save_failure(&path, &bytes).map_err(|e| e.to_string())?;
                    None
                }
                Task::Flush(done) => Some(done),
                Task::Wake => None,
            };
            // Drop the mailbox guard before touching disk; producers must never wait for I/O.
            let checkpoints = pending.lock().unwrap().take();
            if let Some(checkpoints) = checkpoints {
                for (path, bytes) in checkpoints {
                    write(&path, &bytes).map_err(|e| e.to_string())?;
                }
            }
            if let Some(done) = barrier {
                let _ = done.send(());
            }
            if recorder.errors > before {
                return Err("recording write failed; check state directory".into());
            }
            Ok(())
        });
        Self { worker, latest }
    }

    pub fn start_job(&self, cfg: RecordingConfig, job: Option<i64>, name: Option<String>, now: f64) {
        self.worker.submit(Task::Start(cfg, job, name, now));
    }
    pub fn record(&self, cfg: RecordingConfig, r: Record) {
        self.worker.submit(Task::Record(cfg, r));
    }
    pub fn failure(&self, path: PathBuf, jpeg: Arc<Vec<u8>>) {
        self.worker.submit(Task::Failure(path, jpeg));
    }
    pub fn checkpoint(&self, files: Vec<(PathBuf, Vec<u8>)>) {
        *self.latest.lock().unwrap() = Some(files);
        self.worker.submit(Task::Wake);
    }
    pub fn flush(&self) -> Result<(), String> {
        self.worker.flush(Task::Flush)
    }
    pub fn stop(&self) {
        self.worker.stop();
    }
    pub fn stats(&self) -> &crate::worker::WorkerStats {
        &self.worker.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn blocked_disk_does_not_block_checkpoint_producers() {
        let directory = tempfile::tempdir().unwrap();
        let (started, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let mut first = true;
        let storage = Arc::new(Storage::with_writer(
            Default::default(),
            directory.path(),
            move |path, bytes| {
                if first {
                    first = false;
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                }
                atomic_write(path, bytes)
            },
        ));
        storage.checkpoint(vec![(directory.path().join("first.json"), b"first".to_vec())]);
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done, result) = mpsc::channel();
        let producer = storage.clone();
        let path = directory.path().join("latest.json");
        let expected = path.clone();
        let thread = std::thread::spawn(move || {
            producer.checkpoint(vec![(path, b"latest".to_vec())]);
            done.send(()).unwrap();
        });
        let nonblocking = result.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        thread.join().unwrap();
        storage.flush().unwrap();
        assert!(nonblocking.is_ok(), "checkpoint producer waited for the disk");
        assert_eq!(std::fs::read(expected).unwrap(), b"latest");
        storage.stop();
    }
}

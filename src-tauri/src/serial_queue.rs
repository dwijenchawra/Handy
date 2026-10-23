//! Dedicated FIFO workers keep slow inference and disk I/O off shortcut handling.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct SerialQueue {
    tx: mpsc::Sender<Job>,
    pending: Arc<AtomicUsize>,
}

impl SerialQueue {
    pub fn new(name: &str) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let pending = Arc::new(AtomicUsize::new(0));
        let worker_pending = pending.clone();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    // One failed model/job must not kill the queue and lose later captures.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                        log::error!("Dictation queue job panicked; continuing with next recording");
                    }
                    worker_pending.fetch_sub(1, Ordering::Release);
                }
            })
            .expect("Failed to start dictation queue worker");
        Self { tx, pending }
    }

    pub fn is_idle(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }

    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(Box::new(job)).is_err() {
            self.pending.fetch_sub(1, Ordering::Release);
            log::error!("Dictation queue worker is unavailable");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn accepts_new_jobs_while_busy_and_preserves_order_after_panic() {
        let queue = SerialQueue::new("queue-test");
        let (release, wait) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let first = tx.clone();
        queue.submit(move || {
            wait.recv().unwrap();
            first.send(1).unwrap();
        });
        queue.submit(|| panic!("simulated failed inference"));
        queue.submit(move || {
            tx.send(2).unwrap();
        });
        assert!(rx.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
    }
}

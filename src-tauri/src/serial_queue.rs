//! Dedicated FIFO workers keep slow inference and disk I/O off shortcut handling.
use std::sync::mpsc;

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct SerialQueue {
    tx: mpsc::Sender<Job>,
}

impl SerialQueue {
    pub fn new(name: &str) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    // One failed model/job must not kill the queue and lose later captures.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                        log::error!("Dictation queue job panicked; continuing with next recording");
                    }
                }
            })
            .expect("Failed to start dictation queue worker");
        Self { tx }
    }

    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        if self.tx.send(Box::new(job)).is_err() {
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

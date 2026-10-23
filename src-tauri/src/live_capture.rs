//! Speculative live recognition never owns or blocks the authoritative capture.
use crate::managers::transcription::TranscriptionManager;
use anyhow::Result;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};
use tauri::Manager;

pub struct Outcome {
    pub result: Result<Option<String>>,
    pub finished: Instant,
}

pub struct Results {
    pub local: Option<mpsc::Receiver<Outcome>>,
    pub remote: Option<mpsc::Receiver<Outcome>>,
    valid: Arc<AtomicBool>,
}
impl Results {
    pub fn valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }
}

enum Input {
    Samples(Vec<f32>, u32),
    Finish,
}

pub struct LiveCapture {
    tx: mpsc::SyncSender<Input>,
    results: Results,
}

impl LiveCapture {
    pub fn new(app: tauri::AppHandle) -> Self {
        let settings = crate::settings::get_settings(&app);
        let (tx, rx) = mpsc::sync_channel(64);
        let valid = Arc::new(AtomicBool::new(true));
        let (local_tx, local_rx) = mpsc::channel();
        let (remote_tx, remote_rx) = mpsc::channel();
        let use_local = !settings.remote_stt_enabled || settings.remote_stt_compare;
        let use_remote = settings.remote_stt_enabled || settings.remote_stt_compare;
        let pending = app.state::<crate::TranscriptionCoordinator>().has_pending();
        let allow_local = !pending && crate::actions::PROCESSING_QUEUE.is_idle();
        let allow_remote =
            crate::stt_benchmark::remote_idle() && (settings.remote_stt_compare || !pending);
        let valid_worker = valid.clone();
        std::thread::spawn(move || {
            let tm = app.state::<Arc<TranscriptionManager>>().inner().clone();
            let local = if use_local && allow_local {
                tm.initiate_local_model_load();
                tm.start_stream()
            } else {
                false
            };
            let remote = if use_remote && allow_remote {
                Some(crate::remote_stt::start_live(
                    settings.remote_stt_url,
                    remote_tx,
                ))
            } else {
                let _ = remote_tx.send(Outcome {
                    result: Ok(None),
                    finished: Instant::now(),
                });
                None
            };
            let remote_valid = Arc::new(AtomicBool::new(true));
            let mut resampler = None;
            let feed = |frame: &[f32]| {
                if local {
                    tm.stream_router().feed(frame);
                }
                if let Some(tx) = &remote {
                    if tx
                        .try_send(crate::remote_stt::LiveInput::Samples(frame.to_vec()))
                        .is_err()
                    {
                        remote_valid.store(false, Ordering::Release);
                    }
                }
            };
            let mut finished = false;
            while let Ok(input) = rx.recv() {
                match input {
                    Input::Samples(samples, rate) => {
                        let resampler = resampler.get_or_insert_with(|| {
                            crate::audio_toolkit::audio::FrameResampler::new(
                                rate as usize,
                                16000,
                                Duration::from_millis(30),
                            )
                        });
                        resampler.push(&samples, &feed);
                    }
                    Input::Finish => {
                        finished = true;
                        break;
                    }
                }
            }
            if let Some(resampler) = &mut resampler {
                resampler.finish(&feed);
            }
            let valid = finished && valid_worker.load(Ordering::Acquire);
            if let Some(tx) = remote {
                let _ = tx.try_send(crate::remote_stt::LiveInput::Finish(
                    valid && remote_valid.load(Ordering::Acquire),
                ));
            }
            let result = if local && valid {
                tm.finalize_stream()
            } else {
                if local {
                    tm.cancel_stream();
                }
                Ok(None)
            };
            let _ = local_tx.send(Outcome {
                result,
                finished: Instant::now(),
            });
        });
        Self {
            tx,
            results: Results {
                local: use_local.then_some(local_rx),
                remote: use_remote.then_some(remote_rx),
                valid,
            },
        }
    }

    pub fn feed(&self, samples: &[f32], rate: u32) {
        if self.results.valid()
            && self
                .tx
                .try_send(Input::Samples(samples.to_vec(), rate))
                .is_err()
        {
            self.results.valid.store(false, Ordering::Release);
        }
    }

    pub fn finish(self, valid: bool) -> Results {
        if !valid || self.tx.try_send(Input::Finish).is_err() {
            self.results.valid.store(false, Ordering::Release);
        }
        self.results
    }
}

/// Failed, unsupported or interrupted live attempts fall back to the saved take.
pub fn usable_result(rx: Option<mpsc::Receiver<Outcome>>) -> Option<(String, Instant)> {
    let rx = rx?;
    match rx.recv_timeout(Duration::from_secs(135)) {
        Ok(Outcome {
            result: Ok(Some(text)),
            finished,
        }) if !text.trim().is_empty() => Some((text, finished)),
        Ok(Outcome {
            result: Err(error), ..
        }) => {
            log::warn!("Live recognition failed; using complete saved audio: {error}");
            None
        }
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn test_capture() -> (LiveCapture, mpsc::Receiver<Vec<f32>>) {
    let (tx, rx) = mpsc::sync_channel(64);
    let (observed_tx, observed_rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(Input::Samples(samples, _)) = rx.recv() {
            let _ = observed_tx.send(samples);
        }
    });
    (
        LiveCapture {
            tx,
            results: Results {
                local: None,
                remote: None,
                valid: Arc::new(AtomicBool::new(true)),
            },
        },
        observed_rx,
    )
}

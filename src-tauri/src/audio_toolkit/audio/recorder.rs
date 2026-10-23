use std::{
    io::Error,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, SizedSample,
};

use crate::audio_toolkit::{
    audio::{AudioVisualiser, FrameResampler},
    constants,
    vad::{self, VadFrame},
    VoiceActivityDetector,
};

use super::capture::{CaptureBuffer, CapturedAudio};

enum Cmd {
    Start(Instant),
    StartLive(Instant, crate::live_capture::LiveCapture),
    Stop(Instant, mpsc::Sender<CapturedAudio>),
    Samples(Instant, Vec<f32>),
    StreamError(String),
    Shutdown,
}

pub struct AudioRecorder {
    healthy: Arc<AtomicBool>,
    selected_channel: Option<usize>,
    device: Option<Device>,
    cmd_tx: Option<mpsc::Sender<Cmd>>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
    level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>>,
    error_cb: Option<Arc<dyn Fn(String) + Send + Sync + 'static>>,
}

impl AudioRecorder {
    pub fn preferred_input_channel_count(
        device: &Device,
    ) -> Result<u16, Box<dyn std::error::Error>> {
        Ok(Self::get_preferred_config(device)?.channels())
    }

    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(AudioRecorder {
            healthy: Arc::new(AtomicBool::new(false)),
            selected_channel: None,
            device: None,
            cmd_tx: None,
            worker_handle: None,
            vad: None,
            level_cb: None,
            error_cb: None,
        })
    }

    pub fn set_selected_channel(&mut self, channel: Option<u16>) {
        self.selected_channel = channel.map(usize::from);
    }

    pub fn with_vad(mut self, vad: Box<dyn VoiceActivityDetector>) -> Self {
        self.vad = Some(Arc::new(Mutex::new(vad)));
        self
    }

    pub fn with_level_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(Vec<f32>) + Send + Sync + 'static,
    {
        self.level_cb = Some(Arc::new(cb));
        self
    }

    pub fn with_error_callback(mut self, cb: impl Fn(String) + Send + Sync + 'static) -> Self {
        self.error_cb = Some(Arc::new(cb));
        self
    }

    pub fn open(&mut self, device: Option<Device>) -> Result<(), Box<dyn std::error::Error>> {
        if self.worker_handle.is_some() {
            return Ok(()); // already open
        }

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let host = crate::audio_toolkit::get_cpal_host();
        let device = match device {
            Some(dev) => dev,
            None => host
                .default_input_device()
                .ok_or_else(|| Error::new(std::io::ErrorKind::NotFound, "No input device found"))?,
        };

        let thread_device = device.clone();
        // Move the optional level callback into the worker thread
        let level_cb = self.level_cb.clone();
        let error_cb = self.error_cb.clone();

        let sample_tx = cmd_tx.clone();
        let selected_channel = self.selected_channel;
        let healthy = self.healthy.clone();
        let worker = std::thread::spawn(move || {
            let init_result = (|| -> Result<(cpal::Stream, u32), String> {
                let config = AudioRecorder::get_preferred_config(&thread_device)
                    .map_err(|e| format!("Failed to fetch preferred config: {e}"))?;

                let sample_rate = config.sample_rate().0;
                let channels = config.channels() as usize;

                log::info!(
                    "Using device: {:?}\nSample rate: {}\nChannels: {}\nFormat: {:?}",
                    thread_device.name(),
                    sample_rate,
                    channels,
                    config.sample_format()
                );

                let stream = match config.sample_format() {
                    cpal::SampleFormat::U8 => AudioRecorder::build_stream::<u8>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        selected_channel,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::I8 => AudioRecorder::build_stream::<i8>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        selected_channel,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::I16 => AudioRecorder::build_stream::<i16>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        selected_channel,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::I32 => AudioRecorder::build_stream::<i32>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        selected_channel,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::F32 => AudioRecorder::build_stream::<f32>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        selected_channel,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    sample_format => {
                        return Err(format!("Unsupported sample format: {sample_format:?}"));
                    }
                };

                stream
                    .play()
                    .map_err(|e| format!("Failed to start microphone stream: {e}"))?;

                Ok((stream, sample_rate))
            })();

            match init_result {
                Ok((stream, sample_rate)) => {
                    // Report readiness only after an actual input buffer.
                    run_consumer(sample_rate, cmd_rx, level_cb, error_cb, init_tx, healthy);
                    drop(stream);
                }
                Err(error_message) => {
                    log::error!("{error_message}");
                    let _ = init_tx.send(Err(error_message));
                }
            }
        });

        match init_rx.recv() {
            Ok(Ok(())) => {
                self.device = Some(device);
                self.cmd_tx = Some(cmd_tx);
                self.worker_handle = Some(worker);
                Ok(())
            }
            Ok(Err(error_message)) => {
                let _ = worker.join();
                let kind = if is_microphone_access_denied(&error_message) {
                    std::io::ErrorKind::PermissionDenied
                } else {
                    std::io::ErrorKind::Other
                };
                Err(Box::new(Error::new(kind, error_message)))
            }
            Err(recv_error) => {
                let _ = worker.join();
                Err(Box::new(Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to initialize microphone worker: {recv_error}"),
                )))
            }
        }
    }

    pub fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.start_at(Instant::now())
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn start_live(
        &self,
        at: Instant,
        live: crate::live_capture::LiveCapture,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.is_healthy() {
            return Err("Microphone is not delivering audio".into());
        }
        self.cmd_tx
            .as_ref()
            .ok_or("Microphone is not open")?
            .send(Cmd::StartLive(at, live))?;
        Ok(())
    }

    pub fn start_at(&self, at: Instant) -> Result<(), Box<dyn std::error::Error>> {
        if !self.is_healthy() {
            return Err("Microphone is not delivering audio".into());
        }
        self.cmd_tx
            .as_ref()
            .ok_or("Microphone is not open")?
            .send(Cmd::Start(at))?;
        Ok(())
    }

    pub fn stop_at(
        &self,
        at: Instant,
    ) -> Result<mpsc::Receiver<CapturedAudio>, Box<dyn std::error::Error>> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .as_ref()
            .ok_or("Microphone is not open")?
            .send(Cmd::Stop(at, tx))?;
        Ok(rx)
    }

    pub fn stop(&self) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let capture = self.stop_at(Instant::now())?.recv()?;
        if let Some(error) = &capture.error {
            log::error!("Incomplete capture: {error}");
        }
        let mut output = Vec::new();
        let mut resampler = FrameResampler::new(
            capture.sample_rate as usize,
            constants::WHISPER_SAMPLE_RATE as usize,
            Duration::from_millis(30),
        );
        let mut emit = |frame: &[f32]| {
            if let Some(vad) = &self.vad {
                match vad
                    .lock()
                    .unwrap()
                    .push_frame(frame)
                    .unwrap_or(VadFrame::Speech(frame))
                {
                    VadFrame::Speech(samples) => output.extend_from_slice(samples),
                    VadFrame::Noise => {}
                }
            } else {
                output.extend_from_slice(frame);
            }
        };
        resampler.push(&capture.samples, &mut emit);
        resampler.finish(&mut emit);
        Ok(output)
    }

    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.healthy.store(false, Ordering::Release);
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Cmd::Shutdown);
        }
        if let Some(h) = self.worker_handle.take() {
            let _ = h.join();
        }
        self.device = None;
        Ok(())
    }

    fn build_stream<T>(
        device: &cpal::Device,
        config: &cpal::SupportedStreamConfig,
        sample_tx: mpsc::Sender<Cmd>,
        channels: usize,
        selected_channel: Option<usize>,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        T: Sample + SizedSample + Send + 'static,
        f32: cpal::FromSample<T>,
    {
        const BLOCK: usize = 512;
        struct Packet {
            at: Instant,
            len: usize,
            samples: [f32; BLOCK],
        }
        // Preallocate outside CoreAudio's callback. Capacity also accommodates
        // small hardware callbacks without depending on callback buffer size.
        let capacity = (config.sample_rate().0 as usize * 2 / 64).max(128);
        let (mut producer, mut consumer) = rtrb::RingBuffer::<Packet>::new(capacity);
        let overruns = Arc::new(AtomicU64::new(0));
        let callback_overruns = overruns.clone();
        let error_tx = sample_tx.clone();
        std::thread::spawn(move || loop {
            let lost = overruns.swap(0, Ordering::AcqRel);
            if lost > 0 {
                let _ = sample_tx.send(Cmd::StreamError(format!(
                    "Microphone transport overrun: {lost} samples lost"
                )));
            }
            while let Ok(packet) = consumer.pop() {
                if sample_tx
                    .send(Cmd::Samples(
                        packet.at,
                        packet.samples[..packet.len].to_vec(),
                    ))
                    .is_err()
                {
                    return;
                }
            }
            if consumer.is_abandoned() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        });
        let rate = config.sample_rate().0 as f64;
        let selected_channel = selected_channel.filter(|channel| *channel < channels);
        let stream_cb = move |data: &[T], info: &cpal::InputCallbackInfo| {
            let timestamp = info.timestamp();
            let latency = timestamp
                .callback
                .duration_since(&timestamp.capture)
                .unwrap_or_default();
            let now = Instant::now();
            let captured_at = now.checked_sub(latency).unwrap_or(now);
            for (block_index, block) in data.chunks(BLOCK * channels).enumerate() {
                let mut packet = Packet {
                    at: captured_at + Duration::from_secs_f64((block_index * BLOCK) as f64 / rate),
                    len: block.len() / channels,
                    samples: [0.0; BLOCK],
                };
                for (index, frame) in block.chunks_exact(channels).enumerate() {
                    packet.samples[index] = if let Some(channel) = selected_channel {
                        frame[channel].to_sample::<f32>()
                    } else {
                        frame
                            .iter()
                            .map(|sample| sample.to_sample::<f32>())
                            .sum::<f32>()
                            / channels as f32
                    };
                }
                let len = packet.len;
                if producer.push(packet).is_err() {
                    callback_overruns.fetch_add(len as u64, Ordering::Relaxed);
                }
            }
        };

        device.build_input_stream(
            &config.clone().into(),
            stream_cb,
            move |err| {
                log::error!("Stream error: {}", err);
                let _ = error_tx.send(Cmd::StreamError(err.to_string()));
            },
            None,
        )
    }

    fn get_preferred_config(
        device: &cpal::Device,
    ) -> Result<cpal::SupportedStreamConfig, Box<dyn std::error::Error>> {
        // Use the device's native/default sample rate and let the FrameResampler
        // in run_consumer() downsample to 16kHz. This avoids forcing hardware into
        // a non-native rate which can cause issues on some devices (Bluetooth
        // codecs, certain ALSA drivers, etc.).
        let default_config = device.default_input_config()?;
        let target_rate = default_config.sample_rate();

        // Try to find the best sample format at the device's default rate
        let supported_configs = match device.supported_input_configs() {
            Ok(configs) => configs,
            Err(e) => {
                log::warn!("Could not enumerate input configs ({e}), using device default");
                return Ok(default_config);
            }
        };
        let mut best_config: Option<cpal::SupportedStreamConfigRange> = None;

        for config_range in supported_configs {
            if config_range.min_sample_rate() <= target_rate
                && config_range.max_sample_rate() >= target_rate
            {
                match best_config {
                    None => best_config = Some(config_range),
                    Some(ref current) => {
                        // Prioritize F32 > I16 > I32 > others
                        let score = |fmt: cpal::SampleFormat| match fmt {
                            cpal::SampleFormat::F32 => 4,
                            cpal::SampleFormat::I16 => 3,
                            cpal::SampleFormat::I32 => 2,
                            _ => 1,
                        };

                        if score(config_range.sample_format()) > score(current.sample_format()) {
                            best_config = Some(config_range);
                        }
                    }
                }
            }
        }

        if let Some(config) = best_config {
            return Ok(config.with_sample_rate(target_rate));
        }

        // Fall back to device default if no config matched (exotic/virtual devices)
        log::warn!(
            "No supported config matched device default rate {:?}, using default config",
            target_rate
        );
        Ok(default_config)
    }
}

pub fn is_microphone_access_denied(error_message: &str) -> bool {
    let normalized = error_message.to_lowercase();
    normalized.contains("access is denied")
        || normalized.contains("permission denied")
        || normalized.contains("0x80070005")
}

pub fn is_no_input_device_error(error_message: &str) -> bool {
    let normalized = error_message.to_lowercase();
    normalized.contains("no input device found")
        || (normalized.contains("failed to fetch preferred config")
            && normalized.contains("coreaudio"))
}

#[cfg(test)]
mod tests {
    use super::{is_microphone_access_denied, is_no_input_device_error};

    #[test]
    fn detects_access_is_denied() {
        assert!(is_microphone_access_denied("Access is denied"));
    }

    #[test]
    fn detects_permission_denied() {
        assert!(is_microphone_access_denied("permission denied"));
    }

    #[test]
    fn detects_windows_error_code() {
        assert!(is_microphone_access_denied("WASAPI error: 0x80070005"));
    }

    #[test]
    fn does_not_match_unrelated_errors() {
        assert!(!is_microphone_access_denied("device not found"));
    }

    #[test]
    fn detects_no_input_device() {
        assert!(is_no_input_device_error("No input device found"));
    }

    #[test]
    fn detects_coreaudio_config_error() {
        assert!(is_no_input_device_error(
            "Failed to fetch preferred config: A backend-specific error has occurred: An unknown error unknown to the coreaudio-rs API occurred"
        ));
    }

    #[test]
    fn does_not_match_other_errors_for_no_device() {
        assert!(!is_no_input_device_error("permission denied"));
        assert!(!is_no_input_device_error("device not found"));
    }
}

fn run_consumer(
    sample_rate: u32,
    rx: mpsc::Receiver<Cmd>,
    level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>>,
    error_cb: Option<Arc<dyn Fn(String) + Send + Sync + 'static>>,
    init_tx: mpsc::SyncSender<Result<(), String>>,
    healthy: Arc<AtomicBool>,
) {
    let mut capture = CaptureBuffer::new(sample_rate);
    let mut init_tx = Some(init_tx);
    let mut last_callback = Instant::now();
    let mut stalled = false;
    let mut recording = false;
    let mut last_visualization: Option<Instant> = None;
    let mut visualizer = AudioVisualiser::new(sample_rate, 512, 16, 400.0, 4000.0);
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Cmd::StartLive(at, live)) => {
                capture.start_live(at, live);
                recording = true;
                last_visualization = None;
                visualizer.reset();
            }
            Ok(Cmd::Start(at)) => {
                capture.start(at);
                recording = true;
                last_visualization = None;
                visualizer.reset();
            }
            Ok(Cmd::Stop(at, reply)) => {
                recording = false;
                capture.stop(at, reply);
            }
            Ok(Cmd::Samples(at, samples)) => {
                healthy.store(true, Ordering::Release);
                if let Some(tx) = init_tx.take() {
                    let _ = tx.send(Ok(()));
                }
                last_callback = Instant::now();
                stalled = false;
                // UI visualization is optional and bounded to 30 Hz. Every audio
                // buffer still reaches capture, including idle pre-roll and tails.
                // No FFT or webview event work runs while the user is not recording.
                let now = Instant::now();
                if recording
                    && level_cb.is_some()
                    && last_visualization
                        .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(33))
                {
                    if let Some(buckets) = visualizer.feed(&samples) {
                        if let Some(cb) = &level_cb {
                            cb(buckets);
                        }
                        last_visualization = Some(now);
                    }
                }
                capture.samples(at, samples);
            }
            Ok(Cmd::StreamError(error)) => {
                healthy.store(false, Ordering::Release);
                capture.fail(&error);
                if let Some(tx) = init_tx.take() {
                    let _ = tx.send(Err(error.clone()));
                    return;
                }
                if let Some(cb) = &error_cb {
                    cb(error);
                }
            }
            Ok(Cmd::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                capture.fail("Microphone stream closed");
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                capture.tick();
                if !stalled && last_callback.elapsed() >= Duration::from_secs(3) {
                    healthy.store(false, Ordering::Release);
                    let error = "Microphone stopped delivering audio".to_string();
                    capture.fail(&error);
                    if let Some(tx) = init_tx.take() {
                        let _ = tx.send(Err(error));
                        return;
                    }
                    if let Some(cb) = &error_cb {
                        cb(error);
                    }
                    stalled = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod consumer_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn idle_audio_does_not_emit_levels_and_capture_still_keeps_every_sample() {
        let (tx, rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let levels = Arc::new(AtomicUsize::new(0));
        let observed = levels.clone();
        let healthy = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn(move || {
            run_consumer(
                16000,
                rx,
                Some(Arc::new(move |_| {
                    observed.fetch_add(1, Ordering::Relaxed);
                })),
                None,
                init_tx,
                healthy,
            )
        });
        let base = Instant::now();
        // Enough idle samples to produce FFT/UI events in the previous code.
        for i in 0..10 {
            tx.send(Cmd::Samples(
                base + Duration::from_millis(i * 40),
                vec![0.25; 640],
            ))
            .unwrap();
        }
        init_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        // FIFO processing plus this completed pre-roll capture forms a barrier.
        tx.send(Cmd::Start(base)).unwrap();
        let (reply, captured) = mpsc::channel();
        tx.send(Cmd::Stop(base + Duration::from_millis(400), reply))
            .unwrap();
        let audio = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(audio.samples, vec![0.25; 6400]);
        assert_eq!(levels.load(Ordering::Relaxed), 0);

        // A new active take still gets its level feedback and all its samples.
        let start = base + Duration::from_millis(400);
        tx.send(Cmd::Start(start)).unwrap();
        tx.send(Cmd::Samples(start, vec![0.5; 640])).unwrap();
        let (reply, captured) = mpsc::channel();
        tx.send(Cmd::Stop(start + Duration::from_millis(40), reply))
            .unwrap();
        assert_eq!(
            captured
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .samples,
            vec![0.5; 640]
        );
        assert_eq!(levels.load(Ordering::Relaxed), 1);
        tx.send(Cmd::Shutdown).unwrap();
        worker.join().unwrap();
    }
}

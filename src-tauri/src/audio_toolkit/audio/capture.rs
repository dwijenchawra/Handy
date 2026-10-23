//! Timestamped capture independent of inference and UI work.
use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub struct CapturedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub error: Option<String>,
}

struct Chunk {
    at: Instant,
    samples: Vec<f32>,
}

struct Session {
    start: Instant,
    end: Option<Instant>,
    samples: Vec<f32>,
    reply: Option<Sender<CapturedAudio>>,
    spans: Vec<(Instant, usize, usize)>,
    last_end: Option<Instant>,
    error: Option<String>,
}

/// Retain a small in-memory pre-roll to cover scheduling latency between the
/// shortcut callback and its command being handled. Only the requested interval
/// is copied into a recording; idle audio is never persisted.
pub(super) struct CaptureBuffer {
    rate: u32,
    recent: VecDeque<Chunk>,
    active: Option<Session>,
    pending: Vec<Session>,
    latest_end: Option<Instant>,
}

impl CaptureBuffer {
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            recent: VecDeque::new(),
            active: None,
            pending: Vec::new(),
            latest_end: None,
        }
    }

    pub fn start(&mut self, at: Instant) {
        let mut session = Session {
            start: at,
            end: None,
            samples: Vec::new(),
            reply: None,
            spans: Vec::new(),
            last_end: None,
            error: None,
        };
        if self.recent.front().is_some_and(|chunk| chunk.at > at) {
            session.error = Some("Microphone was not ready at the start of the recording".into());
        }
        for chunk in &self.recent {
            append(&mut session, chunk, self.rate);
        }
        self.active = Some(session);
    }

    pub fn stop(&mut self, at: Instant, reply: Sender<CapturedAudio>) {
        if let Some(mut session) = self.active.take() {
            session.end = Some(at);
            // The command can arrive after audio beyond the release was consumed.
            while let Some(&(start, offset, len)) = session.spans.last() {
                let keep = sample_count(at.saturating_duration_since(start), self.rate).min(len);
                session.samples.truncate(offset + keep);
                if at >= start {
                    break;
                }
                session.spans.pop();
            }
            session.reply = Some(reply);
            self.pending.push(session);
            self.flush(at, false);
        } else {
            let _ = reply.send(CapturedAudio {
                samples: Vec::new(),
                sample_rate: self.rate,
                error: Some("No active recording".into()),
            });
        }
    }

    pub fn samples(&mut self, at: Instant, samples: Vec<f32>) {
        let end = at + Duration::from_secs_f64(samples.len() as f64 / self.rate as f64);
        let chunk = Chunk { at, samples };
        if let Some(session) = &mut self.active {
            append(session, &chunk, self.rate);
        }
        for session in &mut self.pending {
            append(session, &chunk, self.rate);
        }
        self.latest_end = Some(end);
        self.recent.push_back(chunk);
        while self
            .recent
            .front()
            .is_some_and(|chunk| end.saturating_duration_since(chunk.at) > Duration::from_secs(2))
        {
            self.recent.pop_front();
        }
        self.flush(Instant::now(), false);
    }

    pub fn fail(&mut self, message: &str) {
        if let Some(session) = &mut self.active {
            session.error = Some(message.into());
        }
        for session in &mut self.pending {
            session.error = Some(message.into());
        }
        self.flush(Instant::now(), true);
    }

    pub fn tick(&mut self) {
        self.flush(Instant::now(), false);
    }

    fn flush(&mut self, now: Instant, force: bool) {
        let mut i = 0;
        while i < self.pending.len() {
            let end = self.pending[i].end.expect("pending captures have an end");
            let complete = self.latest_end.is_some_and(|last| last >= end);
            let timed_out = now.saturating_duration_since(end) >= Duration::from_secs(1);
            if complete || timed_out || force {
                let mut session = self.pending.remove(i);
                if !complete && session.error.is_none() {
                    session.error = Some(
                        "Microphone stopped delivering audio before the recording ended".into(),
                    );
                }
                if let Some(reply) = session.reply {
                    let _ = reply.send(CapturedAudio {
                        samples: session.samples,
                        sample_rate: self.rate,
                        error: session.error,
                    });
                }
            } else {
                i += 1;
            }
        }
    }
}

fn sample_count(duration: Duration, rate: u32) -> usize {
    ((duration.as_nanos() * rate as u128).div_ceil(1_000_000_000)) as usize
}

fn append(session: &mut Session, chunk: &Chunk, rate: u32) {
    let from = sample_count(session.start.saturating_duration_since(chunk.at), rate)
        .min(chunk.samples.len());
    let to = session.end.map_or(chunk.samples.len(), |end| {
        sample_count(end.saturating_duration_since(chunk.at), rate).min(chunk.samples.len())
    });
    if from < to {
        let start = chunk.at + Duration::from_secs_f64(from as f64 / rate as f64);
        let previous = session.last_end.unwrap_or(session.start);
        if start.saturating_duration_since(previous) > Duration::from_millis(20) {
            session.error = Some("Microphone audio contains a gap".into());
        }
        session
            .spans
            .push((start, session.samples.len(), to - from));
        session.samples.extend_from_slice(&chunk.samples[from..to]);
        session.last_end = Some(chunk.at + Duration::from_secs_f64(to as f64 / rate as f64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn delayed_edges_select_only_held_interval() {
        let base = Instant::now();
        let mut buffer = CaptureBuffer::new(1000);
        buffer.samples(base, (0..100).map(|n| n as f32).collect());
        buffer.start(base + Duration::from_millis(20));
        let (tx, rx) = mpsc::channel();
        buffer.stop(base + Duration::from_millis(70), tx);
        assert_eq!(
            rx.recv().unwrap().samples,
            (20..70).map(|n| n as f32).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rapid_second_capture_does_not_lose_first_tail() {
        let base = Instant::now();
        let mut buffer = CaptureBuffer::new(1000);
        buffer.start(base);
        let (tx1, rx1) = mpsc::channel();
        buffer.stop(base + Duration::from_millis(20), tx1);
        buffer.start(base + Duration::from_millis(30));
        let (tx2, rx2) = mpsc::channel();
        buffer.stop(base + Duration::from_millis(50), tx2);
        buffer.samples(base, (0..100).map(|n| n as f32).collect());
        assert_eq!(
            rx1.recv().unwrap().samples,
            (0..20).map(|n| n as f32).collect::<Vec<_>>()
        );
        assert_eq!(
            rx2.recv().unwrap().samples,
            (30..50).map(|n| n as f32).collect::<Vec<_>>()
        );
    }

    #[test]
    fn release_after_a_gap_never_keeps_audio_after_release() {
        let base = Instant::now();
        let mut buffer = CaptureBuffer::new(1000);
        buffer.start(base);
        buffer.samples(base, vec![1.0; 10]);
        buffer.samples(base + Duration::from_millis(50), vec![2.0; 50]);
        let (tx, rx) = mpsc::channel();
        buffer.stop(base + Duration::from_millis(60), tx);
        let result = rx.recv().unwrap();
        assert_eq!(result.samples, [vec![1.0; 10], vec![2.0; 10]].concat());
        assert!(result.error.is_some());
    }

    #[test]
    fn long_recording_retains_every_sample() {
        let base = Instant::now();
        let mut buffer = CaptureBuffer::new(1000);
        buffer.start(base);
        for i in 0..6000 {
            buffer.samples(base + Duration::from_millis(i * 100), vec![i as f32; 100]);
        }
        let (tx, rx) = mpsc::channel();
        buffer.stop(base + Duration::from_secs(600), tx);
        let result = rx.recv().unwrap();
        assert_eq!(result.samples.len(), 600_000);
        assert_eq!(result.samples[599_999], 5999.0);
        assert!(result.error.is_none());
    }

    #[test]
    fn stalled_stream_finishes_with_partial_audio_instead_of_hanging() {
        let base = Instant::now() - Duration::from_secs(2);
        let mut buffer = CaptureBuffer::new(1000);
        buffer.start(base);
        buffer.samples(base, vec![1.0; 10]);
        let (tx, rx) = mpsc::channel();
        buffer.stop(base + Duration::from_millis(20), tx);
        buffer.tick();
        let result = rx.try_recv().unwrap();
        assert_eq!(result.samples.len(), 10);
        assert!(result.error.is_some());
    }

    #[test]
    fn stream_failure_preserves_partial_audio() {
        let base = Instant::now();
        let mut buffer = CaptureBuffer::new(1000);
        buffer.start(base);
        buffer.samples(base, vec![1.0; 10]);
        let (tx, rx) = mpsc::channel();
        buffer.stop(base + Duration::from_millis(20), tx);
        buffer.fail("device disconnected");
        let capture = rx.recv().unwrap();
        assert_eq!(capture.samples, vec![1.0; 10]);
        assert_eq!(capture.error.as_deref(), Some("device disconnected"));
    }
}

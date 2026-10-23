use rubato::{FftFixedIn, Resampler};
use std::time::Duration;

// Make this a constant you can tweak
const RESAMPLER_CHUNK_SIZE: usize = 1024;

pub struct FrameResampler {
    resampler: Option<FftFixedIn<f32>>,
    chunk_in: usize,
    in_buf: Vec<f32>,
    frame_samples: usize,
    pending: Vec<f32>,
}

impl FrameResampler {
    pub fn new(in_hz: usize, out_hz: usize, frame_dur: Duration) -> Self {
        let frame_samples = ((out_hz as f64 * frame_dur.as_secs_f64()).round()) as usize;
        assert!(frame_samples > 0, "frame duration too short");

        // Use fixed chunk size instead of GCD-based
        let chunk_in = RESAMPLER_CHUNK_SIZE;

        let resampler = (in_hz != out_hz).then(|| {
            FftFixedIn::<f32>::new(in_hz, out_hz, chunk_in, 1, 1)
                .expect("Failed to create resampler")
        });

        Self {
            resampler,
            chunk_in,
            in_buf: Vec::with_capacity(chunk_in),
            frame_samples,
            pending: Vec::with_capacity(frame_samples),
        }
    }

    pub fn push(&mut self, mut src: &[f32], mut emit: impl FnMut(&[f32])) {
        if self.resampler.is_none() {
            self.emit_frames(src, &mut emit);
            return;
        }

        while !src.is_empty() {
            let space = self.chunk_in - self.in_buf.len();
            let take = space.min(src.len());
            self.in_buf.extend_from_slice(&src[..take]);
            src = &src[take..];

            if self.in_buf.len() == self.chunk_in {
                // let start = std::time::Instant::now();
                if let Ok(out) = self
                    .resampler
                    .as_mut()
                    .unwrap()
                    .process(&[&self.in_buf[..]], None)
                {
                    // let duration = start.elapsed();
                    // log::debug!("Resampler took: {:?}", duration);
                    self.emit_frames(&out[0], &mut emit);
                }
                self.in_buf.clear();
            }
        }
    }

    pub fn finish(&mut self, mut emit: impl FnMut(&[f32])) {
        // Flush both the partial input and the FFT filter's delayed tail. Without
        // zero-input draining, speech at release can remain inside the filter.
        if self.resampler.is_some() {
            self.in_buf.resize(self.chunk_in, 0.0);
            let tail_blocks = {
                let resampler = self.resampler.as_ref().unwrap();
                resampler
                    .output_delay()
                    .div_ceil(resampler.output_frames_max())
                    + 1
            };
            for _ in 0..=tail_blocks {
                let output = self
                    .resampler
                    .as_mut()
                    .unwrap()
                    .process(&[&self.in_buf[..]], None);
                if let Ok(output) = output {
                    self.emit_frames(&output[0], &mut emit);
                }
                self.in_buf.fill(0.0);
            }
            self.in_buf.clear();
            self.resampler.as_mut().unwrap().reset();
        }

        // Emit any remaining pending frame (padded with zeros)
        if !self.pending.is_empty() {
            self.pending.resize(self.frame_samples, 0.0);
            emit(&self.pending);
            self.pending.clear();
        }
    }

    fn emit_frames(&mut self, mut data: &[f32], emit: &mut impl FnMut(&[f32])) {
        while !data.is_empty() {
            let space = self.frame_samples - self.pending.len();
            let take = space.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];

            if self.pending.len() == self.frame_samples {
                emit(&self.pending);
                self.pending.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_preserves_signal_at_end_of_full_input_block() {
        let mut resampler = FrameResampler::new(48000, 16000, Duration::from_millis(30));
        let mut input = vec![0.0; 1024];
        input[1000..].fill(1.0);
        let mut output = Vec::new();
        resampler.push(&input, |frame| output.extend_from_slice(frame));
        resampler.finish(|frame| output.extend_from_slice(frame));
        assert!(output.iter().any(|sample| *sample > 0.5));
    }
}

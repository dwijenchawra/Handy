use anyhow::Result;
use hound::{WavReader, WavSpec, WavWriter};
use log::debug;
use std::path::Path;

/// Read a WAV file and return normalised f32 samples.
pub fn read_wav_samples<P: AsRef<Path>>(file_path: P) -> Result<Vec<f32>> {
    let reader = WavReader::open(file_path.as_ref())?;
    let spec = reader.spec();
    anyhow::ensure!(spec.channels == 1, "Expected mono recording");
    let samples = if spec.sample_format == hound::SampleFormat::Float {
        reader
            .into_samples::<f32>()
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let scale = 2_f32.powi(spec.bits_per_sample as i32 - 1);
        reader
            .into_samples::<i32>()
            .map(|s| s.map(|v| v as f32 / scale))
            .collect::<Result<Vec<_>, _>>()?
    };
    if spec.sample_rate == 16000 {
        return Ok(samples);
    }
    let mut output = Vec::new();
    let mut resampler = super::FrameResampler::new(
        spec.sample_rate as usize,
        16000,
        std::time::Duration::from_millis(30),
    );
    resampler.push(&samples, |frame| output.extend_from_slice(frame));
    resampler.finish(|frame| output.extend_from_slice(frame));
    Ok(output)
}

/// Verify a WAV file by reading it back and checking the sample count.
pub fn verify_wav_file<P: AsRef<Path>>(file_path: P, expected_samples: usize) -> Result<()> {
    let reader = WavReader::open(file_path.as_ref())?;
    let actual_samples = reader.len() as usize;
    if actual_samples != expected_samples {
        anyhow::bail!(
            "WAV sample count mismatch: expected {}, got {}",
            expected_samples,
            actual_samples
        );
    }
    Ok(())
}

/// Save audio samples as a WAV file
pub fn save_wav_file<P: AsRef<Path>>(file_path: P, samples: &[f32]) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = WavWriter::create(file_path.as_ref(), spec)?;

    // Convert f32 samples to i16 for WAV
    for sample in samples {
        let sample_i16 = (sample * i16::MAX as f32) as i16;
        writer.write_sample(sample_i16)?;
    }

    writer.finalize()?;
    debug!("Saved WAV file: {:?}", file_path.as_ref());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_reads_original_float_recording_and_resamples_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original.wav");
        let mut writer = WavWriter::create(
            &path,
            WavSpec {
                channels: 1,
                sample_rate: 48000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for i in 0..1024 {
            writer
                .write_sample(if i >= 1000 { 1.0_f32 } else { 0.0 })
                .unwrap();
        }
        writer.finalize().unwrap();
        let samples = read_wav_samples(&path).unwrap();
        assert!(samples.iter().any(|sample| *sample > 0.5));
    }

    #[test]
    fn retry_still_reads_legacy_pcm_recordings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.wav");
        save_wav_file(&path, &[0.25, -0.5]).unwrap();
        let samples = read_wav_samples(&path).unwrap();
        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 0.25).abs() < 0.0001);
        assert!((samples[1] + 0.5).abs() < 0.0001);
    }
}

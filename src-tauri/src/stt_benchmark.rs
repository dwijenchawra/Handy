//! Independent remote benchmark lane; only the local lane pastes output.
use anyhow::Result;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};
use tauri::Manager;

static REMOTE_QUEUE: Lazy<crate::serial_queue::SerialQueue> =
    Lazy::new(|| crate::serial_queue::SerialQueue::new("remote-benchmark"));
pub fn remote_idle() -> bool {
    REMOTE_QUEUE.is_idle()
}

static REPORT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize, Deserialize, specta::Type)]
pub struct BenchmarkResult {
    recording: String,
    backend: String,
    model: String,
    queue_ms: f64,
    processing_ms: f64,
    e2e_ms: f64,
    audio_seconds: f64,
    cold_model: bool,
    #[serde(default)]
    streaming: bool,
    success: bool,
    error: Option<String>,
}

fn report_path(app: &tauri::AppHandle) -> Result<PathBuf> {
    Ok(app.path().app_data_dir()?.join("stt-benchmarks.jsonl"))
}

pub fn record(
    app: &tauri::AppHandle,
    recording: &str,
    backend: (&str, &str, bool),
    timing: (Instant, Instant, Instant),
    audio_seconds: f64,
    cold_model: bool,
    result: &Result<String>,
) {
    let (backend, model, streaming) = backend;
    let (released, started, finished) = timing;
    let row = BenchmarkResult {
        recording: recording.to_owned(),
        backend: backend.to_owned(),
        model: model.to_owned(),
        queue_ms: started.duration_since(released).as_secs_f64() * 1000.0,
        processing_ms: finished.duration_since(started).as_secs_f64() * 1000.0,
        e2e_ms: finished.duration_since(released).as_secs_f64() * 1000.0,
        audio_seconds,
        cold_model,
        streaming,
        success: result.is_ok(),
        error: result.as_ref().err().map(ToString::to_string),
    };
    let save = || -> Result<()> {
        let _guard = REPORT_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("Benchmark report lock poisoned"))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(report_path(app)?)?;
        serde_json::to_writer(&mut file, &row)?;
        writeln!(file)?;
        Ok(())
    };
    if let Err(error) = save() {
        log::error!("Could not save STT benchmark: {error}");
    }
}

fn retain_audio(path: &Path, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let retained = dir.join(
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("Missing recording filename"))?,
    );
    std::fs::hard_link(path, &retained).or_else(|_| std::fs::copy(path, &retained).map(|_| ()))?;
    Ok(retained)
}

pub fn enqueue_remote(
    app: tauri::AppHandle,
    path: PathBuf,
    endpoint: String,
    released: Instant,
    audio_seconds: f64,
    live: Option<std::sync::mpsc::Receiver<crate::live_capture::Outcome>>,
) {
    // Retain an independent hard link so local history cleanup cannot delete
    // audio that is still waiting in the slower remote queue.
    let retained = (|| -> Result<PathBuf> {
        let dir = app.path().app_data_dir()?.join("stt-benchmark-audio");
        retain_audio(&path, &dir)
    })();
    let retained = match retained {
        Ok(path) => path,
        Err(error) => {
            let now = Instant::now();
            record(
                &app,
                &path.file_name().unwrap_or_default().to_string_lossy(),
                ("remote", &endpoint, false),
                (released, now, now),
                audio_seconds,
                false,
                &Err(error),
            );
            return;
        }
    };
    REMOTE_QUEUE.submit(move || {
        let started = Instant::now();
        let live_result = crate::live_capture::usable_result(live);
        let streaming = live_result.is_some();
        let (result, finished) = match live_result {
            Some((text, finished)) => (Ok(text), finished),
            None => {
                let result = crate::audio_toolkit::read_wav_samples(&retained)
                    .and_then(|samples| crate::remote_stt::transcribe(&endpoint, &samples));
                (result, Instant::now())
            }
        };
        if let Err(error) = std::fs::remove_file(&retained) {
            log::warn!("Could not remove benchmark audio link: {error}");
        }
        let recording = path.file_name().unwrap_or_default().to_string_lossy();
        record(
            &app,
            &recording,
            ("remote", &endpoint, streaming),
            (
                released,
                if streaming { released } else { started },
                finished,
            ),
            audio_seconds,
            false,
            &result,
        );
    });
}

#[tauri::command]
#[specta::specta]
pub fn get_stt_benchmarks(app: tauri::AppHandle) -> Result<Vec<BenchmarkResult>, String> {
    let read = || -> Result<Vec<BenchmarkResult>> {
        let _guard = REPORT_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("Benchmark report lock poisoned"))?;
        let path = report_path(&app)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut rows = VecDeque::new();
        for line in std::io::BufReader::new(std::fs::File::open(path)?).lines() {
            if let Ok(row) = serde_json::from_str::<BenchmarkResult>(&line?) {
                rows.push_back(row);
                if rows.len() > 100 {
                    rows.pop_front();
                }
            }
        }
        Ok(rows.into_iter().collect())
    };
    read().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_remote_audio_survives_local_history_deletion() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("take.wav");
        std::fs::write(&original, b"original audio bytes").unwrap();
        let retained = retain_audio(&original, &root.path().join("remote")).unwrap();
        std::fs::remove_file(original).unwrap();
        assert_eq!(std::fs::read(&retained).unwrap(), b"original audio bytes");
        std::fs::remove_file(retained).unwrap();
    }
}

use crate::audio_toolkit::{list_input_devices, AudioRecorder};
use crate::helpers::clamshell;
use crate::settings::{get_settings, AppSettings};
use crate::utils;
use log::{debug, error, info};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::Emitter;

fn set_mute(mute: bool) {
    // Expected behavior:
    // - Windows: works on most systems using standard audio drivers.
    // - Linux: works on many systems (PipeWire, PulseAudio, ALSA),
    //   but some distros may lack the tools used.
    // - macOS: native CoreAudio, preserving the previous mute state.
    // If unsupported, fails silently.

    #[cfg(target_os = "windows")]
    {
        unsafe {
            use windows::Win32::{
                Media::Audio::{
                    eMultimedia, eRender, Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator,
                    MMDeviceEnumerator,
                },
                System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED},
            };

            macro_rules! unwrap_or_return {
                ($expr:expr) => {
                    match $expr {
                        Ok(val) => val,
                        Err(_) => return,
                    }
                };
            }

            // Initialize the COM library for this thread.
            // If already initialized (e.g., by another library like Tauri), this does nothing.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let all_devices: IMMDeviceEnumerator =
                unwrap_or_return!(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL));
            let default_device =
                unwrap_or_return!(all_devices.GetDefaultAudioEndpoint(eRender, eMultimedia));
            let volume_interface = unwrap_or_return!(
                default_device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            );

            let _ = volume_interface.SetMute(mute, std::ptr::null());
        }
    }

    #[cfg(target_os = "linux")]
    {
        use std::process::Command;

        let mute_val = if mute { "1" } else { "0" };
        let amixer_state = if mute { "mute" } else { "unmute" };

        // Try multiple backends to increase compatibility
        // 1. PipeWire (wpctl)
        if Command::new("wpctl")
            .args(["set-mute", "@DEFAULT_AUDIO_SINK@", mute_val])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }

        // 2. PulseAudio (pactl)
        if Command::new("pactl")
            .args(["set-sink-mute", "@DEFAULT_SINK@", mute_val])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }

        // 3. ALSA (amixer)
        let _ = Command::new("amixer")
            .args(["set", "Master", amixer_state])
            .output();
    }

    #[cfg(target_os = "macos")]
    {
        if let Err(error) = crate::macos_mute::set_muted(mute) {
            error!("{error}");
        }
    }
}

/* ──────────────────────────────────────────────────────────────── */

#[derive(Clone, Debug)]
pub enum RecordingState {
    Idle,
    Recording { binding_id: String },
}

#[derive(Clone, Debug)]
pub enum MicrophoneMode {
    AlwaysOn,
    OnDemand,
}

/* ──────────────────────────────────────────────────────────────── */

fn create_audio_recorder(app_handle: &tauri::AppHandle) -> Result<AudioRecorder, anyhow::Error> {
    // Preserve speech unconditionally. VAD must not destroy the recoverable recording.
    let recorder = AudioRecorder::new()
        .map_err(|e| anyhow::anyhow!("Failed to create AudioRecorder: {}", e))?
        .with_error_callback({
            let app_handle = app_handle.clone();
            move |error| {
                let _ = app_handle.emit(
                    "recording-error",
                    serde_json::json!({ "error_type": "unknown", "detail": error }),
                );
            }
        })
        .with_level_callback({
            let app_handle = app_handle.clone();
            move |levels| {
                utils::emit_levels(&app_handle, &levels);
            }
        });

    Ok(recorder)
}

/* ──────────────────────────────────────────────────────────────── */

#[derive(Clone)]
pub struct AudioRecordingManager {
    state: Arc<Mutex<RecordingState>>,
    app_handle: tauri::AppHandle,

    recorder: Arc<Mutex<Option<AudioRecorder>>>,
    is_open: Arc<Mutex<bool>>,
    is_recording: Arc<Mutex<bool>>,
    did_mute: Arc<Mutex<bool>>,
}

impl AudioRecordingManager {
    /* ---------- construction ------------------------------------------------ */

    pub fn new(app: &tauri::AppHandle) -> Result<Self, anyhow::Error> {
        let settings = get_settings(app);
        let mode = if settings.always_on_microphone {
            MicrophoneMode::AlwaysOn
        } else {
            MicrophoneMode::OnDemand
        };

        let manager = Self {
            state: Arc::new(Mutex::new(RecordingState::Idle)),
            app_handle: app.clone(),

            recorder: Arc::new(Mutex::new(None)),
            is_open: Arc::new(Mutex::new(false)),
            is_recording: Arc::new(Mutex::new(false)),
            did_mute: Arc::new(Mutex::new(false)),
        };

        // Always-on?  Open immediately.
        if matches!(mode, MicrophoneMode::AlwaysOn) {
            manager.start_microphone_stream()?;
        }

        Ok(manager)
    }

    /* ---------- helper methods --------------------------------------------- */

    fn get_effective_microphone_device(&self, settings: &AppSettings) -> Option<cpal::Device> {
        // Check if we're in clamshell mode and have a clamshell microphone configured
        let use_clamshell_mic = if let Ok(is_clamshell) = clamshell::is_clamshell() {
            is_clamshell && settings.clamshell_microphone.is_some()
        } else {
            false
        };

        let device_name = if use_clamshell_mic {
            settings.clamshell_microphone.as_ref().unwrap()
        } else {
            settings.selected_microphone.as_ref()?
        };

        // Find the device by name
        match list_input_devices() {
            Ok(devices) => devices
                .into_iter()
                .find(|d| d.name == *device_name)
                .map(|d| d.device),
            Err(e) => {
                debug!("Failed to list devices, using default: {}", e);
                None
            }
        }
    }

    /* ---------- microphone life-cycle -------------------------------------- */

    /// Applies mute if mute_while_recording is enabled and stream is open
    pub fn apply_mute(&self) {
        let settings = get_settings(&self.app_handle);
        let is_open = *self.is_open.lock().unwrap();
        let mut did_mute_guard = self.did_mute.lock().unwrap();

        if settings.mute_while_recording && is_open {
            set_mute(true);
            *did_mute_guard = true;
            debug!("Mute applied");
        }
    }

    /// Removes mute if it was applied
    pub fn remove_mute(&self) {
        let mut did_mute_guard = self.did_mute.lock().unwrap();
        if *did_mute_guard {
            set_mute(false);
            *did_mute_guard = false;
            debug!("Mute removed");
        }
    }

    pub fn prepare_recorder(&self) -> Result<(), anyhow::Error> {
        let mut recorder_opt = self.recorder.lock().unwrap();
        if recorder_opt.is_none() {
            *recorder_opt = Some(create_audio_recorder(&self.app_handle)?);
        }
        Ok(())
    }

    pub fn start_microphone_stream(&self) -> Result<(), anyhow::Error> {
        let mut open_flag = self.is_open.lock().unwrap();
        if *open_flag {
            let mut recorder = self.recorder.lock().unwrap();
            if recorder
                .as_ref()
                .is_some_and(|recorder| recorder.is_healthy())
            {
                return Ok(());
            }
            // Re-arm a disconnected/stalled stream on the next attempt. The
            // timestamped capture reports any startup interval it could not cover.
            if let Some(recorder) = recorder.as_mut() {
                recorder
                    .close()
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            }
            *open_flag = false;
        }

        let start_time = Instant::now();

        // The capture action controls mute after arming its interval.
        let mut did_mute_guard = self.did_mute.lock().unwrap();
        *did_mute_guard = false;

        // Get the selected device from settings, considering clamshell mode
        let settings = get_settings(&self.app_handle);
        let selected_device = self.get_effective_microphone_device(&settings);

        // Pre-flight check: if no device was selected/configured AND no devices
        // exist at all, fail early with a clear error instead of letting cpal
        // produce a cryptic backend-specific message.
        if selected_device.is_none() {
            let has_any_device = list_input_devices()
                .map(|devices| !devices.is_empty())
                .unwrap_or(false);
            if !has_any_device {
                return Err(anyhow::anyhow!("No input device found"));
            }
        }

        // Initialize the recorder without running inference on the capture path.
        self.prepare_recorder()?;

        let mut recorder_opt = self.recorder.lock().unwrap();
        if let Some(rec) = recorder_opt.as_mut() {
            rec.open(selected_device)
                .map_err(|e| anyhow::anyhow!("Failed to open recorder: {}", e))?;
        }

        *open_flag = true;
        // The recorder has received its first hardware buffer before returning.
        info!(
            "Microphone stream initialized in {:?}",
            start_time.elapsed()
        );
        Ok(())
    }

    pub fn stop_microphone_stream(&self) {
        let mut open_flag = self.is_open.lock().unwrap();
        if !*open_flag {
            return;
        }

        let mut did_mute_guard = self.did_mute.lock().unwrap();
        if *did_mute_guard {
            set_mute(false);
        }
        *did_mute_guard = false;

        if let Some(rec) = self.recorder.lock().unwrap().as_mut() {
            // If still recording, stop first.
            if *self.is_recording.lock().unwrap() {
                let _ = rec.stop();
                *self.is_recording.lock().unwrap() = false;
            }
            let _ = rec.close();
        }

        *open_flag = false;
        debug!("Microphone stream stopped");
    }

    /* ---------- mode switching --------------------------------------------- */

    pub fn update_mode(&self, _new_mode: MicrophoneMode) -> Result<(), anyhow::Error> {
        // Once shortcuts are armed, the input stream stays warm for reliable edges.
        self.start_microphone_stream()?;
        Ok(())
    }

    /* ---------- recording --------------------------------------------------- */

    pub fn try_start_recording_at(&self, binding_id: &str, at: Instant) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();

        if let RecordingState::Idle = *state {
            if let Err(error) = self.start_microphone_stream() {
                error!("Failed to arm microphone: {error}");
                return Err(error.to_string());
            }

            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                match rec.start_at(at) {
                    Ok(()) => {
                        *self.is_recording.lock().unwrap() = true;
                        *state = RecordingState::Recording {
                            binding_id: binding_id.to_string(),
                        };
                        debug!("Recording started for binding {binding_id}");
                        return Ok(());
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err("Recorder not available".to_string())
        } else {
            Err("Already recording".to_string())
        }
    }

    pub fn update_selected_device(&self) -> Result<(), anyhow::Error> {
        let state = self.state.lock().unwrap();
        anyhow::ensure!(
            matches!(*state, RecordingState::Idle),
            "Finish the current recording before changing microphone"
        );
        // If currently open, restart the microphone stream to use the new device
        if *self.is_open.lock().unwrap() {
            self.stop_microphone_stream();
            self.start_microphone_stream()?;
        }
        Ok(())
    }

    /// Seal the interval immediately; collecting its final device buffer happens
    /// off the shortcut thread, so a second press can start another capture.
    pub fn finish_recording_at(
        &self,
        binding_id: &str,
        at: Instant,
    ) -> Result<std::sync::mpsc::Receiver<crate::audio_toolkit::audio::CapturedAudio>, String> {
        let mut state = self.state.lock().unwrap();
        if !matches!(&*state, RecordingState::Recording { binding_id: active } if active == binding_id)
        {
            return Err("No matching active recording".into());
        }
        let recorder = self.recorder.lock().unwrap();
        let result = recorder
            .as_ref()
            .ok_or("Recorder not available")?
            .stop_at(at)
            .map_err(|e| e.to_string());
        *state = RecordingState::Idle;
        *self.is_recording.lock().unwrap() = false;
        // Keep the stream open: pending tails and subsequent presses share it.
        result
    }

    pub fn is_recording(&self) -> bool {
        matches!(
            *self.state.lock().unwrap(),
            RecordingState::Recording { .. }
        )
    }

    /// Cancel any ongoing recording without returning audio samples
    pub fn cancel_recording(&self) {
        let mut state = self.state.lock().unwrap();

        if let RecordingState::Recording { .. } = *state {
            *state = RecordingState::Idle;
            drop(state);

            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                let _ = rec.stop_at(Instant::now()); // Discard without blocking shortcut handling
            }

            *self.is_recording.lock().unwrap() = false;

            self.remove_mute();
        }
    }
}

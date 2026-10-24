#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::apple_intelligence;
use crate::audio_feedback::{play_feedback_sound, SoundType};
use crate::audio_toolkit::{is_microphone_access_denied, is_no_input_device_error};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::history::HistoryManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{get_settings, AppSettings, APPLE_INTELLIGENCE_PROVIDER_ID};
use crate::shortcut;
use crate::tray::TrayIconState;
use crate::utils::{self, show_recording_overlay, show_transcribing_overlay};
use crate::TranscriptionCoordinator;
use log::{debug, error, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tauri::Manager;
use tauri::{AppHandle, Emitter};

#[derive(Clone, serde::Serialize)]
struct RecordingErrorEvent {
    error_type: String,
    detail: Option<String>,
}

/// Drop guard that notifies the [`TranscriptionCoordinator`] when the
/// transcription pipeline finishes — whether it completes normally or panics.
struct FinishGuard(AppHandle);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        if let Some(c) = self.0.try_state::<TranscriptionCoordinator>() {
            c.notify_processing_finished();
        }
    }
}

// Shortcut Action Trait
pub trait ShortcutAction: Send + Sync {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
    fn start_at(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str, _at: Instant) {
        self.start(app, binding_id, shortcut_str);
    }
    fn stop_at(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str, _at: Instant) {
        self.stop(app, binding_id, shortcut_str);
    }
}

// Transcribe Action
struct TranscribeAction {
    post_process: bool,
}

/// Field name for structured output JSON schema
const TRANSCRIPTION_FIELD: &str = "transcription";

/// Strip invisible Unicode characters that some LLMs may insert
fn strip_invisible_chars(s: &str) -> String {
    s.replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'], "")
}

fn strip_think_block(s: &str) -> &str {
    if let Some(rest) = s.trim_start().strip_prefix("<think>") {
        if let Some(end) = rest.find("</think>") {
            return rest[end + "</think>".len()..].trim_start();
        }
    }
    s
}

/// Build a system prompt from the user's prompt template.
/// Replaces `${output}` with empty string (transcription is sent as user message).
/// Replaces `${screen_context}` with captured screen context, or removes it if none.
fn build_system_prompt(prompt_template: &str, screen_context: Option<&str>) -> String {
    let ctx_placeholder = "${screen_context}";
    let has_placeholder = prompt_template.contains(ctx_placeholder);
    let with_ctx = match screen_context {
        Some(ctx) if !ctx.is_empty() => prompt_template.replace(
            ctx_placeholder,
            &format!(
                "--- Screen Context ---\n{}\n--- End Screen Context ---",
                ctx
            ),
        ),
        _ if has_placeholder => prompt_template.replace(ctx_placeholder, "(no screen context)"),
        _ => prompt_template.to_string(),
    };
    with_ctx.replace("${output}", "").trim().to_string()
}

async fn post_process_transcription(
    settings: &AppSettings,
    transcription: &str,
    screen_context: Option<&str>,
) -> Option<String> {
    let provider = match settings.active_post_process_provider().cloned() {
        Some(provider) => provider,
        None => {
            debug!("Post-processing enabled but no provider is selected");
            return None;
        }
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    if model.trim().is_empty() {
        debug!(
            "Post-processing skipped because provider '{}' has no model configured",
            provider.id
        );
        return None;
    }

    let selected_prompt_id = match &settings.post_process_selected_prompt_id {
        Some(id) => id.clone(),
        None => {
            debug!("Post-processing skipped because no prompt is selected");
            return None;
        }
    };

    let prompt = match settings
        .post_process_prompts
        .iter()
        .find(|prompt| prompt.id == selected_prompt_id)
    {
        Some(prompt) => prompt.prompt.clone(),
        None => {
            debug!(
                "Post-processing skipped because prompt '{}' was not found",
                selected_prompt_id
            );
            return None;
        }
    };

    if prompt.trim().is_empty() {
        debug!("Post-processing skipped because the selected prompt is empty");
        return None;
    }

    debug!(
        "Starting LLM post-processing with provider '{}' (model: {})",
        provider.id, model
    );

    let api_key = settings
        .post_process_api_keys
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    // Disable reasoning for providers where post-processing rarely benefits from it.
    // - custom: top-level reasoning_effort (works for local OpenAI-compat servers)
    // - openrouter: nested reasoning object; exclude:true also keeps reasoning text
    //   out of the response so it can't pollute structured-output JSON parsing
    let disable_reasoning = matches!(provider.id.as_str(), "custom" | "openrouter");

    if provider.supports_structured_output {
        debug!("Using structured outputs for provider '{}'", provider.id);

        let system_prompt = build_system_prompt(&prompt, screen_context);
        let user_content = transcription.to_string();

        // Handle Apple Intelligence separately since it uses native Swift APIs
        if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                if !apple_intelligence::check_apple_intelligence_availability() {
                    debug!(
                        "Apple Intelligence selected but not currently available on this device"
                    );
                    return None;
                }

                let token_limit = model.trim().parse::<i32>().unwrap_or(0);
                return match apple_intelligence::process_text_with_system_prompt(
                    &system_prompt,
                    &user_content,
                    token_limit,
                ) {
                    Ok(result) => {
                        if result.trim().is_empty() {
                            debug!("Apple Intelligence returned an empty response");
                            None
                        } else {
                            let result = strip_invisible_chars(&result);
                            debug!(
                                "Apple Intelligence post-processing succeeded. Output length: {} chars",
                                result.len()
                            );
                            Some(result)
                        }
                    }
                    Err(err) => {
                        error!("Apple Intelligence post-processing failed: {}", err);
                        None
                    }
                };
            }

            #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
            {
                debug!("Apple Intelligence provider selected on unsupported platform");
                return None;
            }
        }

        // Define JSON schema for transcription output
        let json_schema = serde_json::json!({
            "type": "object",
            "properties": {
                (TRANSCRIPTION_FIELD): {
                    "type": "string",
                    "description": "The cleaned and processed transcription text"
                }
            },
            "required": [TRANSCRIPTION_FIELD],
            "additionalProperties": false
        });

        match crate::llm_client::send_chat_completion_with_schema(
            &provider,
            api_key.clone(),
            &model,
            user_content,
            Some(system_prompt),
            Some(json_schema),
            disable_reasoning,
        )
        .await
        {
            Ok(Some(content)) => {
                let content = strip_think_block(&content);
                // Parse the JSON response to extract the transcription field
                match serde_json::from_str::<serde_json::Value>(content) {
                    Ok(json) => {
                        if let Some(transcription_value) =
                            json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str())
                        {
                            let result = strip_invisible_chars(transcription_value);
                            debug!(
                                "Structured output post-processing succeeded for provider '{}'. Output length: {} chars",
                                provider.id,
                                result.len()
                            );
                            return Some(result);
                        } else {
                            error!("Structured output response missing 'transcription' field");
                            return Some(strip_invisible_chars(content));
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to parse structured output JSON: {}. Returning raw content.",
                            e
                        );
                        return Some(strip_invisible_chars(content));
                    }
                }
            }
            Ok(None) => {
                error!("LLM API response has no content");
                return None;
            }
            Err(e) => {
                warn!(
                    "Structured output failed for provider '{}': {}. Falling back to legacy mode.",
                    provider.id, e
                );
                // Fall through to legacy mode below
            }
        }
    }

    // Legacy mode: Replace ${output} and ${screen_context} variables
    let mut processed_prompt = prompt.replace("${output}", transcription);
    processed_prompt = match screen_context {
        Some(ctx) if !ctx.is_empty() => processed_prompt.replace(
            "${screen_context}",
            &format!(
                "--- Screen Context ---\n{}\n--- End Screen Context ---",
                ctx
            ),
        ),
        _ => processed_prompt.replace("${screen_context}", "(no screen context)"),
    };
    debug!("Processed prompt length: {} chars", processed_prompt.len());

    match crate::llm_client::send_chat_completion(
        &provider,
        api_key,
        &model,
        processed_prompt,
        disable_reasoning,
    )
    .await
    {
        Ok(Some(content)) => {
            let content = strip_invisible_chars(strip_think_block(&content));
            debug!(
                "LLM post-processing succeeded for provider '{}'. Output length: {} chars",
                provider.id,
                content.len()
            );
            Some(content)
        }
        Ok(None) => {
            error!("LLM API response has no content");
            None
        }
        Err(e) => {
            error!(
                "LLM post-processing failed for provider '{}': {}. Falling back to original transcription.",
                provider.id,
                e
            );
            None
        }
    }
}

pub(crate) struct ProcessedTranscription {
    pub final_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
}

pub(crate) async fn process_transcription_output(
    app: &AppHandle,
    transcription: &str,
    post_process: bool,
) -> ProcessedTranscription {
    if transcription.trim().is_empty() {
        return ProcessedTranscription {
            final_text: String::new(),
            post_processed_text: None,
            post_process_prompt: None,
        };
    }
    let settings = get_settings(app);
    let mut final_text = transcription.to_string();
    let mut post_processed_text: Option<String> = None;
    let mut post_process_prompt: Option<String> = None;

    // Capture screen context via accessibility API (macOS only).
    // This fires on every transcribe when the toggle is on (not just post-process),
    // so you can verify capture in logs even without an LLM provider configured.
    let screen_context: Option<String> = if settings.use_accessibility_context {
        #[cfg(target_os = "macos")]
        {
            let ctx = crate::accessibility::get_focused_ui_text();
            match &ctx {
                Some(text) => debug!(
                    "Accessibility context captured ({} chars):\n---\n{}\n---",
                    text.len(),
                    text
                ),
                None => debug!("Accessibility context: no focused element or permissions denied"),
            }
            ctx
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = &settings;
            warn!("Accessibility context is only supported on macOS");
            None
        }
    } else {
        None
    };

    if post_process {
        if let Some(processed_text) =
            post_process_transcription(&settings, &final_text, screen_context.as_deref()).await
        {
            post_processed_text = Some(processed_text.clone());
            final_text = processed_text;

            if let Some(prompt_id) = &settings.post_process_selected_prompt_id {
                if let Some(prompt) = settings
                    .post_process_prompts
                    .iter()
                    .find(|prompt| &prompt.id == prompt_id)
                {
                    post_process_prompt = Some(prompt.prompt.clone());
                }
            }
        }
    } else if final_text != transcription {
        post_processed_text = Some(final_text.clone());
    }

    ProcessedTranscription {
        final_text,
        post_processed_text,
        post_process_prompt,
    }
}

static CAPTURE_QUEUE: Lazy<crate::serial_queue::SerialQueue> =
    Lazy::new(|| crate::serial_queue::SerialQueue::new("dictation-persistence"));
pub(crate) static PROCESSING_QUEUE: Lazy<crate::serial_queue::SerialQueue> =
    Lazy::new(|| crate::serial_queue::SerialQueue::new("dictation-inference"));
static RECORDING_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl ShortcutAction for TranscribeAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        self.start_at(app, binding_id, shortcut_str, Instant::now());
    }

    fn start_at(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str, at: Instant) {
        let start_time = Instant::now();
        let rm = app.state::<Arc<AudioRecordingManager>>();
        // Capture is always the first operation. Model loading belongs to the
        // processing worker, since the previous job may currently own the engine.
        let recording_error = rm.try_start_recording_at(binding_id, at).err();
        if recording_error.is_none() {
            rm.apply_mute();
            crate::tray::set_tray_state(app, TrayIconState::Recording);
            let settings = get_settings(app);
            let streaming = !settings.remote_stt_enabled
                && !app.state::<TranscriptionCoordinator>().has_pending()
                && app
                    .state::<Arc<crate::managers::model::ModelManager>>()
                    .get_model_info(&settings.selected_model)
                    .is_some_and(|model| model.supports_streaming);
            if streaming {
                crate::overlay::show_streaming_overlay(app);
            } else {
                show_recording_overlay(app);
            }
            crate::overlay::emit_recording_ready(app);
            if !settings.mute_while_recording {
                play_feedback_sound(app, SoundType::Start);
            }
        }

        if recording_error.is_none() {
            // Dynamically register the cancel shortcut in a separate task to avoid deadlock
            shortcut::register_cancel_shortcut(app);
        } else {
            // Starting failed (for example due to blocked microphone permissions).
            // Revert UI state so we don't stay stuck in the recording overlay.
            utils::hide_recording_overlay(app);
            crate::tray::set_tray_state(app, TrayIconState::Idle);
            if let Some(err) = recording_error {
                let error_type = if is_microphone_access_denied(&err) {
                    "microphone_permission_denied"
                } else if is_no_input_device_error(&err) {
                    "no_input_device"
                } else {
                    "unknown"
                };
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: error_type.to_string(),
                        detail: Some(err),
                    },
                );
            }
        }

        debug!(
            "TranscribeAction::start completed in {:?}",
            start_time.elapsed()
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        self.stop_at(app, binding_id, shortcut_str, Instant::now());
    }

    fn stop_at(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str, at: Instant) {
        let rm = app.state::<Arc<AudioRecordingManager>>();
        // Seal before UI, unmuting, or any disk/model work. The receiver includes
        // the final hardware buffer even if it arrives after the key release.
        let capture = match rm.finish_recording_at(binding_id, at) {
            Ok(capture) => capture,
            Err(error) => {
                error!("Failed to finish capture: {error}");
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: "unknown".into(),
                        detail: Some(error),
                    },
                );
                return;
            }
        };
        app.state::<TranscriptionCoordinator>().begin_processing();
        let guard = FinishGuard(app.clone());
        shortcut::unregister_cancel_shortcut(app);
        rm.remove_mute();
        play_feedback_sound(app, SoundType::Stop);
        crate::tray::set_tray_state(app, TrayIconState::Transcribing);
        show_transcribing_overlay(app);

        let ah = app.clone();
        let post_process = self.post_process;
        let benchmark_settings = crate::settings::get_settings(app);
        let compare = benchmark_settings.remote_stt_compare;
        CAPTURE_QUEUE.submit(move || {
            let mut capture = match capture.recv() {
                Ok(capture) => capture,
                Err(error) => {
                    error!("Capture worker disconnected: {error}");
                    let _ = ah.emit("recording-error", RecordingErrorEvent { error_type: "unknown".into(), detail: Some(error.to_string()) });
                    return;
                }
            };
            if let Some(error) = &capture.error {
                error!("Incomplete recording: {error}");
                let _ = ah.emit("recording-error", RecordingErrorEvent { error_type: "unknown".into(), detail: Some(error.clone()) });
            }
            if capture.samples.is_empty() { return; }
            let hm = Arc::clone(&ah.state::<Arc<HistoryManager>>());
            let id = RECORDING_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let file_name = format!("handy-{}-{id}.wav", chrono::Utc::now().timestamp_micros());
            let wav_path = hm.recordings_dir().join(&file_name);
            // Save original samples before resampling, VAD, or waiting for inference.
            let saved = save_original_audio(&wav_path, &capture.samples, capture.sample_rate);
            let audio_saved = saved.is_ok();
            let audio_seconds = capture.samples.len() as f64 / capture.sample_rate as f64;
            let benchmark_recording = file_name.clone();
            let mut live = capture.live.take();
            let valid_live = live.as_ref().is_some_and(|live| live.valid());
            let local_live = live.as_mut().and_then(|live| live.local.take()).filter(|_| valid_live);
            let mut remote_live = live.as_mut().and_then(|live| live.remote.take()).filter(|_| valid_live);
            if compare && audio_saved {
                crate::stt_benchmark::enqueue_remote(ah.clone(), wav_path.clone(), benchmark_settings.remote_stt_url.clone(), at, audio_seconds, remote_live.take());
            }
            let history_entry = match saved {
                Ok(()) => hm.save_pending_entry(file_name, post_process).map_err(|error| {
                    error!("Failed to register recoverable recording: {error}"); error
                }).ok(),
                Err(error) => {
                    error!("Failed to preserve recording: {error}");
                    let _ = ah.emit("recording-error", RecordingErrorEvent { error_type: "unknown".into(), detail: Some(error.to_string()) });
                    None
                }
            };
            let fallback = if audio_saved { None } else { Some(capture) };
            PROCESSING_QUEUE.submit(move || {
                let benchmark_started = Instant::now();
                let _guard = guard;
                tauri::async_runtime::block_on(async move {
                    let samples = if let Some(capture) = fallback {
                        let mut samples = Vec::new();
                        let mut resampler = crate::audio_toolkit::audio::FrameResampler::new(capture.sample_rate as usize, 16000, std::time::Duration::from_millis(30));
                        resampler.push(&capture.samples, |frame| samples.extend_from_slice(frame));
                        resampler.finish(|frame| samples.extend_from_slice(frame));
                        samples
                    } else {
                        match crate::audio_toolkit::read_wav_samples(&wav_path) {
                            Ok(samples) => samples,
                            Err(error) => {
                                error!("Failed to read queued audio: {error}");
                                let _ = ah.emit("transcription-error", error.to_string());
                                return;
                            }
                        }
                    };
                    let tm = Arc::clone(&ah.state::<Arc<TranscriptionManager>>());
                    let cold_model = !tm.is_model_loaded();
                    let live_result = crate::live_capture::usable_result(if compare || !benchmark_settings.remote_stt_enabled { local_live } else { remote_live });
                    let live_finished = live_result.as_ref().map(|(_, finished)| *finished);
                    let transcription_result = if let Some((text, _)) = live_result {
                        Ok(text)
                    } else if compare {
                        tm.initiate_local_model_load();
                        tm.transcribe_local(samples)
                    } else {
                        tm.initiate_model_load();
                        tm.transcribe(samples)
                    };
                    if compare {
                        crate::stt_benchmark::record(&ah, &benchmark_recording, ("local", &benchmark_settings.selected_model, live_finished.is_some()), (at, if live_finished.is_some() { at } else { benchmark_started }, live_finished.unwrap_or_else(Instant::now)), audio_seconds, cold_model, &transcription_result);
                    }
                    match transcription_result {
                        Ok(transcription) => {
                            let processed = process_transcription_output(&ah, &transcription, post_process).await;
                            if let Some(entry) = &history_entry {
                                if let Err(error) = hm.update_transcription(entry.id, transcription, processed.post_processed_text.clone(), processed.post_process_prompt.clone()) {
                                    error!("Failed to update transcription history: {error}");
                                } else if !processed.final_text.is_empty() {
                                    if let Err(error) = hm.release_pending_entry(entry.id) {
                                        error!("Failed to release pending recording retention: {error}");
                                    }
                                }
                            }
                            if !processed.final_text.is_empty() {
                                if let Err(error) = crate::delivery::enqueue(ah.clone(), benchmark_recording.clone(), processed.final_text) {
                                    error!("Could not queue delivery; transcript remains in History: {error}");
                                    let _ = ah.emit("paste-error", ());
                                }
                            }
                        }
                        Err(error) => {
                            error!("Transcription failed; original recording retained for retry: {error}");
                            let _ = ah.emit("transcription-error", error.to_string());
                        }
                    }
                });
            });
        });
    }
}

fn save_original_audio(
    path: &std::path::Path,
    samples: &[f32],
    sample_rate: u32,
) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &sample in samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

// Cancel Action
struct CancelAction;

impl ShortcutAction for CancelAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        utils::cancel_current_operation(app);
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // Nothing to do on stop for cancel
    }
}

// Test Action
struct TestAction;

impl ShortcutAction for TestAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Started - {} (App: {})", // Changed "Pressed" to "Started" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Stopped - {} (App: {})", // Changed "Released" to "Stopped" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }
}

// Static Action Map
pub static ACTION_MAP: Lazy<HashMap<String, Arc<dyn ShortcutAction>>> = Lazy::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "transcribe".to_string(),
        Arc::new(TranscribeAction {
            post_process: false,
        }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "transcribe_with_post_process".to_string(),
        Arc::new(TranscribeAction { post_process: true }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "cancel".to_string(),
        Arc::new(CancelAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "test".to_string(),
        Arc::new(TestAction) as Arc<dyn ShortcutAction>,
    );
    map
});

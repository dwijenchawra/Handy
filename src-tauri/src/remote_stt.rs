//! Remote recognition runs only on the serial processing worker, after capture is saved.
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tauri::command]
#[specta::specta]
pub fn set_remote_stt(
    app: tauri::AppHandle,
    enabled: bool,
    compare: bool,
    url: String,
) -> Result<(), String> {
    let url = url.trim().to_string();
    if (enabled || compare) && !url.starts_with("wss://") {
        return Err("Remote STT requires a wss:// URL".into());
    }
    let mut settings = crate::settings::get_settings(&app);
    settings.remote_stt_enabled = enabled && !compare;
    settings.remote_stt_compare = compare;
    settings.remote_stt_url = url;
    crate::settings::write_settings(&app, settings);
    Ok(())
}

static REMOTE_SESSION: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn transcribe(url: &str, audio: &[f32]) -> Result<String> {
    // Switching modes while a benchmark is pending must not open two sessions.
    let _session = REMOTE_SESSION
        .lock()
        .map_err(|_| anyhow::anyhow!("Remote session lock poisoned"))?;
    // The caller may already be inside Tauri's runtime. Use an isolated runtime
    // rather than nesting block_on or blocking a runtime executor with socket IO.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(transcribe_async(url, audio))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("Remote STT worker panicked"))?
    })
}

async fn transcribe_async(url: &str, audio: &[f32]) -> Result<String> {
    if audio.is_empty() {
        return Ok(String::new());
    }
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), connect_async(url))
        .await
        .context("Remote STT connection timed out")??;
    let ready = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(message) = socket.next().await {
            if let Message::Text(text) = message? {
                return Ok::<Value, anyhow::Error>(serde_json::from_str(&text)?);
            }
        }
        bail!("Remote STT closed before ready")
    })
    .await
    .context("Remote STT ready timed out")??;
    if let Some(error) = ready.get("error") {
        bail!("Remote STT: {error}. Recording retained; retry from History.");
    }
    if ready.get("ready").and_then(Value::as_bool) != Some(true)
        || ready.get("sample_rate").and_then(Value::as_u64) != Some(16000)
    {
        bail!("Remote STT returned an incompatible ready message");
    }
    let (mut sink, mut stream) = socket.split();
    let send = async {
        // 100 ms of mono float32 PCM: 6,400 bytes, safely below the 64 KB limit.
        // Completed audio can be uploaded immediately. Socket backpressure
        // bounds transmission without adding the recording duration as latency.
        for chunk in audio.chunks(1600) {
            let bytes: Vec<u8> = chunk
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect();
            sink.send(Message::Binary(bytes.into())).await?;
        }
        sink.send(Message::Text("stop".into())).await?;
        Ok::<(), anyhow::Error>(())
    };
    let receive = async {
        let mut final_text = None;
        while let Some(message) = stream.next().await {
            if let Message::Text(text) = message? {
                let event: Value = serde_json::from_str(&text)?;
                if let Some(error) = event.get("error") {
                    bail!("Remote STT: {error}");
                }
                if event.get("final").and_then(Value::as_bool) == Some(true) {
                    final_text = Some(
                        event
                            .get("text")
                            .and_then(Value::as_str)
                            .context("Remote STT final response has no text")?
                            .to_owned(),
                    );
                }
                if event.get("done").and_then(Value::as_bool) == Some(true) {
                    return final_text.context("Remote STT finished without a final transcript");
                }
                // Partials replace earlier partials; none are pasted or saved as final.
            }
        }
        bail!("Remote STT disconnected before completion; recording retained")
    };
    let deadline = Duration::from_secs_f64(audio.len() as f64 / 16000.0 + 120.0);
    let (_, text) = tokio::time::timeout(deadline, async { tokio::try_join!(send, receive) })
        .await
        .context("Remote STT timed out; recording retained")??;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_protocol_preserves_pcm_and_requires_final_and_done() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for (responses, expected) in [
                (
                    vec![
                        r#"{"text":"wrong partial"}"#,
                        r#"{"text":"correct final","final":true}"#,
                        r#"{"done":true}"#,
                    ],
                    Some("correct final"),
                ),
                (vec![r#"{"text":"only partial"}"#, r#"{"done":true}"#], None),
                (vec![r#"{"text":"incomplete","final":true}"#], None),
                (vec![r#"{"error":"busy"}"#], None),
            ] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("ws://{}", listener.local_addr().unwrap());
                let audio: Vec<f32> = (0..1703).map(|i| i as f32 / 1703.0).collect();
                let expected_bytes: Vec<u8> = audio.iter().flat_map(|x| x.to_le_bytes()).collect();
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    ws.send(Message::Text(
                        r#"{"ready":true,"sample_rate":16000}"#.into(),
                    ))
                    .await
                    .unwrap();
                    let mut bytes = Vec::new();
                    while let Some(message) = ws.next().await {
                        match message.unwrap() {
                            Message::Binary(chunk) => {
                                assert!(chunk.len() <= 64000);
                                bytes.extend_from_slice(&chunk);
                            }
                            Message::Text(text) => {
                                assert_eq!(text, "stop");
                                break;
                            }
                            _ => {}
                        }
                    }
                    assert_eq!(bytes, expected_bytes);
                    for response in responses {
                        ws.send(Message::Text(response.into())).await.unwrap();
                    }
                    ws.close(None).await.unwrap();
                });
                let result = transcribe_async(&url, &audio).await;
                match expected {
                    Some(text) => assert_eq!(result.unwrap(), text),
                    None => assert!(result.is_err()),
                }
                server.await.unwrap();
            }
        });
    }

    // Explicit opt-in: sends synthetic silence, never a user's saved recording.
    #[test]
    #[ignore]
    fn live_server_silence() {
        let result = transcribe("wss://stt.dwijen.dev/stream", &vec![0.0; 16000]);
        assert!(result.is_ok(), "{result:?}");
    }
}

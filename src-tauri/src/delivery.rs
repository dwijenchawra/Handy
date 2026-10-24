//! Ordered, durable delivery independent of capture and recognition.
use crate::{managers::audio::AudioRecordingManager, TranscriptionCoordinator};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[derive(Clone, Serialize, Deserialize, specta::Type)]
pub struct Item {
    pub id: String,
    pub text: String,
}
#[derive(Default)]
struct Queue {
    items: VecDeque<Item>,
    loaded: bool,
    working: bool,
    paused: bool,
}
static QUEUE: Mutex<Queue> = Mutex::new(Queue {
    items: VecDeque::new(),
    loaded: false,
    working: false,
    paused: false,
});
#[derive(Clone, Serialize, specta::Type)]
pub struct Status {
    pub current: Option<Item>,
    pub count: usize,
    pub pasting: bool,
}
impl Queue {
    fn status(&self) -> Status {
        Status {
            current: self.items.front().cloned(),
            count: self.items.len(),
            pasting: self.working,
        }
    }
    fn claim(&mut self) -> bool {
        if self.working || self.paused || self.items.is_empty() {
            return false;
        }
        self.working = true;
        true
    }
    fn complete(&mut self) {
        self.items.pop_front();
    }
}
fn load(app: &AppHandle, q: &mut Queue) -> Result<(), String> {
    if !q.loaded {
        let path = app
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())?
            .join("pending-delivery.json");
        if path.exists() {
            q.items = serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        }
        q.paused = !q.items.is_empty(); // Never paste recovered text into an arbitrary app.
        q.loaded = true;
    }
    Ok(())
}
fn save(app: &AppHandle, q: &Queue) -> Result<(), String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec(&q.items).map_err(|e| e.to_string())?;
    let temp = dir.join("pending-delivery.json.tmp");
    std::fs::write(&temp, bytes).map_err(|e| e.to_string())?;
    std::fs::File::open(&temp)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::rename(temp, dir.join("pending-delivery.json")).map_err(|e| e.to_string())
}
#[tauri::command]
#[specta::specta]
pub fn get_delivery_status(app: AppHandle) -> Result<Status, String> {
    let mut q = QUEUE.lock().map_err(|_| "Delivery lock poisoned")?;
    load(&app, &mut q)?;
    Ok(q.status())
}
pub fn refresh_overlay(app: &AppHandle) {
    if app.state::<Arc<AudioRecordingManager>>().is_recording()
        || app.state::<TranscriptionCoordinator>().has_pending()
    {
        return;
    }
    let status = match get_delivery_status(app.clone()) {
        Ok(s) => s,
        Err(e) => {
            log::error!("Delivery status: {e}");
            return;
        }
    };
    if status.current.is_some() {
        crate::overlay::show_delivery_overlay(app, status.pasting);
    } else {
        crate::utils::hide_recording_overlay(app);
    }
}
fn notify(app: &AppHandle) {
    if let Ok(status) = get_delivery_status(app.clone()) {
        let _ = app.emit("delivery-status", status);
    }
    refresh_overlay(app);
}
pub fn enqueue(app: AppHandle, id: String, text: String) -> Result<(), String> {
    {
        let mut q = QUEUE.lock().map_err(|_| "Delivery lock poisoned")?;
        load(&app, &mut q)?;
        q.items.push_back(Item { id, text });
        save(&app, &q)?;
    }
    start(app.clone(), false);
    notify(&app);
    Ok(())
}
fn start(app: AppHandle, retry: bool) {
    {
        let Ok(mut q) = QUEUE.lock() else {
            return;
        };
        if !q.claim() {
            return;
        }
    }
    notify(&app);
    std::thread::spawn(move || {
        if retry {
            std::thread::sleep(Duration::from_millis(300));
        }
        loop {
            let item = match QUEUE.lock() {
                Ok(q) => q.items.front().cloned(),
                Err(_) => None,
            };
            let Some(item) = item else {
                break;
            };
            let (tx, rx) = std::sync::mpsc::channel();
            let handle = app.clone();
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cancel_on_main = cancelled.clone();
            let scheduled = app.run_on_main_thread(move || {
                if cancel_on_main.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let settings = crate::settings::get_settings(&handle);
                let confirm = cfg!(target_os = "macos")
                    && settings.reliable_paste
                    && matches!(
                        settings.paste_method,
                        crate::settings::PasteMethod::CtrlV
                            | crate::settings::PasteMethod::CtrlShiftV
                            | crate::settings::PasteMethod::ShiftInsert
                    );
                let _ = tx.send(crate::utils::paste(item.text, handle).map(|_| confirm));
            });
            let result = scheduled
                .map_err(|e| e.to_string())
                .and_then(|_| {
                    rx.recv_timeout(Duration::from_secs(5)).map_err(|e| {
                        cancelled.store(true, std::sync::atomic::Ordering::Release);
                        e.to_string()
                    })?
                })
                .and_then(|confirm| {
                    if confirm {
                        crate::paste_tx::wait_for_completion()
                    } else {
                        Ok(true)
                    }
                });
            let confirmed = matches!(result, Ok(true));
            if let Err(e) = result {
                log::warn!("Delivery needs attention: {e}");
            }
            let keep_going = if let Ok(mut q) = QUEUE.lock() {
                if confirmed {
                    q.complete();
                } else {
                    q.paused = true;
                }
                if let Err(e) = save(&app, &q) {
                    log::error!("Could not persist delivery completion: {e}");
                    q.paused = true;
                }
                let more = !q.paused && !q.items.is_empty();
                if !more {
                    q.working = false;
                }
                more
            } else {
                false
            };
            notify(&app);
            if !keep_going {
                break;
            }
        }
    });
}
/// Explicit user action only; no automatic retry after an uncertain paste.
#[tauri::command]
#[specta::specta]
pub fn resolve_delivery(app: AppHandle, id: String, action: String) -> Result<(), String> {
    {
        let mut q = QUEUE.lock().map_err(|_| "Delivery lock poisoned")?;
        load(&app, &mut q)?;
        if q.working {
            return Err("Delivery is already in progress".into());
        }
        let item = q.items.front().ok_or("No pending delivery")?;
        if item.id != id {
            return Err("Pending delivery changed; refresh and try again".into());
        }
        match action.as_str() {
            "copy" => {
                app.clipboard()
                    .write_text(&item.text)
                    .map_err(|e| e.to_string())?;
            }
            "done" => {
                q.complete();
                q.paused = !q.items.is_empty();
                save(&app, &q)?;
            }
            "retry" => {}
            _ => return Err("Unknown delivery action".into()),
        }
    }
    if action == "retry" {
        // Hide settings before injecting into the previously focused application.
        if let Some(w) = app.get_webview_window("main") {
            w.hide().map_err(|e| e.to_string())?;
        }
        QUEUE.lock().map_err(|_| "Delivery lock poisoned")?.paused = false;
        start(app, true);
    } else {
        notify(&app);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paused_or_active_delivery_cannot_be_claimed() {
        let mut q = Queue::default();
        assert!(!q.claim());
        q.items.push_back(Item {
            id: "first".into(),
            text: "text".into(),
        });
        q.paused = true;
        assert!(!q.claim());
        q.paused = false;
        assert!(q.claim());
        assert!(!q.claim());
    }
    #[test]
    fn pending_front_survives_new_results_and_round_trip() {
        let mut q = Queue::default();
        q.items.push_back(Item {
            id: "first".into(),
            text: "undelivered".into(),
        });
        q.paused = true;
        q.items.push_back(Item {
            id: "second".into(),
            text: "later".into(),
        });
        let bytes = serde_json::to_vec(&q.items).unwrap();
        let restored: VecDeque<Item> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.front().unwrap().text, "undelivered");
        q.complete();
        assert_eq!(q.items.front().unwrap().id, "second");
    }
}

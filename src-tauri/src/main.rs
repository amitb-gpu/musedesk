mod agent;
mod api;
mod tools;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, State};
use tokio::sync::oneshot;

pub struct AppState {
    pub api: api::ModelBackend,
    pub approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

/// Starts one agent turn in the background; progress streams back as events.
#[tauri::command]
async fn send_message(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    text: String,
) -> Result<(), String> {
    let app_clone = app.clone();
    let state_clone = state.inner().clone();
    tokio::spawn(async move {
        if let Err(e) = agent::run_loop(app_clone.clone(), &state_clone, text).await {
            let _ = app_clone.emit("agent-error", e);
        }
    });
    Ok(())
}

/// Resolves a pending approval card (Approve / Deny in the UI).
#[tauri::command]
async fn resolve_approval(
    state: State<'_, Arc<AppState>>,
    id: String,
    approved: bool,
) -> Result<(), String> {
    let sender = state
        .approvals
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&id)
        .ok_or_else(|| "unknown approval id".to_string())?;
    let _ = sender.send(approved);
    Ok(())
}

#[cfg(test)]
mod capability_tests {
    use serde_json::Value;

    /// Regression guard: the frontend subscribes to agent streaming events
    /// (`token`, `tool-call`, `tool-result`, `approval-requested`, `done`,
    /// `agent-error`) via `event.listen`. Without an event-listen permission
    /// in the main window's capability set, Tauri denies every `listen` call
    /// and the UI renders nothing while the backend runs fine — a silent
    /// failure only visible in the devtools console. This test fails loudly
    /// if that permission goes missing again.
    #[test]
    fn main_window_may_listen_to_agent_events() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| {
            panic!(
                "src-tauri/capabilities must exist so the UI can listen to agent events: {e}"
            )
        });

        let mut main_caps = 0;
        let mut listen_allowed = false;
        for entry in entries {
            let path = entry.expect("readable capability entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let cap: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
            let is_main = cap
                .pointer("/windows")
                .and_then(|w| w.as_array())
                .map(|w| w.iter().any(|v| v.as_str() == Some("main")))
                .unwrap_or(false);
            if !is_main {
                continue;
            }
            main_caps += 1;
            let perms: Vec<String> = cap
                .pointer("/permissions")
                .and_then(|p| p.as_array())
                .map(|a| {
                    a.iter()
                        .map(|v| {
                            v.as_str().unwrap_or_else(|| {
                                panic!("{}: permissions must be strings", path.display())
                            })
                            .to_string()
                        })
                        .collect()
                })
                .unwrap_or_else(|| panic!("{}: main-window capability needs /permissions", path.display()));
            if perms.iter().any(|p| p == "core:event:allow-listen" || p == "core:event:default") {
                listen_allowed = true;
            }
            // Scope stays tight: this window gets event permissions only. All
            // filesystem / shell access is mediated by the Rust tools behind
            // approval-gated commands — no direct grants here.
            for p in &perms {
                assert!(
                    !p.starts_with("core:fs:")
                        && !p.starts_with("core:shell")
                        && !p.starts_with("fs:")
                        && !p.starts_with("shell:")
                        && !p.starts_with("dialog:"),
                    "{}: unexpected broad grant {p} on the main window",
                    path.display()
                );
            }
        }

        assert!(main_caps > 0, "no capability targets the main window");
        assert!(
            listen_allowed,
            "main window capability must include core:event:allow-listen \
             (or core:event:default) or the UI cannot receive agent events"
        );
    }
}

fn main() {
    let api = api::ModelBackend::from_env().expect(
        "MUSED_BACKEND=local-cli needs no key; otherwise set META_API_KEY / META_BASE_URL (see .env.example)",
    );

    tauri::Builder::default()
        .manage(Arc::new(AppState {
            api,
            approvals: Mutex::new(HashMap::new()),
        }))
        .invoke_handler(tauri::generate_handler![send_message, resolve_approval])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

use serde_json::{json, Value};
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{tools, AppState};

const MAX_ITERS: u32 = 25;

const SYSTEM_PROMPT: &str = r#"You are an AI coding assistant inside MuseDesk, a Windows desktop app.
You help the user with software tasks: reading/writing code, running commands, debugging.

You have tools: read_file, write_file, list_dir, shell (Windows PowerShell), wsl (WSL2 Ubuntu).

WSL bridge rules — every `wsl` call is a FRESH login shell, like one-shot SSH:
- Re-`cd` to the working directory and re-establish ALL context in EVERY call.
- Prefer `conda run -n <env> <cmd>` over `conda activate`.
- Use `bash -lc`, never `bash -ic` (interactive shells pollute output).
- Native WSL paths (~/project) are fast; /mnt/c paths are slower — pick one side as source of truth.
- State does NOT persist between calls.

Be concise. Explain what you're about to do before doing anything destructive.
Prefer small, verifiable steps."#;

/// One turn of the agent loop. Streams tokens / tool activity as Tauri events:
/// `token`, `tool-call`, `tool-result`, `approval-requested`, `done`, `agent-error`.
pub async fn run_loop(
    app: tauri::AppHandle,
    state: &Arc<AppState>,
    user_text: String,
) -> Result<(), String> {
    let mut messages: Vec<Value> = vec![
        json!({"role": "system", "content": SYSTEM_PROMPT}),
        json!({"role": "user", "content": user_text}),
    ];
    let tools_schema = tools::to_openai_schema();

    for _ in 0..MAX_ITERS {
        // Accumulate one assistant message from the SSE stream.
        let mut content = String::new();
        // (index -> (id, name, arguments-so-far)) for streaming tool calls
        let mut tcalls: Vec<(Option<String>, String, String)> = Vec::new();

        let app_ref = &app;
        let content_ref = &mut content;
        let tcalls_ref = &mut tcalls;
        state
            .api
            .chat_stream(&messages, &tools_schema, |delta| {
                let choice = delta
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"));
                let Some(choice) = choice else { return };
                if let Some(t) = choice.get("content").and_then(|c| c.as_str()) {
                    content_ref.push_str(t);
                    let _ = app_ref.emit("token", t.to_string());
                }
                if let Some(arr) = choice.get("tool_calls").and_then(|a| a.as_array()) {
                    for tc in arr {
                        let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        while tcalls_ref.len() <= idx {
                            tcalls_ref.push((None, String::new(), String::new()));
                        }
                        let entry = &mut tcalls_ref[idx];
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            entry.0 = Some(id.to_string());
                        }
                        if let Some(f) = tc.get("function") {
                            if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                                if !name.is_empty() {
                                    entry.1 = name.to_string();
                                }
                            }
                            if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }
            })
            .await?;

        // Finalize tool calls (drop empties from sparse indexes).
        let calls: Vec<(String, String, Value)> = tcalls
            .into_iter()
            .filter(|(_, name, _)| !name.is_empty())
            .map(|(id, name, args)| {
                let args_json: Value =
                    serde_json::from_str(&args).unwrap_or(Value::Null);
                (
                    id.unwrap_or_else(|| format!("call_{}", Uuid::new_v4())),
                    name,
                    args_json,
                )
            })
            .collect();

        messages.push(json!({
            "role": "assistant",
            "content": content,
            "tool_calls": calls.iter().map(|(id, name, args)| json!({
                "id": id, "type": "function",
                "function": { "name": name, "arguments": args.to_string() }
            })).collect::<Vec<_>>(),
        }));

        if calls.is_empty() {
            let _ = app.emit("done", content);
            return Ok(());
        }

        for (id, name, args) in &calls {
            let _ = app.emit(
                "tool-call",
                json!({ "id": id, "name": name, "arguments": args }),
            );

            if tools::needs_approval(name) {
                let approval_id = Uuid::new_v4().to_string();
                let (tx, rx) = oneshot::channel::<bool>();
                state
                    .approvals
                    .lock()
                    .map_err(|e| e.to_string())?
                    .insert(approval_id.clone(), tx);
                let _ = app.emit(
                    "approval-requested",
                    json!({ "id": approval_id, "tool": name, "arguments": args }),
                );
                let approved = rx.await.unwrap_or(false);
                if !approved {
                    let result = "denied by user".to_string();
                    let _ = app.emit("tool-result", json!({ "id": id, "result": result }));
                    messages.push(json!({
                        "role": "tool", "tool_call_id": id,
                        "content": "The user denied this action. Do not retry it; ask how to proceed."
                    }));
                    continue;
                }
            }

            let result = tools::execute(name, args)
                .await
                .unwrap_or_else(|e| format!("tool error: {e}"));
            let _ = app.emit("tool-result", json!({ "id": id, "result": result }));
            messages.push(json!({
                "role": "tool", "tool_call_id": id, "content": result
            }));
        }
    }

    Err("agent loop hit iteration limit — task may be too long".into())
}

# Paste this into the `muse` CLI (Windows, `--disable-sandbox`) to continue building MuseDesk

```
I'm building MuseDesk, a Claude Cowork-style desktop agent for Windows.
Stack: Tauri v2 (Rust backend) + plain HTML/JS frontend, powered by the
Meta Model API (OpenAI-compatible /chat/completions with SSE streaming).

The scaffold is in the current directory. It has:
- src-tauri/src/main.rs  (Tauri commands: send_message, resolve_approval)
- src-tauri/src/api.rs    (streaming API client)
- src-tauri/src/agent.rs  (agent loop with tool calls + approval gating)
- src-tauri/src/tools.rs  (read_file, write_file, list_dir, shell, wsl)
- src/                    (chat UI: index.html, styles.css, main.js)

My environment: Windows 11 host + WSL2 Ubuntu distro named "Ubuntu".
My WSL bridge pattern (from https://github.com/amitb-gpu/wsl-agent-bridge):
one-shot `wsl.exe -d Ubuntu -e bash -lc '<cmd>'` calls. Every call is a fresh
login shell, so re-cd and re-establish context each time. Prefer
`conda run -n <env>` over activation. Use `bash -lc`, never `bash -ic`.

First: get it compiling. Run `npx tauri dev` (or `cargo check` in src-tauri)
and fix every error. Then verify the agent loop end-to-end against the real
API: send a message, confirm streaming tokens render, trigger a write_file
to confirm the approval card appears and gating works, and run a `wsl` tool
call to confirm the bridge works from inside the app.

Keep approval prompts ON for shell/write_file/wsl. Do not add --yolo or
disable approvals. Report each milestone before moving on.
```

use futures_util::StreamExt;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

/// Minimal client for the Meta Model API's OpenAI-compatible chat endpoint.
#[derive(Clone, Debug)]
pub struct MetaApiClient {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    http: reqwest::Client,
}

impl MetaApiClient {
    pub fn from_env() -> Result<Self, String> {
        // Name every missing setting up front (and treat blank as missing),
        // so the guidance points at what's actually wrong. local-cli needs
        // none of these — see ModelBackend::from_env.
        let missing: Vec<&str> = ["META_BASE_URL", "META_API_KEY"]
            .into_iter()
            .filter(|k| {
                std::env::var(k)
                    .map(|v| v.trim().is_empty())
                    .unwrap_or(true)
            })
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "missing {} — set {} (see .env.example), or set MUSED_BACKEND=local-cli to use the local CLI with no key",
                missing.join(" and "),
                if missing.len() == 1 { "it" } else { "them" },
            ));
        }
        let get = |k: &str| {
            std::env::var(k).map_err(|_| format!("missing env var {k} — see .env.example"))
        };
        Ok(Self {
            base_url: get("META_BASE_URL")?,
            api_key: get("META_API_KEY")?,
            model: std::env::var("MODEL").unwrap_or_else(|_| "muse-spark-1.2".into()),
            http: reqwest::Client::new(),
        })
    }

    /// POSTs a streaming chat completion; invokes `on_delta` with each parsed
    /// SSE `data:` JSON chunk until `[DONE]`.
    pub async fn chat_stream(
        &self,
        messages: &[Value],
        tools: &[Value],
        mut on_delta: impl FnMut(Value),
    ) -> Result<(), String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "stream": true,
        });
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("API error: {text}"));
        }

        let mut buf = String::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("stream error: {e}"))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(idx) = buf.find("\n\n") {
                let event: String = buf.drain(..idx + 2).collect();
                for line in event.lines() {
                    let Some(data) = line.trim().strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        return Ok(());
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        on_delta(v);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Demo backend (until Meta support unblocks key creation): runs each agent
/// turn through the locally installed `muse` CLI, which bills to the Muse
/// Code subscription instead of a Meta API key. The Meta API HTTP path above
/// stays fully intact — this only swaps where the reasoning comes from.
///
/// Mechanism (verified live against `muse exec --help`): the CLI refuses to
/// emit fenced tool-call blocks, so instead we pass `--output-schema` with
/// the fixed schema below. Its final answer is then exactly
/// `{"text": "...", "tool_calls": [{"name": "...", "arguments": "<JSON string>"}]}`,
/// which maps 1:1 onto the OpenAI shapes the agent loop parses
/// (`choices[0].delta.content`, `choices[0].delta.tool_calls` with string
/// `function.arguments`).
///
/// The CLI is told it is the reasoning engine only (its own shell/write/web
/// tools are disabled via flags, and the prompt forbids acting). It therefore
/// cannot write or execute anything itself: every destructive step comes back
/// as a `tool_calls` entry and runs through the app's own tools + approval
/// cards. `--json` JSONL events carry the answer tokens; the structured
/// answer arrives in the terminal event, so per-turn output lands as one
/// content delta plus one delta per tool call rather than token-by-token.
#[derive(Clone, Debug)]
pub struct LocalCliClient {
    timeout: Duration,
}

impl Default for LocalCliClient {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5 * 60),
        }
    }
}

/// Strict-mode structured-output schema for the CLI's final answer.
/// `arguments` is a JSON-encoded *string* (mirroring OpenAI's
/// `function.arguments`); a free-form object schema would only ever decode
/// to `{}` under strict decoding, which was verified live.
const OUTPUT_SCHEMA: &str = r#"{"additionalProperties":false,"properties":{"text":{"type":"string"},"tool_calls":{"items":{"additionalProperties":false,"properties":{"arguments":{"description":"JSON object as a string, e.g. {\"path\": \"C:\\temp\"}","type":"string"},"name":{"type":"string"}},"required":["name","arguments"],"type":"object"},"type":"array"}},"required":["text","tool_calls"],"type":"object"}"#;

impl LocalCliClient {
    pub async fn chat_stream(
        &self,
        messages: &[Value],
        tools: &[Value],
        mut on_delta: impl FnMut(Value),
    ) -> Result<(), String> {
        let prompt = render_prompt(messages, tools);
        let tag = uuid::Uuid::new_v4();
        let prompt_path = std::env::temp_dir().join(format!("musedesk-prompt-{tag}.md"));
        let schema_path = std::env::temp_dir().join(format!("musedesk-schema-{tag}.json"));
        tokio::fs::write(&prompt_path, &prompt)
            .await
            .map_err(|e| format!("local CLI: failed to write prompt file: {e}"))?;
        tokio::fs::write(&schema_path, OUTPUT_SCHEMA)
            .await
            .map_err(|e| format!("local CLI: failed to write schema file: {e}"))?;

        // `muse` resolves via a .cmd shim, so go through cmd /C. The prompt
        // travels via --prompt-file (stdin is key-only: --api-key-stdin, and
        // argv risks quoting/length bugs). Its own action tools are disabled:
        // it must only reason and return the structured answer.
        let mut child = tokio::process::Command::new("cmd")
            // CREATE_NO_WINDOW: never flash a console when reasoning.
            .creation_flags(0x08000000)
            .args(["/C", "muse", "exec", "--json", "--prompt-file"])
            .arg(&prompt_path)
            .args(["--output-schema"])
            .arg(&schema_path)
            .args(["--disable-shell", "--disable-write", "--disable-web-tools"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("local CLI: failed to spawn `muse exec`: {e}"))?;

        let result = tokio::time::timeout(self.timeout, stream_child(&mut child, &mut on_delta)).await;
        let _ = tokio::fs::remove_file(&prompt_path).await;
        let _ = tokio::fs::remove_file(&schema_path).await;
        match result {
            Ok(r) => r,
            Err(_) => {
                let _ = child.kill().await;
                Err("local CLI timed out after 5 minutes".into())
            }
        }
    }
}

/// Reads the CLI's `--json` JSONL stdout until the terminal event, then
/// converts the structured answer into OpenAI-shaped deltas. Returns Err on
/// spawn/IO failure, a non-`completed` terminal state, an unparsable answer,
/// or a non-zero exit (with stderr attached).
async fn stream_child(
    child: &mut tokio::process::Child,
    on_delta: &mut impl FnMut(Value),
) -> Result<(), String> {
    let stdout = child.stdout.take().ok_or("local CLI: stdout was not piped")?;
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_string(&mut s).await;
        }
        s
    });

    let mut answer: Option<Value> = None;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(line) else {
                    continue; // banners / non-JSON noise
                };
                if event.pointer("/payload_type").and_then(|t| t.as_str())
                    != Some("run.terminal.completed")
                {
                    continue;
                }
                let payload = event.pointer("/payload").unwrap_or(&Value::Null);
                if payload.pointer("/terminal").and_then(|t| t.as_str()) != Some("completed") {
                    return Err(format!(
                        "local CLI run did not complete: {}",
                        payload
                            .pointer("/reason")
                            .and_then(|r| r.as_str())
                            .unwrap_or("unknown reason")
                    ));
                }
                let text = payload
                    .pointer("/text")
                    .and_then(|t| t.as_str())
                    .ok_or("local CLI terminal event has no text")?;
                answer = Some(
                    serde_json::from_str(text)
                        .map_err(|e| format!("local CLI answer is not valid JSON: {e}"))?,
                );
                break;
            }
            Ok(None) => break,
            Err(e) => return Err(format!("local CLI: failed reading stdout: {e}")),
        }
    }
    // Drain any remaining output so the child can exit cleanly.
    drop(lines);

    let status = child
        .wait()
        .await
        .map_err(|e| format!("local CLI: failed waiting for exit: {e}"))?;
    let stderr = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let tail: String = stderr.chars().rev().take(2000).collect::<String>().chars().rev().collect();
        return Err(format!("local CLI failed ({status}): {tail}"));
    }
    let Some(answer) = answer else {
        return Err("local CLI exited without a terminal answer event".into());
    };
    for delta in deltas_for_answer(&answer)? {
        on_delta(delta);
    }
    Ok(())
}

/// Converts one structured CLI answer (`{text, tool_calls[]}`) into the
/// OpenAI-shaped deltas the agent loop parses: one `content` delta plus one
/// `tool_calls` delta per call (indexes 0,1,2… with synthetic
/// `call_local_<n>` ids and string `function.arguments`).
pub(crate) fn deltas_for_answer(answer: &Value) -> Result<Vec<Value>, String> {
    let text = answer
        .pointer("/text")
        .and_then(|t| t.as_str())
        .ok_or("local CLI answer is missing string field \"text\"")?;
    let calls = answer
        .pointer("/tool_calls")
        .and_then(|c| c.as_array())
        .ok_or("local CLI answer is missing array field \"tool_calls\"")?;

    let mut deltas = Vec::new();
    if !text.is_empty() {
        deltas.push(json!({"choices": [{"delta": {"content": text}}]}));
    }
    for (idx, call) in calls.iter().enumerate() {
        let name = call
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| format!("local CLI tool call #{idx} is missing a name"))?;
        let args = call
            .get("arguments")
            .and_then(|a| a.as_str())
            .ok_or_else(|| format!("local CLI tool call #{idx} is missing arguments"))?;
        if serde_json::from_str::<Value>(args).is_err() {
            return Err(format!(
                "local CLI tool call #{idx} arguments are not a JSON string"
            ));
        }
        deltas.push(json!({"choices": [{"delta": {"tool_calls": [{
            "index": idx,
            "id": format!("call_local_{idx}"),
            "type": "function",
            "function": {"name": name, "arguments": args}
        }]}}]}));
    }
    Ok(deltas)
}

/// Which model backend serves `chat_stream`. Selected via `MUSED_BACKEND`:
/// `local-cli` runs on the Muse Code subscription (no API key); anything
/// else (including unset) keeps the existing Meta API behavior.
#[derive(Clone, Debug)]
pub enum ModelBackend {
    MetaApi(MetaApiClient),
    LocalCli(LocalCliClient),
}

impl ModelBackend {
    pub fn from_env() -> Result<Self, String> {
        // Trimmed and case-insensitive: a stray space or capital letter must
        // never silently reroute local-cli users into the key-requiring path.
        // local-cli needs no META_* vars at all.
        let raw = std::env::var("MUSED_BACKEND").unwrap_or_default();
        match raw.trim().to_lowercase().as_str() {
            "local-cli" => Ok(ModelBackend::LocalCli(LocalCliClient::default())),
            "" | "meta-api" => Ok(ModelBackend::MetaApi(MetaApiClient::from_env()?)),
            _ => Err(format!(
                "unknown MUSED_BACKEND={raw:?} (expected meta-api or local-cli)"
            )),
        }
    }

    pub async fn chat_stream(
        &self,
        messages: &[Value],
        tools: &[Value],
        on_delta: impl FnMut(Value),
    ) -> Result<(), String> {
        match self {
            ModelBackend::MetaApi(c) => c.chat_stream(messages, tools, on_delta).await,
            ModelBackend::LocalCli(c) => c.chat_stream(messages, tools, on_delta).await,
        }
    }
}

/// Renders the OpenAI-style message list plus tool schemas as one plain-text
/// prompt: engine-only directive, system prompt, tool definitions (names +
/// JSON schemas), conversation, and the structured-answer format instruction.
pub(crate) fn render_prompt(messages: &[Value], tools: &[Value]) -> String {
    fn content_of(msg: &Value) -> String {
        match msg.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    }

    let mut out = String::new();
    out.push_str(
        "You are the REASONING ENGINE ONLY. You have no working tools in this \
         session: do not call, invoke, or use any built-in tool for any reason. \
         The only way to get anything done is the structured response described \
         at the end of this prompt.\n\n",
    );
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
            let c = content_of(msg);
            if !c.is_empty() {
                out.push_str("SYSTEM:\n");
                out.push_str(&c);
                out.push_str("\n\n");
            }
        }
    }

    out.push_str("AVAILABLE TOOLS (request them in tool_calls — see format below):\n");
    for t in tools {
        let f = t.get("function").unwrap_or(t);
        let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let desc = f.get("description").and_then(|d| d.as_str()).unwrap_or("");
        let params = f.get("parameters").cloned().unwrap_or(Value::Null);
        out.push_str(&format!("- {name}: {desc}\n  parameters JSON schema:\n  {params}\n"));
    }

    out.push_str("\nCONVERSATION:\n");
    for msg in messages {
        match msg.get("role").and_then(|r| r.as_str()) {
            Some("system") => {}
            Some("assistant") => {
                out.push_str("assistant:\n");
                let c = content_of(msg);
                if !c.is_empty() {
                    out.push_str(&c);
                    out.push('\n');
                }
                if let Some(calls) = msg.get("tool_calls") {
                    out.push_str("assistant tool_calls:\n");
                    out.push_str(&calls.to_string());
                    out.push('\n');
                }
            }
            Some("tool") => {
                let id = msg.get("tool_call_id").and_then(|i| i.as_str()).unwrap_or("?");
                out.push_str(&format!("tool result (id={id}):\n{}\n", content_of(msg)));
            }
            _ => {
                out.push_str("user:\n");
                out.push_str(&content_of(msg));
                out.push('\n');
            }
        }
    }

    out.push_str(
        "\nRESPONSE FORMAT — your entire response must be exactly one JSON \
         object, and nothing else:\n\
         {\"text\": \"<reply to the user>\", \"tool_calls\": \
         [{\"name\": \"<tool-name>\", \"arguments\": \"<JSON-encoded args string>\"}]}\n\
         - \"arguments\" is a STRING containing a JSON object, e.g. \
         \"{\\\"path\\\": \\\"C:\\\\temp\\\"}\".\n\
         - Multiple tool calls allowed; use an empty array when no tool is needed.\n\
         - Use only the tools listed above, with arguments matching their schemas.\n\
         - CRITICAL: never invoke any of your own built-in tools (no searching, \
         reading, executing, or browsing yourself) — using them makes your answer \
         unusable.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tools() -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List entries in a directory.",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        })]
    }

    // Boot contract (ship-blocker guard): bad or missing config must come back
    // as a plain Err the app boots past — never a panic. Release builds have
    // no console, so a startup panic would fail completely silently.
    // Note: MUSED_BACKEND is process-global; this test saves and restores it.
    #[test]
    fn bad_config_is_a_plain_error_never_a_panic() {
        let prior = std::env::var("MUSED_BACKEND").ok();
        std::env::set_var("MUSED_BACKEND", "bogus-backend-for-test");
        let result = ModelBackend::from_env();
        match prior {
            Some(v) => std::env::set_var("MUSED_BACKEND", v),
            None => std::env::remove_var("MUSED_BACKEND"),
        }
        assert!(
            result.is_err(),
            "bad backend config must be an Err the app can boot past"
        );
    }

    // One test (not two) so the process-global env var can't race: every
    // assertion that touches MUSED_BACKEND lives here.
    #[test]
    fn backend_selection_from_env() {
        // local-cli resolves with no META keys at all (fresh-install path),
        // and tolerates stray whitespace/case in the value.
        std::env::remove_var("META_BASE_URL");
        std::env::remove_var("META_API_KEY");
        for value in ["local-cli", "  Local-CLI  "] {
            std::env::set_var("MUSED_BACKEND", value);
            let backend =
                ModelBackend::from_env().expect("local-cli must not require META keys");
            assert!(matches!(backend, ModelBackend::LocalCli(_)));
        }
        // Unset backend + no keys: the error must name what's actually
        // missing (both keys), not just the first one checked.
        std::env::remove_var("MUSED_BACKEND");
        let err = ModelBackend::from_env().expect_err("keyless meta-api must fail");
        assert!(
            err.contains("META_BASE_URL") && err.contains("META_API_KEY"),
            "unhelpful missing-key error: {err}"
        );
        std::env::set_var("MUSED_BACKEND", "bogus");
        assert!(ModelBackend::from_env().is_err());
        std::env::remove_var("MUSED_BACKEND");
    }

    #[test]
    fn output_schema_is_strict_and_string_argued() {
        let schema: Value = serde_json::from_str(OUTPUT_SCHEMA).unwrap();
        assert_eq!(schema.pointer("/type").unwrap(), "object");
        assert_eq!(schema.pointer("/additionalProperties").unwrap(), false);
        // arguments must be a *string* (strict objects can't be free-form).
        assert_eq!(
            schema.pointer("/properties/tool_calls/items/properties/arguments/type").unwrap(),
            "string"
        );
    }

    #[test]
    fn prompt_renders_system_tools_conversation_and_format() {
        let messages = vec![
            json!({"role": "system", "content": "sys-prompt"}),
            json!({"role": "user", "content": "list files"}),
            json!({"role": "assistant", "content": "ok", "tool_calls": []}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "a\nb"}),
        ];
        let prompt = render_prompt(&messages, &sample_tools());
        assert!(prompt.contains("sys-prompt"));
        assert!(prompt.contains("list_dir"));
        assert!(prompt.contains("\"path\""));
        assert!(prompt.contains("list files"));
        assert!(prompt.contains("tool result (id=call_1)"));
        assert!(prompt.contains("tool_calls"));
        assert!(prompt.contains("REASONING ENGINE ONLY"));
        assert!(prompt.contains("JSON-encoded args string"));
    }

    #[test]
    fn answer_with_tool_call_maps_to_agent_shapes() {
        let answer = json!({
            "text": "listing now",
            "tool_calls": [{"name": "list_dir", "arguments": "{\"path\": \"C:\\\\x\"}"}]
        });
        let deltas = deltas_for_answer(&answer).unwrap();
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].pointer("/choices/0/delta/content").unwrap(), "listing now");
        let tc = deltas[1].pointer("/choices/0/delta/tool_calls/0").unwrap();
        assert_eq!(tc.pointer("/index").unwrap(), 0);
        assert_eq!(tc.pointer("/id").unwrap(), "call_local_0");
        assert_eq!(tc.pointer("/function/name").unwrap(), "list_dir");
        // agent.rs reads arguments as a JSON *string* — must re-parse.
        let args: Value = serde_json::from_str(
            tc.pointer("/function/arguments").unwrap().as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(args.pointer("/path").unwrap(), "C:\\x");
    }

    #[test]
    fn answer_indexes_multiple_calls_and_skips_empty_text() {
        let answer = json!({
            "text": "",
            "tool_calls": [
                {"name": "read_file", "arguments": "{}"},
                {"name": "wsl", "arguments": "{\"command\": \"whoami\"}"},
            ]
        });
        let deltas = deltas_for_answer(&answer).unwrap();
        assert_eq!(deltas.len(), 2);
        let second = deltas[1].pointer("/choices/0/delta/tool_calls/0").unwrap();
        assert_eq!(second.pointer("/index").unwrap(), 1);
        assert_eq!(second.pointer("/id").unwrap(), "call_local_1");
    }

    #[test]
    fn answer_rejects_bad_shapes() {
        assert!(deltas_for_answer(&json!({"text": "x"})).is_err());
        assert!(deltas_for_answer(&json!({"text": "x", "tool_calls": "nope"})).is_err());
        assert!(deltas_for_answer(&json!({
            "text": "x", "tool_calls": [{"name": "", "arguments": "{}"}]
        }))
        .is_err());
        assert!(deltas_for_answer(&json!({
            "text": "x", "tool_calls": [{"name": "shell", "arguments": "not json"}]
        }))
        .is_err());
    }
}

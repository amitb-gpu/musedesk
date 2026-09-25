use serde_json::{json, Value};

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    /// When true the agent loop pauses for an Approve / Deny card first.
    pub needs_approval: bool,
}

pub fn all() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_file",
            description: "Read a text file. Returns up to ~20k chars.",
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            needs_approval: false,
        },
        ToolDef {
            name: "write_file",
            description: "Write (create or overwrite) a text file.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            needs_approval: true,
        },
        ToolDef {
            name: "list_dir",
            description: "List entries in a directory.",
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            needs_approval: false,
        },
        ToolDef {
            name: "shell",
            description: "Run a command in Windows PowerShell. Returns stdout+stderr.",
            parameters: json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
            needs_approval: true,
        },
        ToolDef {
            name: "wsl",
            description: "Run a command inside WSL2 Ubuntu via wsl.exe interop. \
                Every call is a FRESH login shell: re-cd to the working directory \
                and re-establish all context in every call. Prefer `conda run -n <env>` \
                over activation. Do not use `bash -ic`.",
            parameters: json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "bash -lc command to run inside Ubuntu" } },
                "required": ["command"]
            }),
            needs_approval: true,
        },
    ]
}

pub fn to_openai_schema() -> Vec<Value> {
    all()
        .into_iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters
                }
            })
        })
        .collect()
}

pub fn needs_approval(name: &str) -> bool {
    all()
        .into_iter()
        .find(|t| t.name == name)
        .map(|t| t.needs_approval)
        .unwrap_or(true) // unknown tools are gated by default
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string arg: {key}"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…[truncated, {} bytes total]", &s[..max], s.len())
    }
}

async fn run_powershell(command: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .output()
        .await
        .map_err(|e| format!("failed to spawn powershell: {e}"))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.is_empty() {
        combined.push_str("\n[stderr]\n");
        combined.push_str(&stderr);
    }
    if !out.status.success() {
        combined.push_str(&format!("\n[exit code: {}]", out.status));
    }
    Ok(truncate(&combined, 12000))
}

/// Runs `inner` inside WSL2 Ubuntu by spawning wsl.exe directly — argv goes
/// straight through CreateProcess, so no PowerShell or shell quoting layer
/// can mangle payloads (a POSIX single-quote wrap does NOT survive
/// `powershell -Command`, verified live: it arrives split and prints blank).
/// Still one-shot `bash -lc` per call: a fresh login shell every time.
async fn run_wsl(inner: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("wsl.exe")
        .args(["-d", "Ubuntu", "-e", "bash", "-lc", inner])
        .output()
        .await
        .map_err(|e| format!("failed to spawn wsl.exe: {e}"))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.is_empty() {
        combined.push_str("\n[stderr]\n");
        combined.push_str(&stderr);
    }
    if !out.status.success() {
        combined.push_str(&format!("\n[exit code: {}]", out.status));
    }
    Ok(truncate(&combined, 12000))
}

pub async fn execute(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "read_file" => {
            let p = str_arg(args, "path")?;
            let s = tokio::fs::read_to_string(&p)
                .await
                .map_err(|e| format!("read {p}: {e}"))?;
            Ok(truncate(&s, 20000))
        }
        "write_file" => {
            let p = str_arg(args, "path")?;
            let c = str_arg(args, "content")?;
            if let Some(parent) = std::path::Path::new(&p).parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("mkdir for {p}: {e}"))?;
                }
            }
            tokio::fs::write(&p, c)
                .await
                .map_err(|e| format!("write {p}: {e}"))?;
            Ok(format!("wrote {p}"))
        }
        "list_dir" => {
            let p = str_arg(args, "path")?;
            let mut rd = tokio::fs::read_dir(&p)
                .await
                .map_err(|e| format!("list {p}: {e}"))?;
            let mut names = Vec::new();
            while let Some(e) = rd.next_entry().await.map_err(|e| e.to_string())? {
                names.push(e.file_name().to_string_lossy().to_string());
            }
            names.sort();
            Ok(names.join("\n"))
        }
        "shell" => run_powershell(&str_arg(args, "command")?).await,
        "wsl" => run_wsl(&str_arg(args, "command")?).await,
        _ => Err(format!("unknown tool: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_tools_require_approval() {
        assert!(!needs_approval("read_file"));
        assert!(!needs_approval("list_dir"));
        assert!(needs_approval("write_file"));
        assert!(needs_approval("shell"));
        assert!(needs_approval("wsl"));
        // Unknown tools fail closed: gated by default.
        assert!(needs_approval("rm_rf_everything"));
    }

}

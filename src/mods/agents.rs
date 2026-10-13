//! Folded-in subagent tool surface: prime-agent-style tools over the
//! subagent supervisor (mechanics port — no prime code). The supervisor
//! itself enforces depth-1 (`GRAY_SUBAGENTS_ACTIVE`) and the running cap
//! (`max_running` setting); this layer adds the model-facing tool surface
//! prime's `spawn/collect/send/delete/progress_note` map to, plus the
//! per-session spawn budget and session notes. No duplicate registry —
//! the supervisor's own runs/ records (session_id-scoped) are the truth.

use anyhow::Result;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gray_subagents::widget::AgentState;

use crate::{
    Entry, backend, chat, collect_entries, foreign_stop, home, menu_order, menu_target,
    resolve_entry, session_of_pid, session_root,
};

const MAX_SPAWNS: usize = 32;
const COLLECT_MAX_WAIT_MS: u64 = 120_000;
const DEPTH_MSG: &str = "subagent_spawn is unavailable inside a subagent run — children cannot spawn children (depth-1 only)";

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Session scope from the wire params, else the exported env marker,
/// else the process-tree-derived root.
fn session_id(params: &Value) -> String {
    params
        .pointer("/session/id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("GRAY_SESSION_ID")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| session_root().and_then(session_of_pid))
        .unwrap_or_else(|| "default".into())
}

fn notes_file(sid: &str) -> Result<PathBuf> {
    Ok(home()?
        .join("subagents")
        .join("notes")
        .join(format!("{}.md", sanitize(sid))))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Runs this session spawned — the supervisor records owner `session_id`
/// on every job, so the registry doubles as the subagent spawn ledger.
fn owned_runs(sid: &str) -> Vec<Value> {
    let dir = match home() {
        Ok(h) => h.join("subagents").join("runs"),
        Err(_) => return Vec::new(),
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut jobs = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(j) = serde_json::from_slice::<Value>(&std::fs::read(&p).unwrap_or_default()) else {
            continue;
        };
        if j["session_id"].as_str() == Some(sid) {
            jobs.push(j);
        }
    }
    jobs.sort_by(|a, b| {
        b["created"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["created"].as_f64().unwrap_or(0.0))
    });
    jobs
}

// ---------- tools ----------

/// Batch items as the supervisor reads them: `prompt` is the documented alias
/// of `task`, so an item with no `task` (or an empty one) takes its `prompt`.
/// Other keys pass through unchanged.
fn batch_items(tasks: &[Value]) -> Vec<Value> {
    tasks
        .iter()
        .map(|item| {
            let mut item = item.clone();
            let task_missing = match item.get("task") {
                None | Some(Value::Null) => true,
                Some(task) => task.as_str() == Some(""),
            };
            if task_missing
                && let Some(prompt) = item.get("prompt").cloned()
            {
                item["task"] = prompt;
            }
            item
        })
        .collect()
}

/// `subagent_spawn` — admit a supervised child, return a handle immediately.
/// `prompt` (or `task`) is required; `tasks: [...]` fans out 1–8 in one
/// admission (prime's batch spawn), each item taking name/agent/model/etc.
fn subagent_spawn(params: &Value, args: &Value) -> Result<String, String> {
    if std::env::var_os("GRAY_SUBAGENTS_ACTIVE").is_some() {
        return Err(DEPTH_MSG.into());
    }
    let sid = session_id(params);
    let batch = args["tasks"].as_array();
    if batch.is_none() && args["prompt"].as_str().or(args["task"].as_str()).is_none() {
        return Err("subagent_spawn requires prompt".into());
    }
    let owned = owned_runs(&sid);
    if owned.len() >= MAX_SPAWNS {
        return Err(format!(
            "spawn budget exhausted — {MAX_SPAWNS} spawns per session (this session has {})",
            owned.len()
        ));
    }
    let mut run = json!({});
    if let Some(tasks) = batch {
        if tasks.is_empty() || tasks.len() > 8 {
            return Err("tasks must contain 1–8 items".into());
        }
        run["tasks"] = Value::Array(batch_items(tasks));
    } else {
        run["task"] = args["prompt"].as_str().or(args["task"].as_str()).into();
        for (k, t) in [
            ("agent", "agent"),
            ("name", "name"),
            ("tools", "tools"),
            ("approve", "approve"),
        ] {
            if let Some(v) = args[t].as_str().filter(|s| !s.is_empty()) {
                run[k] = json!(v);
            }
        }
        if run["agent"].is_null() {
            run["agent"] = json!("scout");
        }
        if let Some(m) = args["model"].as_str().filter(|s| !s.is_empty()) {
            run["model"] = json!(crate::resolve_model(m).map_err(|e| e.to_string())?);
        }
        if let Some(e) = args["effort"].as_str().filter(|s| !s.is_empty()) {
            run["effort"] = json!(crate::check_effort(e).map_err(|e| e.to_string())?);
        }
        if let Some(c) = args["context"].as_str().filter(|s| !s.is_empty()) {
            run["context"] = json!(crate::check_context(c).map_err(|e| e.to_string())?);
        }
        if let Some(i) = args["isolation"].as_str().filter(|s| !s.is_empty()) {
            if i != "worktree" {
                return Err("isolation must be 'worktree'".into());
            }
            run["isolation"] = json!(i);
        }
    }
    run["session"] = params
        .get("session")
        .cloned()
        .filter(|s| s.is_object())
        .unwrap_or_else(|| json!({"id": sid}));
    let v = backend("run", run, None, None).map_err(|e| e.to_string())?;
    let rids = v["run_ids"].as_array().cloned().unwrap_or_default();
    let names = v["names"].as_array().cloned().unwrap_or_default();
    let handles: Vec<Value> = rids
        .iter()
        .enumerate()
        .map(|(i, r)| json!({"id": r, "name": names.get(i).cloned().unwrap_or(Value::Null)}))
        .collect();
    Ok(json!({
        "handles": handles,
        "handle": handles.first().cloned().unwrap_or(Value::Null),
        "status": "admitted",
        "note": "admission returns handles, never the child's answer — results arrive via subagent_check, completion notices, or agent messages",
        "cli": v["content"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string(),
    })
    .to_string())
}

fn snapshot(e: &Entry, preview_lines: usize) -> Value {
    let (running, settled, activity, status) = match &e.row.state {
        AgentState::Running { activity } => (true, false, activity.clone(), "running".to_string()),
        AgentState::Queued => (false, false, String::new(), "queued".to_string()),
        AgentState::Completed => (false, true, String::new(), "completed".to_string()),
        AgentState::Failed { error } => (false, true, String::new(), format!("failed: {error}")),
        AgentState::Stopped => (false, true, String::new(), "stopped".to_string()),
    };
    let preview = backend(
        "view",
        json!({"run_id": e.target, "lines": preview_lines}),
        None,
        None,
    )
    .ok()
    .and_then(|v| v["content"].as_str().map(str::to_string))
    .unwrap_or_else(|| "(view unavailable)".into());
    json!({
        "name": e.row.name,
        "target": e.target,
        "status": if status.is_empty() { e.status.clone() } else { status },
        "running": running,
        "settled": settled,
        "activity": activity,
        "session": e.session,
        "preview": preview.chars().take(1200).collect::<String>(),
    })
}

/// `subagent_check` — non-steering typed snapshots; `wait_ms` polls until
/// every target settles (cap ~120s). Default targets: this session's runs.
fn subagent_check(params: &Value, args: &Value) -> Result<String, String> {
    let sid = session_id(params);
    let targets: Vec<String> = args["targets"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .or_else(|| args["targets"].as_str().map(|s| vec![s.to_string()]))
        .unwrap_or_else(|| {
            owned_runs(&sid)
                .iter()
                .filter_map(|j| j["name"].as_str().map(str::to_string))
                .collect()
        });
    if targets.is_empty() {
        return Ok("[]".into());
    }
    let wait_ms = args["wait_ms"]
        .as_u64()
        .unwrap_or(0)
        .min(COLLECT_MAX_WAIT_MS);
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let entries = menu_order(collect_entries(true, true).map_err(|e| e.to_string())?);
        let mut snaps = Vec::new();
        for name in &targets {
            match resolve_entry(&entries, name) {
                Some(e) => snaps.push(snapshot(e, 5)),
                None => snaps.push(json!({
                    "name": name, "status": "unknown",
                    "settled": true, "error": "no such child",
                })),
            }
        }
        let all_settled = snaps
            .iter()
            .all(|s| s["settled"].as_bool().unwrap_or(false));
        if all_settled || Instant::now() >= deadline {
            return Ok(serde_json::to_string_pretty(&snaps).unwrap_or_default());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `subagent_message` — steer a running child; a finished one's `chat` resumes its
/// session (prime's agent_message.send analogue).
fn subagent_message(args: &Value) -> Result<String, String> {
    let name = args["name"]
        .as_str()
        .ok_or("subagent_message requires name")?;
    let message = args["message"]
        .as_str()
        .ok_or("subagent_message requires message")?;
    let v = chat(&menu_target(name), vec![message.to_string()]).map_err(|e| e.to_string())?;
    Ok(v["content"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| serde_json::to_string_pretty(&v).unwrap_or_default()))
}

/// `subagent_stop` — stop a child by name/id (foreign workers included).
fn subagent_stop(params: &Value, args: &Value) -> Result<String, String> {
    let name = args["name"].as_str().ok_or("subagent_stop requires name")?;
    let target = menu_target(name);
    if let Some(v) = foreign_stop(&target) {
        return Ok(v["content"].as_str().unwrap_or("stopped").to_string());
    }
    let sid = session_id(params);
    let v = backend(
        "stop",
        json!({"run_id": target, "session": {"id": sid}}),
        None,
        None,
    )
    .map_err(|e| e.to_string())?;
    Ok(v["content"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| serde_json::to_string_pretty(&v).unwrap_or_default()))
}

/// `subagent_note` — report upward without steering: append to the
/// session notes file and (when host.say is granted) emit a say frame.
fn subagent_note(params: &Value, args: &Value) -> Result<String, String> {
    let sid = session_id(params);
    let msg = args["message"]
        .as_str()
        .ok_or("subagent_note requires message")?
        .chars()
        .take(512)
        .collect::<String>();
    let path = notes_file(&sid).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("cannot append note file: {e}"))?;
    writeln!(f, "- [{}] {}", now(), msg).map_err(|e| e.to_string())?;
    Ok(format!("noted → {}", path.display()))
}

// ---------- wire surface ----------

/// Names from before the rename. Sessions recorded earlier replay these
/// `rlm_*` calls, so the dispatcher still routes them to the same handlers;
/// `manifest_tools` never lists them.
const LEGACY_TOOL_NAMES: &[(&str, &str)] = &[
    ("rlm_spawn", "subagent_spawn"),
    ("rlm_collect", "subagent_check"),
    ("rlm_send", "subagent_message"),
    ("rlm_delete", "subagent_stop"),
    ("rlm_progress_note", "subagent_note"),
];

/// Canonical `subagent_*` name for `name`; names outside the table pass through.
fn canonical_tool(name: &str) -> &str {
    LEGACY_TOOL_NAMES
        .iter()
        .find(|&&(old, _)| old == name)
        .map_or(name, |&(_, new)| new)
}

pub fn is_subagent_tool(name: &str) -> bool {
    matches!(
        canonical_tool(name),
        "subagent_spawn"
            | "subagent_check"
            | "subagent_message"
            | "subagent_stop"
            | "subagent_note"
    )
}

/// One tool call → (tool-result value, extra outbound frames to print
/// first — currently just the optional host/say delivery).
pub fn handle_tool(params: &Value) -> (Value, Vec<Value>) {
    let name = canonical_tool(params["name"].as_str().unwrap_or(""));
    let args = &params["args"];
    let res: Result<String, String> = match name {
        "subagent_spawn" => subagent_spawn(params, args),
        "subagent_check" => subagent_check(params, args),
        "subagent_message" => subagent_message(args),
        "subagent_stop" => subagent_stop(params, args),
        "subagent_note" => subagent_note(params, args),
        other => {
            return (
                json!({"content": format!("unknown tool {other}"), "is_error": true}),
                Vec::new(),
            );
        }
    };
    let mut frames = Vec::new();
    if name == "subagent_note" && res.is_ok() {
        let msg = args["message"].as_str().unwrap_or("");
        frames.push(json!({"id": "subagent-say", "method": "host/say",
            "params": {"text": format!("subagent progress: {}", msg.chars().take(200).collect::<String>())}}));
    }
    match res {
        Ok(c) => (json!({"content": c}), frames),
        Err(e) => (json!({"content": e, "is_error": true}), frames),
    }
}

/// `/rlm …` — status/collect/stop/notes over this session's children.
pub fn command(params: &Value) -> String {
    let argv: Vec<String> = params["argv"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let sid = session_id(params);
    match argv.first().map(String::as_str) {
        Some("stop") => argv
            .get(1)
            .map(|n| subagent_stop(params, &json!({"name": n})).unwrap_or_else(|e| e))
            .unwrap_or_else(|| "usage: /rlm stop <name>".into()),
        Some("collect") => subagent_check(params, &json!({})).unwrap_or_else(|e| e),
        Some("notes") => notes_file(&sid)
            .and_then(|p| std::fs::read_to_string(p).map_err(|e| e.into()))
            .unwrap_or_else(|_: anyhow::Error| "(no notes yet)".into()),
        _ => {
            let owned = owned_runs(&sid);
            if owned.is_empty() {
                return "no subagent children this session — spawn via the subagent_spawn tool"
                    .into();
            }
            let entries = menu_order(collect_entries(true, true).unwrap_or_default());
            let mut lines = vec![format!("subagent children ({})", owned.len())];
            for j in &owned {
                let name = j["name"].as_str().unwrap_or("?");
                let status = resolve_entry(&entries, name)
                    .map(|e| match &e.row.state {
                        AgentState::Running { activity } => {
                            format!("running · {activity}")
                        }
                        _ => e.status.clone(),
                    })
                    .unwrap_or_else(|| j["status"].as_str().unwrap_or("?").to_string());
                lines.push(format!("  {name} — {status}"));
            }
            lines.join("\n")
        }
    }
}

/// `gray rlm <verb>` argv surface — read verbs work without a host.
pub fn cli(argv: &[String]) -> String {
    let params = json!({"session": {"id": "cli"}});
    match argv.first().map(String::as_str) {
        Some("collect") => subagent_check(&params, &json!({})).unwrap_or_else(|e| e),
        Some("stop") => argv
            .get(1)
            .map(|n| subagent_stop(&params, &json!({"name": n})).unwrap_or_else(|e| e))
            .unwrap_or_else(|| "usage: gray rlm stop <name>".into()),
        Some("notes") => command(&json!({"argv": ["notes"], "session": {"id": "cli"}})),
        _ => command(&params),
    }
}

pub fn manifest_tools() -> Value {
    json!([
        {"name": "subagent_spawn",
         "label": "Started agent",
         "preview": "{name} · {model|model} · {effort} · ctx {context}\n{prompt|firstline}",
         "description": "Admit a supervised subagent child (same supervisor as `gray subagents run`). Returns a handle immediately — never the child's answer. Depth-1 only; caps: max_running setting (default 128) concurrent, 32 total per session. `tasks` fans out 1–8 children in one admission.",
         "parameters": {"type": "object", "properties": {
            "prompt": {"type": "string", "description": "the child's task prompt"},
            "tasks": {"description": "batch spawn: 1–8 items, each {prompt|task, agent?, name?, model?, effort?, context?}"},
            "name": {"type": "string"}, "model": {"type": "string"},
            "effort": {"type": "string"},
            "context": {"type": "string", "description": "context ceiling passed as GRAY_COMPACT_AT: '90k', '300k', '1m', or 'auto'; above 200k a Claude model auto-uses its [1m] route — Haiku bills 5x past 100k"},
            "agent": {"type": "string", "description": "profile (scout|worker|oracle|reviewer)"},
            "tools": {"type": "string"}, "approve": {"type": "string"},
            "isolation": {"type": "string", "description": "e.g. worktree"}}}},
        {"name": "subagent_check",
         "label": "Checked agents",
         "preview": "{targets}",
         "description": "Non-steering typed snapshots of children: name, status, settled, activity, transcript-tail preview. wait_ms polls until all settle (max ~120s). Default targets: this session's runs.",
         "parameters": {"type": "object", "properties": {
            "targets": {"description": "child name or list of names"},
            "wait_ms": {"type": "integer"}}}},
        {"name": "subagent_message",
         "label": "Messaged",
         "preview": "{name}\n{message|firstline}",
         "description": "Message a child: steers a running run, resumes a finished one's session (agent_message analogue).",
         "parameters": {"type": "object", "properties": {
            "name": {"type": "string"}, "message": {"type": "string"}},
         "required": ["name", "message"]}},
        {"name": "subagent_stop",
         "label": "Stopped",
         "preview": "{name}",
         "description": "Stop a child run by name.",
         "parameters": {"type": "object", "properties": {
            "name": {"type": "string"}},
         "required": ["name"]}},
        {"name": "subagent_note",
         "label": "Note",
         "preview": "{message|firstline}",
         "description": "Report upward without steering: appends to this session's notes file (~/.gray/subagents/notes/<session>.md) and, when host.say is granted, delivers it to the parent.",
         "parameters": {"type": "object", "properties": {
            "message": {"type": "string", "description": "≤512 chars"}},
         "required": ["message"]}},
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_items_take_prompt_as_task_when_task_is_missing() {
        let items = vec![
            json!({"prompt": "from prompt", "name": "a"}),
            json!({"task": "keeps task", "prompt": "ignored"}),
            json!({"task": "", "prompt": "fills empty"}),
            json!({"name": "neither"}),
        ];
        let out = batch_items(&items);
        assert_eq!(out[0]["task"], "from prompt");
        assert_eq!(out[0]["name"], "a");
        assert_eq!(out[1]["task"], "keeps task");
        assert_eq!(out[2]["task"], "fills empty");
        assert!(out[3].get("task").is_none());
    }

    #[test]
    fn subagent_tools_declared() {
        let t = manifest_tools();
        let names: Vec<&str> = t
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for n in [
            "subagent_spawn",
            "subagent_check",
            "subagent_message",
            "subagent_stop",
            "subagent_note",
        ] {
            assert!(names.contains(&n), "missing {n}");
        }
        assert!(names.iter().all(|n| is_subagent_tool(n)));
        assert!(!is_subagent_tool("narrow_delegate"));
    }

    #[test]
    fn session_id_from_params() {
        assert_eq!(session_id(&json!({"session": {"id": "abc"}})), "abc");
        let p = json!({});
        // falls back to proc-derived root, else "default" — just never empty
        assert!(!session_id(&p).is_empty());
    }
}

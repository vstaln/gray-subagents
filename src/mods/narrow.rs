//! Folded from gray-narrow/src/main.rs — CLI, wire dispatch, spawner.
//! gray-narrow — capability narrowing for delegated gray children.
//!
//! Wire (NDJSON on stdio, protocol 1.1):
//!   tool/before   — deny any tool call not covered by GRAY_NARROW_GRANT
//!   prompt/context— tell a governed session what it holds
//!   command/run   — /narrow status|grants|ledger|help
//!   tool/call     — narrow_delegate: spawn a `gray -p` child with a grant ⊆ ours
//!
//! CLI (`gray narrow …` via cli_argv; bare `gray-narrow` is the sidecar):
//!   run     spawn a narrowed `gray -p` child (ledgered)
//!   plan    resolve a request against this process's grant; prints the env
//!           a spawner should set — the integration seam gray-subagents uses
//!   grants  show this session's grant and depth
//!   ledger  tail ~/.gray/narrow-ledger.jsonl
//!   status  list narrow-spawned runs

use std::path::PathBuf;
use std::process::Command;

use crate::narrow_lib::*;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

const USAGE: &str = "\
gray-narrow — capability narrowing for delegated children.

  gray narrow run [--tools CSV] [--agent PROFILE] [--approve CSV] 'task'
  gray narrow plan --request 'read,grep,bash:*'   (integration seam)
  gray narrow grants | status | ledger [--lines N]
  /narrow grants|ledger|help                     (in-session)

Grants only ever narrow: a child can never exceed its parent's set. Tool
patterns: 'read' = tool read, 'bash:git*' = bash calls whose command starts
'git', 'agent:scout' = may spawn the scout profile, '*' = everything.";

// ---------------------------------------------------------------------------
// manifest / wire
// ---------------------------------------------------------------------------

pub fn manifest_narrow() -> Value {
    json!({
        "name": "narrow",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "sidecar_args": ["sidecar"],
        "tools": [{
            "name": "narrow_delegate",
            "label": "Started restricted agent",
            "preview": "{name} · {tools}\n{task|firstline}",
            "description": "Spawn a detached `gray -p` child agent whose tool grant is a strict subset of this session's grant — the child can never exceed the parent set, and every grant is recorded in an append-only ledger (~/.gray/narrow-ledger.jsonl). tools is a comma-separated allowlist: 'read,grep' = those tools; 'bash:git*' = bash constrained to git commands; omit it to inherit this session's full grant. Gated capabilities (bash) need approve='tool:bash'. Use for work that should run with least privilege.",
            "parameters": {
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "the child's assignment"},
                    "tools": {"type": "string", "description": "comma-separated capability allowlist, e.g. 'read,grep,bash:git*'; omit to inherit"},
                    "agent": {"type": "string", "description": "agent profile name from ~/.gray/subagents/agents/<name>.md"},
                    "name": {"type": "string", "description": "short descriptive run label like 'auth-scout' or 'fix-flaky-test' [a-zA-Z0-9][a-zA-Z0-9_-]* — shown in the Agents panel; always pass a meaningful name so runs are identifiable (a task-derived slug is used when omitted)"},
                    "approve": {"type": "string", "description": "comma-separated gated capabilities to approve, e.g. 'tool:bash'"}
                },
                "required": ["task"]
            }
        }],
        "commands": ["/narrow"],
        "hooks": ["tool/before", "prompt/context"],
        "subcommands": ["narrow"],
        "completion": ["run", "plan", "grants", "status", "ledger", "help"],
    })
}

fn tool_before_narrow(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("args").cloned().unwrap_or(json!({}));
    let session = params["session"]["id"].as_str().unwrap_or("");
    match grant_allows(name, &args) {
        None | Some(true) => json!({"decision": "allow"}),
        Some(false) => {
            let grant = current_grant().unwrap_or_default();
            let _ = append_ledger(&deny_event(
                session,
                name,
                &primary_arg(name, &args),
                &grant,
            ));
            json!({
                "decision": "deny",
                "reason": format!("{name} is outside this session's narrowed grant ({}) — logged in the narrowing ledger; finish without it or report the blocker", grant.join(","))
            })
        }
    }
}

fn prompt_context_narrow() -> Value {
    let Some(grant) = current_grant() else {
        return json!({"text": ""});
    };
    let (depth, max, _) = depth_config();
    let held = if grant.is_empty() {
        "nothing (empty grant — every tool call will be denied)".to_string()
    } else {
        grant.join(", ")
    };
    json!({"text": format!(
        "[narrow] This session's tool grant is narrowed to: {held} (depth {depth}/{max}). \
         Calls outside it are denied and logged in the narrowing ledger. You may delegate \
         further with the narrow_delegate tool or `gray narrow run` (which need tool:narrow_delegate \
         / tool:bash in the grant) — grants can only shrink. \
         If a needed tool is denied, report it rather than retrying."
    )})
}

// ---------------------------------------------------------------------------
// resolution → env for a child (shared by `run`, `plan`, `narrow_delegate`)
// ---------------------------------------------------------------------------

struct Planned {
    run_id: String,
    env: Vec<(String, String)>,
    input: ResolveInput,
    out: ResolveOutput,
    depth: u32,
    max_depth: u32,
}

/// Resolve `request_csv` against this process's grant. `via` labels the
/// ledger event. Err(msg) when the delegation is refused (denied entries,
/// gated without approval, empty result, depth cap, bad ids) — callers
/// propagate the message to the user and do NOT spawn.
fn plan(
    request_csv: Option<&str>,
    agent: Option<&str>,
    approve_csv: Option<&str>,
    via: &str,
    cwd: &str,
    run_id: Option<String>,
) -> Result<Planned, String> {
    let (depth, max_depth, malformed) = depth_config();
    if !malformed.is_empty() {
        return Err(format!(
            "refusing to delegate: {} is malformed (non-negative integers only); grant resolution fails closed",
            malformed.join(", ")
        ));
    }
    if depth >= max_depth {
        return Err(format!(
            "refusing to delegate: depth {depth} is at the narrowing bound {max_depth}"
        ));
    }
    let parent = current_grant().unwrap_or_else(|| vec![WILDCARD.to_string()]);
    let requested = match request_csv {
        None => parent.clone(),
        Some(csv) => parse_capability_list(csv)?,
    };
    if let Some(a) = agent {
        // Spawning a named profile consumes agent authority; it is checked
        // against the parent grant but not carried into the child's grant.
        if !may_spawn_agent(&parent, a) {
            return Err(format!("this session's grant does not cover agent:{a}"));
        }
    }
    let approved = match approve_csv {
        Some(csv) => parse_capability_list(csv)?,
        None => Vec::new(),
    };
    let input = ResolveInput {
        requested,
        parent_grant: parent,
        ceiling: None,
        gated: DEFAULT_GATED.iter().map(|s| s.to_string()).collect(),
        approved,
    };
    let out = resolve(&input);
    let run_id = run_id.unwrap_or_else(|| {
        format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        )
    });

    if !out.denied.is_empty() {
        let ev = grant_event(
            via,
            &run_id,
            agent,
            cwd,
            depth + 1,
            max_depth,
            &input,
            &out,
            "refused",
            "requested capabilities not held by parent",
        );
        let _ = append_ledger(&ev);
        return Err(format!(
            "refused — not held by this session's grant: {}",
            out.denied.join(", ")
        ));
    }
    if !out.gated_blocked.is_empty() {
        let ev = grant_event(
            via,
            &run_id,
            agent,
            cwd,
            depth + 1,
            max_depth,
            &input,
            &out,
            "refused",
            "gated capabilities not approved",
        );
        let _ = append_ledger(&ev);
        return Err(format!(
            "refused — gated capabilities need explicit approval: {} (pass --approve '{}')",
            out.gated_blocked.join(", "),
            out.gated_blocked.join(",").replace("tool:", "")
        ));
    }
    if out.effective.is_empty() {
        let ev = grant_event(
            via,
            &run_id,
            agent,
            cwd,
            depth + 1,
            max_depth,
            &input,
            &out,
            "refused",
            "empty effective grant",
        );
        let _ = append_ledger(&ev);
        return Err(
            "refused — the resolved grant is empty (a no-tool child cannot work)".to_string(),
        );
    }
    let grant_value = grant_env_value(&out.effective)?;
    let env = vec![
        (ENV_GRANT.to_string(), grant_value),
        (ENV_DEPTH.to_string(), (depth + 1).to_string()),
        (ENV_MAX_DEPTH.to_string(), max_depth.to_string()),
        (ENV_RUN.to_string(), run_id.clone()),
    ];
    Ok(Planned {
        run_id,
        env,
        input,
        out,
        depth,
        max_depth,
    })
}

// ---------------------------------------------------------------------------
// run: spawn a detached `gray -p` child under the resolved grant
// ---------------------------------------------------------------------------

fn load_profile(name: &str) -> Result<(String, Value), String> {
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || name.len() > 64
    {
        return Err(format!("invalid agent name '{name}'"));
    }
    let path = crate::narrow_lib::gray_home()
        .join("subagents/agents")
        .join(format!("{name}.md"));
    let text = std::fs::read_to_string(&path)
        .map_err(|_| format!("unknown agent '{name}' (no {})", path.display()))?;
    // Same `---` frontmatter convention as gray-subagents: model/effort/tools.
    let mut meta = json!({});
    let body = if let Some(rest) = text.strip_prefix("---\n") {
        match rest.find("\n---") {
            Some(end) => {
                for line in rest[..end].lines() {
                    if let Some((k, v)) = line.split_once(':') {
                        meta[k.trim().to_lowercase()] = json!(v.trim());
                    }
                }
                rest[end + 4..].trim_start_matches('\n').to_string()
            }
            None => text.trim().to_string(),
        }
    } else {
        text.trim().to_string()
    };
    if body.is_empty() {
        return Err(format!("profile '{name}' has an empty body"));
    }
    Ok((body, meta))
}

fn gray_binary() -> String {
    std::env::var("GRAY_SUBAGENTS_BIN")
        .or_else(|_| std::env::var("GRAY_BIN"))
        .unwrap_or_else(|_| "gray".to_string())
}

fn child_prompt(agent: Option<&str>, task: &str, grant_csv: &str) -> String {
    let profile = agent
        .and_then(|a| load_profile(a).ok())
        .map(|(body, _)| format!("{body}\n\n"))
        .unwrap_or_default();
    let who = agent.unwrap_or("worker");
    format!(
        "You are the '{who}' subagent of a parent Gray session, delegated under a \
         NARROWED TOOL GRANT: {grant_csv}.\n\n\
         {profile}# Task\n\n{task}\n\n\
         Tool calls outside your grant are denied and logged; do not try to widen it. \
         Work only on this assignment. Do not spawn more agents unless your grant allows it. \
         If blocked, report the blocker; do not guess unapproved decisions. \
         Return findings, changed paths, and verification in your final answer."
    )
}

fn spawn_child(
    prompt: &str,
    env: &[(String, String)],
    cwd: &str,
    run_id: &str,
    name: &str,
    route: Option<&crate::model_catalog::Route>,
    effort: Option<&str>,
) -> Result<u32, String> {
    let dir = runs_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let log =
        std::fs::File::create(dir.join(format!("{run_id}.log"))).map_err(|e| e.to_string())?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(gray_binary());
    cmd.arg("-p")
        .arg(prompt)
        .arg("--json")
        .arg("--max-requests")
        .arg(std::env::var("GRAY_SUBAGENTS_MAX_REQUESTS").unwrap_or_else(|_| "256".into()))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(err));
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Matches gray-subagents' child env: marks the session auxiliary and the
    // run detached; GRAY_SUBAGENTS_ACTIVE keeps its own recursion guard working.
    cmd.env("GRAY_SUBAGENTS_ACTIVE", "1")
        .env("GRAY_SESSION_ORIGIN", "subagent")
        .env("GRAY_AGENT_NAME", name)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("GRAY_SHOW_REASONING", "0");
    if let Some(route) = route {
        // A routed spec carries the child provider identity env; a stale
        // GRAY_BASE_URL must not override a plugin's declared transport.
        if !route.env.is_empty() {
            cmd.env_remove("GRAY_BASE_URL");
            for (k, v) in &route.env {
                cmd.env(k, v);
            }
        }
        cmd.env("GRAY_MODEL", &route.model);
    }
    if let Some(e) = effort {
        cmd.env("GRAY_THINKING_EFFORT", e);
    }
    // Own process group: the run outlives this spawner like gray-subagents'
    // detached children (start_new_session equivalent for our purposes).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map(|c| c.id())
        .map_err(|e| format!("could not spawn {}: {e}", gray_binary()))
}

fn run_record(
    run_id: &str,
    name: &str,
    agent: Option<&str>,
    task: &str,
    cwd: &str,
    grant: &[String],
    pid: u32,
) {
    let rec = json!({
        "run_id": run_id, "name": name, "agent": agent.unwrap_or(""),
        "task": task, "cwd": cwd, "pid": pid,
        "grant": grant, "status": "running",
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or_default(),
    });
    let _ = std::fs::write(runs_dir().join(format!("{run_id}.json")), rec.to_string());
}

#[allow(clippy::too_many_arguments)]

/// Fallback run label: first words of the task slugified, suffixed with a
/// short run-id for uniqueness — "e2e-smoke-7f3a" beats "run-7f3a1b2c".
fn name_from_task(task: &str, run_id: &str) -> String {
    let mut slug = String::new();
    for w in task.split_whitespace() {
        let w: String = w
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let w = w.trim_matches('-');
        if w.is_empty() {
            continue;
        }
        let would = slug.len() + usize::from(!slug.is_empty()) + w.len();
        if would > 24 && !slug.is_empty() {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&w[..w.len().min(24)]);
    }
    if slug.is_empty() {
        slug = "run".to_string()
    }
    format!("{slug}-{}", &run_id[..4])
}

fn do_run_narrow(
    task: &str,
    tools: Option<&str>,
    agent: Option<&str>,
    name: Option<&str>,
    approve: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    cwd: &str,
    via: &str,
    session: &str,
) -> Result<String, String> {
    let planned = plan(tools, agent, approve, via, cwd, None)?;
    let grant_csv = grant_env_value(&planned.out.effective)?;
    let prompt = child_prompt(agent, task, &grant_csv);
    let name = name
        .map(str::to_string)
        .unwrap_or_else(|| name_from_task(task, &planned.run_id));
    // provider/model specs onto an enabled plugin provider resolve to the bare
    // id plus the provider env the child's connection needs.
    let route = match model {
        Some(m) => Some(
            crate::model_catalog::resolve_route(m, &crate::narrow_lib::gray_home())
                .map_err(|e| format!("model '{m}': {e}"))?,
        ),
        None => None,
    };
    let arrow = route
        .as_ref()
        .filter(|r| !r.label.is_empty())
        .map(|r| format!(" → {}", r.label))
        .unwrap_or_default();
    // Forward the owning-session marker: the child's own descendants
    // (and foreign_scan adoption) scope back to this session's tree.
    let mut env = planned.env.clone();
    if !session.is_empty() {
        env.push(("GRAY_SESSION_ID".into(), session.into()));
    }
    let pid = spawn_child(&prompt, &env, cwd, &planned.run_id, &name, route.as_ref(), effort)?;
    let ev = grant_event(
        via,
        &planned.run_id,
        agent,
        cwd,
        planned.depth + 1,
        planned.max_depth,
        &planned.input,
        &planned.out,
        "granted",
        "",
    );
    let _ = append_ledger(&ev);
    run_record(
        &planned.run_id,
        &name,
        agent,
        task,
        cwd,
        &planned.out.effective,
        pid,
    );
    let mut note = String::new();
    if !planned.out.subsumed_by.is_empty() {
        note = format!(
            " (subsumed by parent grant: {})",
            planned.out.subsumed_by.join(", ")
        );
    }
    Ok(format!(
        "Started narrowed run {name} [{}] pid {pid}{arrow} — grant: {grant_csv}{note}. \
         Ledger: {}. Log: {}",
        planned.run_id,
        ledger_path().display(),
        runs_dir().join(format!("{}.log", planned.run_id)).display()
    ))
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "gray-narrow",
    version,
    about = "capability narrowing for delegated gray children",
    disable_help_subcommand = true
)]
struct NarrowCli {
    #[command(subcommand)]
    command: Option<NarrowCmd>,
}

#[derive(Subcommand)]
enum NarrowCmd {
    /// Spawn a detached `gray -p` child under a narrowed grant.
    Run {
        #[arg(required = true, num_args = 1..)]
        task: Vec<String>,
        /// Comma-separated allowlist: read,grep,bash:git*,agent:scout,*
        #[arg(long)]
        tools: Option<String>,
        /// Agent profile (~/.gray/subagents/agents/<name>.md)
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Approve gated capabilities (e.g. 'tool:bash')
        #[arg(long)]
        approve: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        /// Working directory for the child (default: cwd)
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Resolve a request against this process's grant; print the child env as
    /// JSON and append the grant event to the ledger. The integration seam:
    /// gray-subagents calls this before spawning a `gray -p` run.
    Plan {
        /// Requested capabilities CSV; omit to inherit the full grant.
        #[arg(long)]
        request: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        approve: Option<String>,
        /// Label for the ledger's `via` field (e.g. subagents)
        #[arg(long, default_value = "plan")]
        via: String,
        /// Run id to record (default: generated)
        #[arg(long)]
        run_id: Option<String>,
    },
    /// Show this process's grant and delegation depth.
    Grants,
    /// List narrow-spawned runs under this GRAY_HOME.
    Status,
    /// Tail the narrowing ledger.
    Ledger {
        #[arg(long, default_value = "20")]
        lines: usize,
    },
    /// Print the plugin manifest_narrow(host install probe).
    Manifest,
    /// Run the sidecar loop (what the host spawns).
    Sidecar,
    Help,
}

fn do_plan_narrow(args: NarrowPlanArgs) -> Result<Value, String> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let planned = plan(
        args.request.as_deref(),
        args.agent.as_deref(),
        args.approve.as_deref(),
        &args.via,
        &cwd,
        args.run_id,
    )?;
    let ev = grant_event(
        &args.via,
        &planned.run_id,
        args.agent.as_deref(),
        &cwd,
        planned.depth + 1,
        planned.max_depth,
        &planned.input,
        &planned.out,
        "granted",
        "",
    );
    let _ = append_ledger(&ev);
    let env: serde_json::Map<String, Value> = planned
        .env
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    Ok(json!({
        "run_id": planned.run_id,
        "env": env,
        "effective": planned.out.effective,
        "denied": planned.out.denied,
        "clipped": planned.out.clipped,
        "gated_blocked": planned.out.gated_blocked,
        "subsumed_by": planned.out.subsumed_by,
        "depth": planned.depth + 1,
        "max_depth": planned.max_depth,
        "ledger": ledger_path().to_string_lossy(),
    }))
}

struct NarrowPlanArgs {
    request: Option<String>,
    agent: Option<String>,
    approve: Option<String>,
    via: String,
    run_id: Option<String>,
}

fn do_grants_narrow() -> String {
    let (depth, max, malformed) = depth_config();
    let grant = current_grant()
        .map(|g| {
            if g.is_empty() {
                "(empty — denies everything)".into()
            } else {
                g.join(", ")
            }
        })
        .unwrap_or_else(|| "(ungoverned — no GRAY_NARROW_GRANT; all tools allowed)".into());
    let mut s = format!(
        "grant: {grant}\ndepth: {depth}/{max}\nledger: {}",
        ledger_path().display()
    );
    if !malformed.is_empty() {
        s.push_str(&format!(
            "\nWARNING: malformed depth env(s): {} — delegation is refused",
            malformed.join(", ")
        ));
    }
    s
}

fn do_status_narrow() -> String {
    let mut rows = vec!["run_id     name                 pid      alive  grant".to_string()];
    let mut found = false;
    if let Ok(rd) = std::fs::read_dir(runs_dir()) {
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
        {
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(rec) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            found = true;
            let pid = rec["pid"].as_u64().unwrap_or(0);
            let alive = pid > 0 && PathBuf::from(format!("/proc/{pid}")).exists();
            rows.push(format!(
                "{:<10} {:<20} {:<8} {:<5} {}",
                &rec["run_id"].as_str().unwrap_or("")
                    [..8.min(rec["run_id"].as_str().unwrap_or("").len())],
                rec["name"].as_str().unwrap_or(""),
                pid,
                if alive { "yes" } else { "no" },
                rec["grant"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default()
            ));
        }
    }
    if !found {
        rows.push("no narrow runs recorded".into());
    }
    rows.join("\n")
}

fn do_ledger_narrow(lines: usize) -> String {
    let path = ledger_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let all: Vec<&str> = text.lines().collect();
            let start = all.len().saturating_sub(lines);
            all[start..].join("\n")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            format!("no ledger yet ({})", path.display())
        }
        Err(e) => format!("cannot read {}: {e}", path.display()),
    }
}

fn run_command_narrow(argv: &[&str]) -> String {
    match argv.first().copied() {
        Some("grants") => do_grants_narrow(),
        Some("status") => do_status_narrow(),
        Some("ledger") => do_ledger_narrow(20),
        Some("run") | Some("plan") => {
            let full: Vec<String> = std::iter::once("gray-narrow".to_string())
                .chain(argv.iter().map(|s| s.to_string()))
                .collect();
            match NarrowCli::try_parse_from(&full) {
                Ok(cli) => match cli.command.unwrap_or(NarrowCmd::Help) {
                    NarrowCmd::Run {
                        task,
                        tools,
                        agent,
                        name,
                        approve,
                        model,
                        effort,
                        cwd,
                    } => {
                        let cwd = cwd.unwrap_or_else(|| {
                            std::env::current_dir()
                                .map(|p| p.to_string_lossy().into_owned())
                                .unwrap_or_else(|_| ".".into())
                        });
                        do_run_narrow(
                            &task.join(" "),
                            tools.as_deref(),
                            agent.as_deref(),
                            name.as_deref(),
                            approve.as_deref(),
                            model.as_deref(),
                            effort.as_deref(),
                            &cwd,
                            "command",
                            &std::env::var("GRAY_SESSION_ID").unwrap_or_default(),
                        )
                        .unwrap_or_else(|e| format!("refused: {e}"))
                    }
                    NarrowCmd::Plan {
                        request,
                        agent,
                        approve,
                        via,
                        run_id,
                    } => {
                        match do_plan_narrow(NarrowPlanArgs {
                            request,
                            agent,
                            approve,
                            via,
                            run_id,
                        }) {
                            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
                            Err(e) => format!("refused: {e}"),
                        }
                    }
                    _ => USAGE.to_string(),
                },
                Err(e) => format!("{e}"),
            }
        }
        _ => USAGE.to_string(),
    }
}

/// tool/call → narrow_delegate: same spawn path as `narrow run`.
fn tool_call_narrow(params: &Value) -> Value {
    let args = &params["args"];
    let task = args["task"].as_str().unwrap_or("").trim();
    if task.is_empty() || task.len() > 32000 || task.contains('\0') {
        return json!({"content": "narrow_delegate: task must be 1–32000 non-NUL characters", "is_error": true});
    }
    let cwd = params["session"]["cwd"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| ".".into());
    match do_run_narrow(
        task,
        args["tools"].as_str(),
        args["agent"].as_str(),
        args["name"].as_str(),
        args["approve"].as_str(),
        None,
        None,
        &cwd,
        "delegate-tool",
        params["session"]["id"].as_str().unwrap_or(""),
    ) {
        Ok(text) => json!({"content": text}),
        Err(e) => json!({"content": format!("narrow_delegate refused: {e}"), "is_error": true}),
    }
}

pub fn handle_narrow(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => Ok(manifest_narrow()),
        "tool/before" => Ok(tool_before_narrow(&params)),
        "prompt/context" => Ok(prompt_context_narrow()),
        "tool/call" => Ok(tool_call_narrow(&params)),
        "command/run" => {
            let argv: Vec<&str> = params["argv"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            Ok(json!({"text": run_command_narrow(&argv)}))
        }
        "plugin/shutdown" => return (Some(json!({"id": id, "result": {}})), true),
        _ => Err(format!("method not found: {method}")),
    };
    let reply = match result {
        Ok(result) => json!({"id": id, "result": result}),
        Err(e) => json!({"id": id, "error": {"code": -32601, "message": e}}),
    };
    (Some(reply), false)
}

/// In-process `gray narrow …` — `args` are the user's argv (no binary name,
/// no "narrow"). `Err("__sidecar")` marks "the sidecar loop was asked for".
pub fn narrow_cli(args: Vec<String>) -> Result<String, String> {
    let argv: Vec<String> = std::iter::once("narrow".to_string()).chain(args).collect();
    let cli = NarrowCli::parse_from(&argv);
    let default_cwd = || {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into())
    };
    let out: Result<String, String> = match cli.command.unwrap_or(NarrowCmd::Sidecar) {
        NarrowCmd::Manifest => Ok(manifest_narrow().to_string()),
        NarrowCmd::Sidecar => return Err("__sidecar".into()),
        NarrowCmd::Run {
            task,
            tools,
            agent,
            name,
            approve,
            model,
            effort,
            cwd,
        } => do_run_narrow(
            &task.join(" "),
            tools.as_deref(),
            agent.as_deref(),
            name.as_deref(),
            approve.as_deref(),
            model.as_deref(),
            effort.as_deref(),
            &cwd.unwrap_or_else(default_cwd),
            "narrow-run",
            &std::env::var("GRAY_SESSION_ID").unwrap_or_default(),
        ),
        NarrowCmd::Plan {
            request,
            agent,
            approve,
            via,
            run_id,
        } => do_plan_narrow(NarrowPlanArgs {
            request,
            agent,
            approve,
            via,
            run_id,
        })
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()),
        NarrowCmd::Grants => Ok(do_grants_narrow()),
        NarrowCmd::Status => Ok(do_status_narrow()),
        NarrowCmd::Ledger { lines } => Ok(do_ledger_narrow(lines)),
        NarrowCmd::Help => Ok(USAGE.to_string()),
    };
    out
}

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gray_subagents::widget::{
    AgentRow, AgentState, pretty_model, render_menu, render_widget_spans, state_icon,
};
use serde_json::{Value, json};

#[path = "mods/narrow.rs"]
mod narrow;
#[path = "mods/narrow_lib.rs"]
pub mod narrow_lib;
#[path = "mods/rlm.rs"]
mod rlm;
mod model_catalog;
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    process::Command,
};

#[derive(Parser)]
#[command(
    name = "gray-subagents",
    version,
    about = "Gray subagent plugin: Bash commands and an on-demand agents panel"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    Settings {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        /// Context ceiling for children, exported as GRAY_COMPACT_AT
        /// ("90k", "300k", "1m", or "auto").
        #[arg(long)]
        context: Option<String>,
        #[arg(long)]
        max_running: Option<u32>,
    },
    Setup,
    Run {
        #[arg(required = true, num_args = 1..)]
        task: Vec<String>,
        #[arg(long, default_value = "scout")]
        agent: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        /// Context ceiling for this child (GRAY_COMPACT_AT): "90k", "300k",
        /// "1m", or "auto". Above 200k a Claude model takes its [1m] route.
        #[arg(long)]
        context: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Capability allowlist for the child (gray-narrow), e.g.
        /// "read,grep,bash:*". Requires gray-narrow installed; the grant can
        /// only ever shrink a parent's own GRAY_NARROW_GRANT.
        #[arg(long)]
        tools: Option<String>,
        /// Approve gated capabilities for this run (e.g. "tool:bash").
        #[arg(long)]
        approve: Option<String>,
        /// Run the child in its own git worktree — use it when parallel
        /// runs edit the same repository so they cannot collide.
        #[arg(long)]
        isolation: Option<String>,
    },
    Steer {
        run_id: String,
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
    },
    /// JSON row data for the host's interactive agents panel.
    Entries {
        /// Include runs from every session, not just this one.
        #[arg(long)]
        all: bool,
    },
    /// Show this session's selectable Agents menu.
    Menu,
    /// Enter a run's session: prints the `/resume` line for finished runs,
    /// the live controls for running ones.
    Open {
        target: String,
    },
    /// Talk to a run — queues a follow-up into its child session (alias for
    /// steer; finished runs resume their session).
    Chat {
        target: String,
        #[arg(num_args = 0..)]
        message: Vec<String>,
    },
    Status {
        run_id: Option<String>,
        /// Show every run across all sessions, not just this session's.
        #[arg(long)]
        all: bool,
    },
    /// Print a run or worker's transcript tail (task + tool calls + replies).
    View {
        /// Bare `view` shows the most recent run.
        target: Option<String>,
        #[arg(long, default_value = "40")]
        lines: usize,
    },
    /// Cancel a run by name/id, `all` for every running run, or bare for
    /// just this session's runs.
    Stop {
        run_id: Option<String>,
    },
    List,
    Widget {
        #[arg(long)]
        demo: bool,
        /// Skip the first N agent rows when rendering the band (paging).
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    Manifest,
}
pub(crate) fn home() -> Result<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".gray")))
        .context("cannot resolve Gray home")
}
fn settings(
    model: Option<String>,
    cap: Option<u32>,
    effort: Option<String>,
    context: Option<String>,
) -> Result<Value> {
    let path = home()?.join("subagents/settings.json");
    let mut value: Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({"max_running":4,"model":null}),
        Err(e) => return Err(e.into()),
    };
    anyhow::ensure!(value.is_object(), "settings must be an object");
    let changed = model.is_some() || cap.is_some() || effort.is_some() || context.is_some();
    if let Some(model) = model {
        let model = resolve_model(&model)?;
        value["model"] = json!(model);
    }
    if let Some(effort) = effort {
        value["effort"] = json!(check_effort(&effort)?);
    }
    if let Some(context) = context {
        value["context"] = json!(check_context(&context)?);
    }
    if let Some(cap) = cap {
        anyhow::ensure!((1..=32).contains(&cap), "max-running must be 1–32");
        value["max_running"] = json!(cap);
    }
    if changed {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        use std::io::Write;
        writeln!(file, "{}", serde_json::to_string_pretty(&value)?)?;
        file.persist(path)?;
    }
    Ok(value)
}
// Retain the tested Python process supervisor while the UI/CLI is native Rust.
// Embedded source makes the installed executable independent of the source tree.
pub(crate) fn backend(
    action: &str,
    args: Value,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Value> {
    let cfg = settings(None, None, None, None)?;
    let script = concat!(
        "import sys,json\nns={'__name__':'gray_subagents_backend','__file__':sys.argv[3]}\nexec(sys.argv[1],ns)\nargs=json.loads(sys.argv[2])\naction=sys.argv[4]\n",
        "if action=='run': result=ns['spawn_jobs'](args,args.get('session') or {'cwd':__import__('os').getcwd()})\n",
        "elif action=='status': result=ns['status_job'](args.get('run_id'))\n",
        "elif action=='view': result=ns['view_transcript'](args.get('run_id'),args.get('lines'))\n",
        "elif action=='stop': result=ns['stop_job'](args.get('run_id'),(args.get('session') or {}).get('id',''))\n",
        "elif action=='steer': result=ns['steer_job'](args['run_id'],args['message'])\n",
        "elif action=='context': result={'text':ns['completion_notices'](args.get('cwd'),(args.get('session') or {}).get('id',''))}\n",
        "elif action=='notices': result={'text':ns['completion_notices'](args.get('cwd'),(args.get('session') or {}).get('id',''),False)}\n",
        "elif action=='setup': result={'content':ns['setup_profiles']()}\n",
        "elif action=='list': result=ns['list_profiles']()\n",
        "print(json.dumps(result))\n"
    );
    // ValueError is the supervisor's user-facing refusal ("unknown run",
    // "name reserved"): surface its message, not a Python traceback.
    let script = format!(
        "try:\n{}\nexcept ValueError as e:\n    sys.stderr.write(str(e)+'\\n'); sys.exit(1)\n",
        script
            .lines()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let dir = home()?.join("subagents/runtime");
    std::fs::create_dir_all(&dir)?;
    let runtime = dir.join("supervisor.py");
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    use std::io::Write;
    file.write_all(include_bytes!("../mod"))?;
    file.persist(&runtime)?;
    let mut cmd = Command::new("python3");
    // gray-narrow is folded into this binary: default GRAY_NARROW_BIN to
    // ourselves so `run --tools` works without a separate install.
    if std::env::var_os("GRAY_NARROW_BIN").is_none() {
        if let Ok(me) = std::env::current_exe() {
            cmd.env("GRAY_NARROW_BIN", me);
        }
    }
    cmd.args(["-c", &script, include_str!("../mod"), &args.to_string()])
        .arg(runtime)
        .arg(action);
    if let Some(cap) = cfg["max_running"].as_u64() {
        cmd.env("GRAY_SUBAGENTS_MAX_RUNNING", cap.to_string());
    }
    if let Some(model) = model.or(cfg["model"].as_str()) {
        cmd.env("GRAY_MODEL", model);
    }
    if let Some(effort) = effort.or(cfg["effort"].as_str()) {
        cmd.env("GRAY_THINKING_EFFORT", effort);
    }
    if let Some(context) = cfg["context"].as_str() {
        cmd.env("GRAY_COMPACT_AT", context);
    }
    if let Some(bin) = std::env::var_os("GRAY_BIN") {
        cmd.env("GRAY_SUBAGENTS_BIN", bin);
    }
    // Owning-session marker for the whole tree under this call: the
    // supervisor's children inherit os.environ, so GRAY_SESSION_ID here
    // scopes grandchildren to the caller's session without /proc sniffs.
    if let Some(sid) = args["session"]["id"].as_str().filter(|s| !s.is_empty()) {
        cmd.env("GRAY_SESSION_ID", sid);
    }
    let out = cmd
        .output()
        .context("Python 3 is required for the process supervisor")?;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(serde_json::from_slice(&out.stdout)?)
}
fn manifest() -> Value {
    let mut m = json!({"name":"subagents","version":env!("CARGO_PKG_VERSION"),"protocol":"1.1","widget":true,"tools":[],"capabilities":["widget.override","host.say"],"completion":["settings","setup","run","status","view","stop","steer","list","open","chat","narrow","rlm"],"commands":["/subagents","/narrow","/rlm"],"hooks":["prompt/context","tool/before"],"subcommands":["subagents","narrow","rlm"]});
    // Folded-in surfaces ride this manifest too: gray-narrow's tools
    // (narrow_delegate) plus gray-rlm's rlm_* model-facing tools.
    let mut tools: Vec<Value> = narrow::manifest_narrow()
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    tools.extend(
        rlm::manifest_tools()
            .as_array()
            .cloned()
            .unwrap_or_default(),
    );
    m["tools"] = json!(tools);
    m
}
fn widget(demo: bool, offset: usize) -> Result<Value> {
    let mut rows = Vec::new();
    if demo {
        rows.push(AgentRow {
            name: "auth-scout".into(),
            description: "Find authentication entry points".into(),
            stats: "14.2s".into(),
            model: "Haiku 5.5".into(),
            result: String::new(),
            state: AgentState::Running {
                activity: "searching…".into(),
            },
        });
        rows.push(AgentRow {
            name: "Reviewer".into(),
            description: "Check session handling".into(),
            stats: "38.4s".into(),
            model: "Haiku 5.5".into(),
            result: String::new(),
            state: AgentState::Running {
                activity: "reading…".into(),
            },
        });
    } else {
        rows = collect_entries(false, false)?
            .into_iter()
            .map(|entry| entry.row)
            .collect();
    }
    let (text, ranges) = render_widget_spans(&rows, offset);
    // `shimmer_lines` stays for hosts that predate `shimmer_ranges`.
    let mut shimmer: Vec<usize> = ranges.iter().map(|r| r.0).collect();
    shimmer.dedup();
    Ok(json!({"version":1,"text":text,"shimmer_lines":shimmer,"shimmer_ranges":ranges}))
}

/// One selectable Agents-menu row: the widget's [`AgentRow`] display plus
/// the identity every verb resolves and the run's own gray session.
pub(crate) struct Entry {
    pub row: AgentRow,
    /// What `view`/`stop`/`steer`/`chat`/`open` accept: the run_id for
    /// managed runs; for adopted `gray -p` workers the session slug when
    /// known (foreign_session_id resolves it), else the worker name.
    pub target: String,
    /// The run's own gray session — a real resumable session, the same
    /// model as opencode's child sessions (`parent_id`) and Claude Code's
    /// job sessions. Empty while the child has not reached one, or for
    /// foreign workers holding no `.open` file.
    pub session: String,
    /// Registry status verbatim; adopted workers are always "running".
    pub status: String,
    /// Registry runs accept steer/chat; raw `gray -p` workers do not.
    pub managed: bool,
    /// Creation time — `last` selects the newest run regardless of the
    /// finished-first display grouping.
    pub created: f64,
}

/// This session's runs plus adopted `gray -p` workers, scoped exactly like
/// the widget (owner session, legacy cwd, or in-tree). `include_stale`
/// keeps finished runs past the widget's 30s fade — the menu selects old
/// runs for `open`/`chat`.
pub(crate) fn collect_entries(include_stale: bool, all: bool) -> Result<Vec<Entry>> {
    let cwd = std::env::current_dir()?;
    let my_sid = session_root().and_then(session_of_pid).unwrap_or_default();
    let dir = home()?.join("subagents/runs");
    let mut managed_pids = std::collections::HashSet::new();
    let mut entries = Vec::new();
    if dir.is_dir() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64();
        let mut jobs = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let job: Value = serde_json::from_slice(&std::fs::read(path)?)?;
            jobs.push(job);
        }
        jobs.sort_by(|a, b| {
            a["created"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&b["created"].as_f64().unwrap_or(0.0))
        });
        for job in jobs {
            let status = job["status"].as_str().unwrap_or("lost");
            let active = matches!(status, "running" | "stopping");
            // A queued run holds no slot and has no `finished` stamp yet, so it
            // is live for the stale-row filter as well.
            let live = active || status == "queued";
            if active
                && let Some(pid) = job["child_pid"].as_u64()
                && match job["child_birth"].as_str() {
                    Some(birth) => {
                        start_ticks(pid).map(|t| t.to_string()).as_deref() == Some(birth)
                    }
                    None => true,
                }
            {
                managed_pids.insert(pid);
            }
            let job_sid = job["session_id"].as_str().unwrap_or("");
            let owned = !job_sid.is_empty() && job_sid == my_sid;
            // Session-less runs (manual `gray subagents run`) keep the
            // cwd match; once a run knows its session it hides elsewhere.
            let legacy =
                job_sid.is_empty() && job["cwd"].as_str().map(PathBuf::from).as_ref() == Some(&cwd);
            let in_tree = active
                && job["child_pid"]
                    .as_u64()
                    .is_some_and(|pid| visible_in_session(pid));
            if !all && !owned && !legacy && !in_tree {
                continue;
            }
            // The widget fades finished runs quickly; the menu keeps them
            // selectable (that's where `open`/`chat` act on them).
            if !include_stale && !live && now - job["finished"].as_f64().unwrap_or(0.0) > 30.0 {
                continue;
            }
            let state = match status {
                "running" | "stopping" => AgentState::Running {
                    activity: if status == "stopping" {
                        "stopping…".into()
                    } else {
                        // The child's own transcript says `$ ls src`; the
                        // registry only knows the bare tool name. Rows
                        // show the command — the `$ ` prompt marker stays
                        // in the record, not the UI.
                        let a = job["child_session"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .and_then(session_activity)
                            .unwrap_or_else(|| job["activity"].as_str().unwrap_or("").into());
                        let a = a.strip_prefix("$ ").unwrap_or(&a);
                        if a.is_empty() { "working…".into() } else { a.into() }
                    },
                },
                "queued" => AgentState::Queued,
                "completed" => AgentState::Completed,
                "stopped" => AgentState::Stopped,
                _ => AgentState::Failed {
                    error: job["error"].as_str().unwrap_or(status).into(),
                },
            };
            entries.push(Entry {
                row: AgentRow {
                    name: job["name"]
                        .as_str()
                        .or_else(|| job["agent"].as_str())
                        .unwrap_or("agent")
                        .into(),
                    description: {
                        let t: String = job["task"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(160)
                            .collect();
                        if job["task"].as_str().unwrap_or("").chars().count() > 160 {
                            format!("{t}…")
                        } else {
                            t
                        }
                    },
                    stats: fmt_elapsed(
                        (job["finished"].as_f64().unwrap_or(now)
                            - job["created"].as_f64().unwrap_or(now))
                        .max(0.0),
                    ),
                    model: {
                        let mut m = pretty_model(job["model"].as_str().unwrap_or(""));
                        for part in [
                            job["effort"].as_str().unwrap_or("").to_string(),
                            job["context"]
                                .as_str()
                                .filter(|c| !c.is_empty())
                                .map(|c| format!("ctx {c}"))
                                .unwrap_or_default(),
                        ] {
                            if !part.is_empty() {
                                m.push_str(&format!(" · {part}"));
                            }
                        }
                        m
                    },
                    result: job["result"].as_str().unwrap_or("").into(),
                    state,
                },
                target: job["run_id"].as_str().unwrap_or("").into(),
                session: job["child_session"].as_str().unwrap_or("").into(),
                status: status.into(),
                managed: true,
                created: job["created"].as_f64().unwrap_or(0.0),
            });
        }
    }
    for w in foreign_scan(&managed_pids) {
        entries.push(Entry {
            target: w.session.clone().unwrap_or_else(|| w.name.clone()),
            session: w.session.clone().unwrap_or_default(),
            status: "running".into(),
            managed: false,
            created: now_epoch() - w.elapsed,
            row: AgentRow {
                name: w.name,
                description: if w.task.is_empty() {
                    "gray -p".into()
                } else {
                    w.task.chars().take(160).collect()
                },
                model: pretty_model(&w.model),
                result: String::new(),
                stats: format!(
                    "{}{}",
                    fmt_elapsed(w.elapsed),
                    if w.cwd_base.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", w.cwd_base)
                    }
                ),
                state: AgentState::Running {
                    activity: w.activity,
                },
            },
        });
    }
    Ok(entries)
}

/// Display order for the menu and the Agents panel: live work first —
/// running/queued rows you check on — then finished ones; each group
/// newest-first. Menu numbers follow this order.
pub(crate) fn menu_order(mut entries: Vec<Entry>) -> Vec<Entry> {
    let live = |e: &Entry| matches!(e.row.state, AgentState::Running { .. } | AgentState::Queued);
    entries.sort_by(|a, b| live(b).cmp(&live(a)).then(b.created.total_cmp(&a.created)));
    entries
}

/// `N` (a menu number), exact name, session slug, run-id prefix, or `last`
/// → the entry. Names win over numbers so a run literally called `2`
/// stays addressable.
pub(crate) fn resolve_entry<'a>(entries: &'a [Entry], target: &str) -> Option<&'a Entry> {
    entries
        .iter()
        .find(|e| e.row.name == target)
        .or_else(|| {
            target
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| entries.get(i))
        })
        .or_else(|| {
            entries
                .iter()
                .find(|e| !e.session.is_empty() && e.session == target)
        })
        .or_else(|| {
            entries
                .iter()
                .find(|e| target.len() >= 4 && !e.target.is_empty() && e.target.starts_with(target))
        })
        .or_else(|| {
            (target == "last")
                .then_some(
                    entries
                        .iter()
                        .max_by(|a, b| a.created.total_cmp(&b.created)),
                )
                .flatten()
        })
}

/// A menu number resolves against this session's rows; anything else
/// passes through for the backend's own name/prefix/`last` resolution.
/// (`all`/`mine`/names parse as non-numeric and pass untouched.)
pub(crate) fn menu_target(target: &str) -> String {
    if target.parse::<usize>().is_err() {
        return target.to_string();
    }
    let Ok(entries) = collect_entries(true, false) else {
        return target.to_string();
    };
    let entries = menu_order(entries);
    resolve_entry(&entries, target)
        .map(|e| e.target.clone())
        .unwrap_or_else(|| target.to_string())
}

/// The interactive `/subagents` menu: same flat voice as the widget, each
/// row numbered so one word selects it. Session-scoped like the widget.
fn menu() -> Result<Value> {
    let entries = menu_order(collect_entries(true, false)?);
    if entries.is_empty() {
        return Ok(json!({"content": "  no runs yet — gray subagents run 'task' to start one"}));
    }
    let running = entries
        .iter()
        .filter(|e| matches!(e.row.state, AgentState::Running { .. }))
        .count();
    let rows: Vec<AgentRow> = entries.iter().map(|e| e.row.clone()).collect();
    let mut text = render_menu(&rows);
    text.push_str(&format!(
        "\n  N|NAME selects · open — enter a finished run's session · chat '…' — talk · view · steer{}",
        if running > 0 { " · stop N|all|mine" } else { "" }
    ));
    Ok(json!({"content": text}))
}

/// The session file a `open`/`/resume` needs: present once the child has
/// started its first turn, gone only if sessions were pruned.
fn session_file(session: &str) -> Option<PathBuf> {
    if session.is_empty() {
        return None;
    }
    let path = home()
        .ok()?
        .join("sessions")
        .join(format!("{session}.jsonl"));
    path.is_file().then_some(path)
}

/// The `open` card: what the run is doing and the one next action. Running
/// runs steer (their session is lock-held — `/resume` refuses); finished
/// runs carry a real resumable session — the opencode/CC child-session
/// model — so the card hands over `/resume <sid>` verbatim.
fn open_card(name: &str, status: &str, session: &str, managed: bool) -> Value {
    let head = format!("⬢ {name} — {status}");
    let mut hints: Vec<String> = Vec::new();
    if matches!(status, "running" | "stopping" | "queued") {
        if managed {
            hints.push(format!(
                "chat {name} 'msg' — queue a follow-up · view {name} — transcript · stop {name}"
            ));
        } else {
            hints.push(format!("view {name} — transcript · stop {name}"));
        }
        if !session.is_empty() {
            hints.push(format!("/resume {session} — refuses while the run is live"));
        }
    } else if session_file(session).is_some() {
        hints.push(format!(
            "/resume {session} — opens its session; chat continues the run"
        ));
    } else {
        hints.push(format!(
            "view {name} — transcript tail{}",
            if !session.is_empty() {
                " · session file is gone"
            } else {
                ""
            }
        ));
    }
    let body = hints
        .iter()
        .map(|h| format!("  {h}"))
        .collect::<Vec<_>>()
        .join("\n");
    json!({"content": format!("{head}\n{body}")})
}

/// `open NAME|N|SLUG`: first this session's menu rows; then a bare session
/// slug; then any managed run in the registry (another session's runs are
/// still enterable — they are sessions too).
fn open(target: &str) -> Result<Value> {
    let entries = menu_order(collect_entries(true, false)?);
    if let Some(entry) = resolve_entry(&entries, target) {
        return Ok(open_card(
            &entry.row.name,
            &entry.status,
            &entry.session,
            entry.managed,
        ));
    }
    if session_file(target).is_some() {
        return Ok(
            json!({"content": format!("⬢ {target}\n  /resume {target} — opens that session")}),
        );
    }
    let detail = backend("status", json!({"run_id": target}), None, None)?;
    let name = detail["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| target.to_string());
    Ok(open_card(
        &name,
        detail["status"].as_str().unwrap_or("lost"),
        detail["child_session"].as_str().unwrap_or(""),
        true,
    ))
}

/// `chat NAME|N 'msg'` — a follow-up into the run's own child session.
/// On a live run it queues (lands between turns); on a finished run the
/// supervisor revives the child session — exactly how opencode resumes a
/// subagent session and how Claude Code messages a background task.
/// Raw `gray -p` workers have no supervisor: there is nothing to queue
/// into, so chat says so instead of steering into the void.
pub(crate) fn chat(target: &str, message: Vec<String>) -> Result<Value> {
    let entries = menu_order(collect_entries(true, false)?);
    let entry = resolve_entry(&entries, target).map(|e| {
        (
            e.target.clone(),
            e.row.name.clone(),
            e.status.clone(),
            e.session.clone(),
            e.managed,
        )
    });
    if let Some((rid, name, status, session, managed)) = entry {
        if message.is_empty() {
            return Ok(open_card(&name, &status, &session, managed));
        }
        if !managed {
            anyhow::bail!(
                "{name} is a raw gray -p worker — no run record to chat through;                  {} once it exits",
                if session.is_empty() {
                    "its session slug was not captured".to_string()
                } else {
                    format!("/resume {session}")
                }
            );
        }
        return backend(
            "steer",
            json!({"run_id": rid, "message": message.join(" ")}),
            None,
            None,
        );
    }
    if message.is_empty() {
        anyhow::bail!("chat needs a target and a message — chat NAME 'follow-up'");
    }
    backend(
        "steer",
        json!({"run_id": target, "message": message.join(" ")}),
        None,
        None,
    )
}

/// /proc/<pid>/stat fields after the comm: index 1 is ppid, index 19 is the
/// starttime clock tick (the birth check supervisors use).
fn proc_stat(pid: u64) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(") ").map(|(_, t)| t)?;
    let fields: Vec<String> = tail.split_whitespace().map(str::to_owned).collect();
    (fields.first().map(String::as_str) != Some("Z")).then_some(fields)
}

fn ppid_of(pid: u64) -> Option<u64> {
    proc_stat(pid)?.get(1)?.parse().ok()
}

fn start_ticks(pid: u64) -> Option<u64> {
    proc_stat(pid)?.get(19)?.parse().ok()
}

fn proc_argv(pid: u64) -> Option<Vec<String>> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = cmdline
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

fn exe_basename(argv: &[String]) -> &str {
    argv.first()
        .and_then(|e| std::path::Path::new(e).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("")
}

/// The task text of a `gray -p` invocation, or None when argv carries no
/// prompt flag (interactive TUI, subcommands, plugin calls).
fn gray_p_prompt(argv: &[String]) -> Option<String> {
    if exe_basename(argv) != "gray" {
        return None;
    }
    for (i, arg) in argv.iter().enumerate().skip(1) {
        if arg == "-p" || arg == "--prompt" {
            return Some(argv.get(i + 1).cloned().unwrap_or_default());
        }
        if let Some(v) = arg
            .strip_prefix("--prompt=")
            .or_else(|| arg.strip_prefix("-p="))
        {
            return Some(v.into());
        }
        if arg.len() > 2
            && let Some(v) = arg.strip_prefix("-p")
        {
            return Some(v.into());
        }
    }
    None
}

/// The session's interactive gray process: nearest ancestor that is `gray`
/// without a prompt flag. The widget/command sidecars are spawned by it, so
/// any process sharing it as an ancestor belongs to this session.
pub(crate) fn session_root() -> Option<u64> {
    let mut pid = std::process::id() as u64;
    let mut hops = 0;
    while let Some(ppid) = ppid_of(pid) {
        if ppid <= 1 || hops > 64 {
            return None;
        }
        if proc_argv(ppid)
            .is_some_and(|argv| exe_basename(&argv) == "gray" && gray_p_prompt(&argv).is_none())
        {
            return Some(ppid);
        }
        pid = ppid;
        hops += 1;
    }
    None
}

/// Worker processes belong to a session when the session's interactive gray
/// is one of their ancestors — run-all.sh subshells and mcp sidecars both
/// keep that lineage. No root (manual CLI invocation) means show everything.
fn visible_in_session(pid: u64) -> bool {
    let Some(root) = session_root() else {
        return true;
    };
    let mut cur = pid;
    let mut hops = 0;
    while let Some(ppid) = ppid_of(cur) {
        if ppid == root {
            return true;
        }
        if ppid <= 1 || hops > 64 {
            return false;
        }
        cur = ppid;
        hops += 1;
    }
    false
}

/// A `gray -p` worker adopted from /proc: everything the row needs plus the
/// identity fields `stop`/`view` resolve names against.
struct ForeignWorker {
    pid: u64,
    name: String,
    task: String,
    elapsed: f64,
    cwd_base: String,
    session: Option<String>,
    model: String,
    activity: String,
}

/// The session id (slug) a process holds open: `~/.gray/sessions/<id>.open`
/// is flock-held for the session's life, so it shows up under /proc/<pid>/fd.
/// Owning session of a process tree: the exported GRAY_SESSION_ID marker
/// first (the root session the tree belongs to), then the process's own
/// held session via `own_session_of_pid` for pre-env spawns.
pub(crate) fn session_of_pid(pid: u64) -> Option<String> {
    env_value(pid, "GRAY_SESSION_ID").or_else(|| own_session_of_pid(pid))
}

/// The session id a process holds open itself: `~/.gray/sessions/<id>.open`
/// is flock-held for the session's life, so it shows up under /proc/<pid>/fd.
fn own_session_of_pid(pid: u64) -> Option<String> {
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let in_sessions = target
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == "sessions"));
        if !in_sessions {
            continue;
        }
        let Some(name) = target.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let name = name.strip_suffix(" (deleted)").unwrap_or(name);
        if let Some(id) = name
            .strip_suffix(".open")
            .or_else(|| name.strip_suffix(".jsonl"))
            && !id.is_empty()
        {
            return Some(id.to_string());
        }
    }
    None
}

/// One `K=V` from a live process's environment (`/proc/<pid>/environ`).
fn env_value(pid: u64, key: &str) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{key}=");
    raw.split(|b| *b == 0)
        .filter_map(|kv| std::str::from_utf8(kv).ok())
        .find_map(|kv| kv.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `45s`, `2m18s`, `1h3m` — compact elapsed for the stats segment.
fn fmt_elapsed(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// One-line summary of a session-transcript tool call, mirroring the
/// managed-run `describe_row` voice (`read src/foo.rs`, `$ cargo test`).
fn tool_summary(name: &str, args: &Value) -> String {
    let str_arg = |key: &str| args[key].as_str().unwrap_or("");
    let first_line = |v: &str| v.lines().next().unwrap_or("").trim().to_string();
    let home_dir = std::env::var_os("HOME")
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    let short_path = |v: &str| {
        if !home_dir.is_empty() && v.starts_with(&home_dir) {
            format!("~{}", &v[home_dir.len()..])
        } else {
            v.to_string()
        }
    };
    let text = match name {
        "bash" | "exec" | "shell" => format!("$ {}", first_line(str_arg("command"))),
        "read" => format!("read {}", short_path(str_arg("file_path"))),
        "edit" => format!("edit {}", short_path(str_arg("file_path"))),
        "write" => format!("write {}", short_path(str_arg("file_path"))),
        "grep" => format!("grep {}", first_line(str_arg("pattern"))),
        "glob" | "find_file_by_name" | "find" => {
            format!("find {}", first_line(str_arg("pattern")))
        }
        "web_search" => format!("web search: {}", first_line(str_arg("query"))),
        other => {
            let compact = serde_json::to_string(args).unwrap_or_default();
            let compact = compact.trim_matches(|c| c == '{' || c == '}');
            format!("{other} {}", compact.chars().take(60).collect::<String>())
        }
    };
    text.chars().take(100).collect()
}

/// The last thing a session transcript records: most recent tool call, or
/// "replying" when the child is drafting text. Reads at most the tail 64 KiB.
fn session_activity(sid: &str) -> Option<String> {
    let path = home().ok()?.join("sessions").join(format!("{sid}.jsonl"));
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)))
        .ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let buf = String::from_utf8_lossy(&buf);
    let mut activity = None;
    for line in buf.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = entry["message"]["role"].as_str().unwrap_or("");
        let Some(content) = entry["message"]["content"].as_array() else {
            continue;
        };
        // Text and tool_use share one assistant message — the text is the
        // preamble to the call, so a live tool always wins "replying".
        let mut tool = None;
        for part in content {
            if part["type"].as_str() == Some("tool_use") {
                tool = Some(tool_summary(
                    part["name"].as_str().unwrap_or("tool"),
                    &part["args"],
                ));
            }
        }
        if tool.is_some() {
            activity = tool;
            continue;
        }
        if role == "assistant"
            && content.iter().any(|p| {
                p["type"].as_str() == Some("text")
                    && !p["text"].as_str().unwrap_or("").trim().is_empty()
            })
        {
            activity = Some("replying".into());
        }
    }
    activity
}

/// The `"model"` field of a session header (first jsonl line), short form.
fn session_model(sid: &str) -> Option<String> {
    let path = home().ok()?.join("sessions").join(format!("{sid}.jsonl"));
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 2048];
    let n = file.read(&mut buf).ok()?;
    let first = std::str::from_utf8(&buf[..n]).ok()?.lines().next()?;
    let header: Value = serde_json::from_str(first).ok()?;
    header["model"]
        .as_str()
        .map(|m| m.rsplit('/').next().unwrap_or(m).to_string())
        .filter(|m| !m.is_empty())
}

/// `gray -p` workers no run record owns: mcp `gray_prompt` children and plain
/// scripted fan-outs never register, so the widget adopts them by scanning
/// /proc. Managed children are excluded by pid; workers from other sessions
/// are excluded by lineage. A worker's name is its GRAY_AGENT_NAME env (set
/// by the spawner — `gray_prompt --name`, scripts), else its session slug
/// (`<id>.open` held by the process), else `worker-<pid>`.
fn foreign_scan(managed: &std::collections::HashSet<u64>) -> Vec<ForeignWorker> {
    let mut workers = Vec::new();
    let Some(uptime) = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
    else {
        return workers;
    };
    let self_pid = std::process::id() as u64;
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return workers;
    };
    for entry in proc_dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        if pid == self_pid || managed.contains(&pid) {
            continue;
        }
        let Some(argv) = proc_argv(pid) else { continue };
        let Some(prompt) = gray_p_prompt(&argv) else {
            continue;
        };
        if !visible_in_session(pid) {
            continue;
        }
        let task: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        let elapsed = start_ticks(pid)
            .map(|t| (uptime - t as f64 / 100.0).max(0.0))
            .unwrap_or(0.0);
        let cwd_base = std::fs::read_link(entry.path().join("cwd"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into());
        let session = own_session_of_pid(pid).or_else(|| env_value(pid, "GRAY_SESSION_ID"));
        let name = env_value(pid, "GRAY_AGENT_NAME")
            .or_else(|| session.clone())
            .unwrap_or_else(|| format!("worker-{pid}"));
        let model = session
            .as_deref()
            .and_then(session_model)
            .unwrap_or_default();
        let activity = session
            .as_deref()
            .and_then(session_activity)
            .unwrap_or_else(|| "starting…".into());
        workers.push(ForeignWorker {
            pid,
            name,
            task,
            elapsed,
            cwd_base,
            session,
            model,
            activity,
        });
    }
    // Self-registered `gray -p` workers: the host writes
    // ~/.gray/running/<pid>.json on startup (removed on exit), so a worker
    // that never went through a spawn tool is discoverable without argv
    // archaeology. The record's own metadata beats scraping; rows for
    // dead pids are stale (kill -9) and drop on the liveness check.
    if let Ok(dir) = home()
        .map(|h| h.join("running"))
        .and_then(|d| std::fs::read_dir(d).map_err(|e| e.into()))
    {
        for entry in dir.flatten() {
            let Ok(raw) = std::fs::read(entry.path()) else {
                continue;
            };
            let Ok(row) = serde_json::from_slice::<Value>(&raw) else {
                continue;
            };
            let Some(pid) = row["pid"].as_u64() else {
                continue;
            };
            if pid == self_pid
                || managed.contains(&pid)
                || workers.iter().any(|w| w.pid == pid)
                || !std::path::Path::new(&format!("/proc/{pid}")).exists()
                || !visible_in_session(pid)
            {
                continue;
            }
            let session = row["session"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| row["owner"].as_str().filter(|s| !s.is_empty()))
                .map(str::to_string);
            let elapsed = row["started"]
                .as_f64()
                .map(|t| {
                    (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0)
                        - t)
                        .max(0.0)
                })
                .unwrap_or(0.0);
            let cwd_base = row["cwd"]
                .as_str()
                .and_then(|c| c.rsplit('/').next())
                .filter(|s| !s.is_empty())
                .unwrap_or("?")
                .to_string();
            workers.push(ForeignWorker {
                pid,
                name: row["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("worker-{pid}")),
                task: row["task"].as_str().unwrap_or_default().to_string(),
                elapsed,
                cwd_base,
                model: row["model"].as_str().unwrap_or_default().to_string(),
                activity: session
                    .as_deref()
                    .and_then(session_activity)
                    .unwrap_or_else(|| "starting…".into()),
                session,
            });
        }
    }
    workers.sort_by(|a, b| a.elapsed.total_cmp(&b.elapsed));
    workers
}

/// `stop <pid|name>` for an adopted worker: TERM, then KILL if it lingers.
/// Only fires on workers in this session's tree — anything else falls
/// through to the registry.
/// Live descendants of root_pid via /proc ppid links. Scanned BEFORE the
/// root is signalled — ppid links die with it, and orphaned plugin
/// sidecars (a worker carries ~a dozen) would otherwise linger.
fn descendants_of(root_pid: u64) -> Vec<u64> {
    let mut children: std::collections::HashMap<u64, Vec<u64>> = std::collections::HashMap::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u64>() else {
                continue;
            };
            if let Some(ppid) = ppid_of(pid) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut out = Vec::new();
    let mut stack: Vec<u64> = children.get(&root_pid).cloned().unwrap_or_default();
    while let Some(pid) = stack.pop() {
        out.push(pid);
        stack.extend(children.get(&pid).cloned().unwrap_or_default());
    }
    out
}

/// Signal a pid's process group first, the lone pid when it leads no
/// group — the supervisor's kill_tree semantics for unmanaged workers.
fn signal_proc_or_group(pid: u64, sig: &str) {
    let grouped = Command::new("kill")
        .args([sig, &format!("-{pid}")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !grouped {
        let _ = Command::new("kill").args([sig, &pid.to_string()]).status();
    }
}

pub(crate) fn foreign_stop(rid: &str) -> Option<Value> {
    let worker = foreign_scan(&std::collections::HashSet::new())
        .into_iter()
        .find(|w| w.name == rid || w.pid.to_string() == rid || w.session.as_deref() == Some(rid))?;
    let mut tree = descendants_of(worker.pid);
    tree.insert(0, worker.pid);
    for pid in &tree {
        signal_proc_or_group(*pid, "-TERM");
    }
    for _ in 0..20 {
        if proc_stat(worker.pid).is_none() {
            return Some(json!({"content": format!("killed worker {}", worker.name)}));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    for pid in &tree {
        signal_proc_or_group(*pid, "-KILL");
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    Some(json!({"content": if proc_stat(worker.pid).is_none() {
        format!("killed worker {} (SIGKILL)", worker.name)
    } else {
        format!("worker {} did not exit after SIGKILL", worker.name)
    }}))
}

const EFFORTS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// A context ceiling as GRAY_COMPACT_AT takes it: "auto", or a token count
/// like "90k" / "300k" / "1.5m" / "90000" — normalized to lowercase. The
/// supervisor twin is `clean_context` in mod; both must agree.
pub(crate) fn check_context(value: &str) -> Result<String> {
    let v = value.trim().to_lowercase();
    if v == "auto" {
        return Ok(v);
    }
    let digits = v.strip_suffix(['k', 'm']).unwrap_or(&v);
    let mut parts = digits.split('.');
    let number = matches!(parts.next(), Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && parts
            .next()
            .map_or(true, |p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && parts.next().is_none();
    let positive = digits.parse::<f64>().is_ok_and(|n| n > 0.0);
    anyhow::ensure!(
        number && positive,
        "context must be 'auto' or a token count like 90k, 300k, 1m"
    );
    Ok(v)
}

pub(crate) fn check_effort(effort: &str) -> Result<String> {
    let value = effort.trim().to_lowercase();
    anyhow::ensure!(
        EFFORTS.contains(&value.as_str()),
        "effort must be one of: {}",
        EFFORTS.join(", ")
    );
    Ok(value)
}

/// Resolve a `--model` value against the caches under $GRAY_HOME; the rules
/// live in `model_catalog::resolve_model` (shared with the `mod` supervisor).
pub(crate) fn resolve_model(spec: &str) -> Result<String> {
    model_catalog::resolve_model(spec, &home()?)
}

const USAGE: &str = "Subagents are managed through Bash: gray subagents run [--name NAME] [--agent scout] [--model PROVIDER/MODEL] [--effort LEVEL] [--context CTX] [--tools CAPS] [--approve CAPS] [--isolation worktree] 'task'; gray subagents status [ID]; gray subagents view [ID] [--lines N]; gray subagents steer ID 'follow-up'; gray subagents stop ID; gray subagents settings --model PROVIDER/MODEL [--effort LEVEL] [--context CTX]. Always pass a descriptive --name — it labels the run in the Agents view. Per-run --model/--effort/--context override profile frontmatter and the global setting (`context` becomes the child's GRAY_COMPACT_AT; above 200k a Claude model uses its [1m] route — Haiku bills 5x past 100k); profiles in ~/.gray/subagents/agents/*.md may open with a `---` block carrying `model:`/`effort:`/`tools:`/`context:` defaults (`tools:` narrows the child's grant via gray-narrow — a child can never exceed its parent's set). Every run gets a readable name (or use --name); ID accepts a name, unique hex prefix, a menu number from `gray subagents status`, or 'last'. open NAME prints the /resume line that enters a finished run's own session; chat NAME 'msg' talks into it (steer alias). Raw `gray -p` fan-outs set GRAY_AGENT_NAME to carry a display name (and are shown with their session slug otherwise); view/stop also accept worker names and session slugs. --isolation worktree gives the child its own git worktree — required when parallel runs edit the same repo. Runs have no time cap unless `timeout` (seconds) is passed to the subagent tool or GRAY_SUBAGENTS_TIMEOUT_SECS is set; a run that times out reports partial output and stays steerable. Children cannot spawn subagents. Runs are detached — they keep going if this session ends. The rlm_spawn/rlm_collect/rlm_send/rlm_delete/rlm_progress_note tools expose the same supervisor programmatically (handles + typed snapshots; ≤32 spawns per session). Do not invent other subagent tools.";

/// JSON rows for the host's agents panel: the same entries the text menu
/// numbers, plus the session/target fields its actions need.
fn entries_json(all: bool) -> Result<Value> {
    let entries = menu_order(collect_entries(true, all)?);
    let rows: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let running = matches!(e.row.state, AgentState::Running { .. });
            let activity = match &e.row.state {
                AgentState::Running { activity } => activity.clone(),
                _ => String::new(),
            };
            let error = match &e.row.state {
                AgentState::Failed { error } => error.clone(),
                _ => String::new(),
            };
            json!({
                "i": i + 1,
                "name": e.row.name,
                "task": e.row.description,
                "icon": state_icon(&e.row.state),
                "status": e.status,
                "stats": if e.row.model.is_empty() {
                    e.row.stats.clone()
                } else {
                    format!("{} · {}", e.row.stats, e.row.model)
                },
                "activity": activity,
                "session": e.session,
                "target": e.target,
                "managed": e.managed,
                "running": running,
                "resumable": session_file(&e.session).is_some(),
                "error": error,
            })
        })
        .collect();
    Ok(json!({"entries": rows}))
}

fn execute(action: Action, session: Option<&Value>) -> Result<Value> {
    match action {
        Action::Settings {
            model,
            effort,
            context,
            max_running,
        } => settings(model, max_running, effort, context),
        Action::Setup => backend("setup", json!({}), None, None),
        Action::Run {
            task,
            agent,
            model,
            effort,
            context,
            name,
            tools,
            approve,
            isolation,
        } => {
            let mut args = json!({"task":task.join(" "),"agent":agent});
            if let Some(tools) = tools {
                args["tools"] = json!(tools);
            }
            if let Some(approve) = approve {
                args["approve"] = json!(approve);
            }
            if let Some(isolation) = isolation {
                anyhow::ensure!(isolation == "worktree", "isolation must be 'worktree'");
                args["isolation"] = json!(isolation);
            }
            if let Some(model) = model {
                args["model"] = json!(resolve_model(&model)?);
            }
            if let Some(effort) = effort {
                args["effort"] = json!(check_effort(&effort)?);
            }
            if let Some(context) = context {
                args["context"] = json!(check_context(&context)?);
            }
            if let Some(name) = name {
                args["name"] = json!(name);
            }
            if let Some(session) = session {
                args["session"] = session.clone();
            } else if let Some(sid) = session_root().and_then(session_of_pid) {
                args["session"] = json!({"id": sid});
            }
            backend("run", args, None, None)
        }
        Action::Steer { run_id, message } => backend(
            "steer",
            json!({"run_id": menu_target(&run_id),"message":message.join(" ")}),
            None,
            None,
        ),
        Action::Entries { all } => entries_json(all),
        Action::Menu => menu(),
        Action::Open { target } => open(&target),
        Action::Chat { target, message } => chat(&target, message),
        Action::Status {
            run_id: None,
            all: false,
        } => {
            let mut v = menu()?;
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let notices = backend("notices", json!({"cwd": cwd}), None, None)
                .ok()
                .and_then(|n| n["text"].as_str().map(str::to_owned))
                .unwrap_or_default();
            if !notices.is_empty() {
                let c = v["content"].as_str().unwrap_or_default().to_string();
                v["content"] = json!(format!("{notices}\n\n{c}"));
            }
            Ok(v)
        }
        Action::Status { run_id, .. } => backend(
            "status",
            json!({"run_id": run_id.map(|t| menu_target(&t))}),
            None,
            None,
        ),
        Action::View { target, lines } => backend(
            "view",
            json!({"run_id": menu_target(target.as_deref().unwrap_or("last")),"lines":lines}),
            None,
            None,
        ),
        Action::Stop { run_id } => {
            let rid = run_id
                .map(|t| menu_target(&t))
                .unwrap_or_else(|| "mine".to_string());
            match (rid != "all" && rid != "mine")
                .then(|| foreign_stop(&rid))
                .flatten()
            {
                Some(v) => Ok(v),
                None => {
                    let sid = session_root().and_then(session_of_pid).unwrap_or_default();
                    backend(
                        "stop",
                        json!({"run_id": rid, "session": {"id": sid}}),
                        None,
                        None,
                    )
                }
            }
        }
        Action::List => backend("list", json!({}), None, None),
        Action::Widget { demo, offset } => widget(demo, offset),
        Action::Manifest => Ok(manifest()),
    }
}
fn present(v: &Value) -> String {
    if let Some(text) = v["content"].as_str() {
        return text.to_string();
    }
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn sidecar() -> Result<()> {
    for line in io::stdin().lock().lines() {
        let Ok(req) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if req["method"] == "plugin/shutdown" {
            break;
        }
        if req.get("id").is_none() {
            continue;
        }
        // Folded-in gray-rlm surface: rlm_* tools and /rlm command.
        if req["method"].as_str() == Some("tool/call")
            && rlm::is_rlm_tool(req["params"]["name"].as_str().unwrap_or(""))
        {
            let (result, frames) = rlm::handle_tool(&req["params"]);
            for f in frames {
                println!("{f}");
            }
            println!("{}", json!({"id": req["id"], "result": result}));
            io::stdout().flush()?;
            continue;
        }
        if req["method"].as_str() == Some("command/run")
            && matches!(req["params"]["name"].as_str(), Some("/rlm") | Some("rlm"))
        {
            let text = rlm::command(&req["params"]);
            println!("{}", json!({"id": req["id"], "result": {"text": text}}));
            io::stdout().flush()?;
            continue;
        }
        // Folded-in gray-narrow surface: its hooks/tools/commands go straight
        // to the narrow module's own dispatcher (it emits a full reply).
        if matches!(
            req["method"].as_str(),
            Some("tool/before") | Some("tool/call")
        ) || (req["method"].as_str() == Some("command/run")
            && matches!(
                req["params"]["name"].as_str(),
                Some("/narrow") | Some("narrow")
            ))
        {
            let (reply, _) = narrow::handle_narrow(&req);
            if let Some(reply) = reply {
                println!("{reply}");
                io::stdout().flush()?;
            }
            continue;
        }
        let result = match req["method"].as_str() {
            Some("plugin/manifest") => Ok(manifest()),
            Some("prompt/context") => {
                if std::env::var_os("GRAY_SUBAGENTS_ACTIVE").is_some() {
                    Ok(json!({"text":""}))
                } else {
                    let cwd = req["params"]["cwd"].as_str().unwrap_or("");
                    let session = req["params"].get("session").cloned().unwrap_or(json!({}));
                    let notices =
                        backend("context", json!({"cwd":cwd,"session":session}), None, None)
                            .ok()
                            .and_then(|v| v["text"].as_str().map(str::to_owned))
                            .unwrap_or_default();
                    let mut text = format!("{USAGE}\n{notices}");
                    let nreq = json!({"id":0,"method":"prompt/context","params":req["params"]});
                    if let (Some(r), _) = narrow::handle_narrow(&nreq) {
                        if let Some(t) = r.pointer("/result/text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                text.push_str("\n");
                                text.push_str(t);
                            }
                        }
                    }
                    Ok(json!({"text":text}))
                }
            }
            Some("command/run") => {
                let args = req["params"]["argv"]
                    .as_array()
                    .context("command argv must be an array")?;
                let mut argv = vec!["gray-subagents".to_string()];
                argv.extend(args.iter().filter_map(|v| v.as_str().map(str::to_owned)));
                if argv.len() == 1 {
                    // Bare `/subagents` opens the host's interactive panel:
                    // rows/actions ride this CLI via `entries`/`open`/
                    // `view`/`stop`/`chat`.
                    Ok(json!({"agent_picker":"subagents"}))
                } else {
                    let session = req["params"].get("session").cloned();
                    match Cli::try_parse_from(preprocess(argv)) {
                        // Usage/help is an answer, not a crash: the host
                        // shows `error` replies as a failed command.
                        Err(e) => Ok(json!({"text": e.render().to_string()})),
                        Ok(cli) => match cli.command {
                            Some(action) => execute(action, session.as_ref())
                                .map(|v| json!({"text":present(&v)})),
                            None => Ok(json!({"agent_picker":"subagents"})),
                        },
                    }
                }
            }
            _ => Err(anyhow::anyhow!("unsupported sidecar method")),
        };
        let reply = match result {
            Ok(result) => json!({"id":req["id"],"result":result}),
            Err(e) => json!({"id":req["id"],"error":e.to_string()}),
        };
        println!("{reply}");
        io::stdout().flush()?;
    }
    Ok(())
}
/// `/subagents NAME` and `/subagents 3` select like `open NAME`/`open 3` —
/// the menu's whole point is one-word selection. Applies to the CLI and
/// the sidecar argv alike.
fn preprocess(mut argv: Vec<String>) -> Vec<String> {
    const VERBS: &[&str] = &[
        "settings", "setup", "run", "steer", "status", "view", "stop", "list", "widget",
        "manifest", "menu", "open", "chat", "entries", "help",
    ];
    if argv.len() == 2 && !argv[1].starts_with('-') && !VERBS.contains(&argv[1].as_str()) {
        argv.insert(1, "open".to_string());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::check_context;

    #[test]
    fn context_parses_like_the_python_twin() {
        for ok in ["auto", "AUTO", "90k", "90K", "300k", "1m", "1.5m", "90000", "0.5m"] {
            assert!(check_context(ok).is_ok(), "{ok:?}");
        }
        assert_eq!(check_context(" 90K ").unwrap(), "90k");
        for bad in ["", " ", "ctx", "90kk", "1.5x", "0", "-5", "1e5", ".5k", "1.2.3m"] {
            assert!(check_context(bad).is_err(), "{bad:?}");
        }
    }
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    // Folded-in gray-narrow argv surface:
    //   `bin narrow …`  — direct narrow CLI (`gray narrow run`, lock cli_argv)
    //   `bin plan …`    — the supervisor's GRAY_NARROW_BIN self-call
    //   `bin sidecar`   — narrow-style spawn argv (sidecar_args)
    if argv.len() > 1 && argv[1] == "sidecar" {
        return sidecar();
    }
    if argv.len() > 1 && argv[1] == "rlm" {
        println!("{}", rlm::cli(&argv[2..].to_vec()));
        return Ok(());
    }
    if argv.len() > 1 && (argv[1] == "narrow" || argv[1] == "plan") {
        let args = if argv[1] == "narrow" {
            argv[2..].to_vec()
        } else {
            argv[1..].to_vec()
        };
        return match narrow::narrow_cli(args) {
            Ok(s) => {
                println!("{s}");
                Ok(())
            }
            Err(e) if e == "__sidecar" => sidecar(),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
    }
    let cli = match Cli::try_parse_from(preprocess(argv)) {
        Ok(cli) => cli,
        // `gray subagents view` from the composer runs this argv path: a
        // usage slip must print usage, not exit 2 and read as a crash.
        Err(e) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"text": e.render().to_string()}))?
            );
            return Ok(());
        }
    };
    if let Some(action) = cli.command {
        println!("{}", serde_json::to_string_pretty(&execute(action, None)?)?);
        Ok(())
    } else {
        sidecar()
    }
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    /// gray headlines each tool with its declared label and only humanizes the
    /// wire name when none is set, so every folded-in tool must declare one.
    #[test]
    fn every_folded_tool_has_a_clean_label() {
        let m = manifest();
        let tools = m["tools"].as_array().expect("manifest tools");
        assert!(!tools.is_empty());
        for t in tools {
            let name = t["name"].as_str().unwrap_or("?");
            let label = t["label"].as_str().unwrap_or("").trim();
            assert!(!label.is_empty(), "tool {name} declares no label");
            assert!(!label.contains('_'), "tool {name} label {label:?} is a wire name");
        }
    }

    /// A {field} or {a.b|filter} placeholder renders empty when the field is not
    /// a call arg, so every head path must be a declared parameter of the tool.
    #[test]
    fn every_folded_tool_preview_references_declared_params() {
        let m = manifest();
        for t in m["tools"].as_array().expect("manifest tools") {
            let name = t["name"].as_str().unwrap_or("?");
            let Some(preview) = t["preview"].as_str() else {
                continue;
            };
            let props = &t["parameters"]["properties"];
            let mut rest = preview;
            while let Some(open) = rest.find('{') {
                let Some(rel) = rest[open + 1..].find('}') else {
                    break;
                };
                let head = rest[open + 1..open + 1 + rel]
                    .split('|')
                    .next()
                    .unwrap_or("")
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .trim();
                if !head.is_empty() {
                    assert!(
                        props.get(head).is_some(),
                        "tool {name} preview references undeclared param {head:?}"
                    );
                }
                rest = &rest[open + 1 + rel + 1..];
            }
        }
    }
}

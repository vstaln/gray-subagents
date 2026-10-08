use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gray_subagents::widget::{AgentRow, AgentState, render_widget};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    process::Command,
};

#[derive(Parser)]
#[command(
    name = "gray-subagents",
    version,
    about = "Gray subagent plugin: Bash commands and above-editor widget"
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
        #[arg(long)]
        name: Option<String>,
    },
    Steer {
        run_id: String,
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
    },
    Status {
        run_id: Option<String>,
    },
    Stop {
        run_id: String,
    },
    List,
    Widget {
        #[arg(long)]
        demo: bool,
    },
    Manifest,
}
fn home() -> Result<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".gray")))
        .context("cannot resolve Gray home")
}
fn settings(model: Option<String>, cap: Option<u32>, effort: Option<String>) -> Result<Value> {
    let path = home()?.join("subagents/settings.json");
    let mut value: Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({"max_running":4,"model":null}),
        Err(e) => return Err(e.into()),
    };
    anyhow::ensure!(value.is_object(), "settings must be an object");
    let changed = model.is_some() || cap.is_some() || effort.is_some();
    if let Some(model) = model {
        let model = resolve_model(&model)?;
        value["model"] = json!(model);
    }
    if let Some(effort) = effort {
        value["effort"] = json!(check_effort(&effort)?);
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
fn backend(action: &str, args: Value, model: Option<&str>, effort: Option<&str>) -> Result<Value> {
    let cfg = settings(None, None, None)?;
    let script = concat!(
        "import sys,json\nns={'__name__':'gray_subagents_backend','__file__':sys.argv[3]}\nexec(sys.argv[1],ns)\nargs=json.loads(sys.argv[2])\naction=sys.argv[4]\n",
        "if action=='run': result=ns['spawn_jobs'](args,args.get('session') or {'cwd':__import__('os').getcwd()})\n",
        "elif action=='status': result=ns['status_job'](args.get('run_id'))\n",
        "elif action=='stop': result=ns['stop_job'](args['run_id'])\n",
        "elif action=='steer': result=ns['steer_job'](args['run_id'],args['message'])\n",
        "elif action=='context': result={'text':ns['completion_notices'](args.get('cwd'))}\n",
        "elif action=='setup': result={'content':ns['setup_profiles']()}\n",
        "elif action=='list': result=ns['list_profiles']()\n",
        "print(json.dumps(result))\n"
    );
    let dir = home()?.join("subagents/runtime");
    std::fs::create_dir_all(&dir)?;
    let runtime = dir.join("supervisor.py");
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    use std::io::Write;
    file.write_all(include_bytes!("../mod"))?;
    file.persist(&runtime)?;
    let mut cmd = Command::new("python3");
    cmd.args(["-c", script, include_str!("../mod"), &args.to_string()])
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
    if let Some(bin) = std::env::var_os("GRAY_BIN") {
        cmd.env("GRAY_SUBAGENTS_BIN", bin);
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
    json!({"name":"subagents","version":env!("CARGO_PKG_VERSION"),"protocol":"1.1","widget":true,"tools":[],"completion":["settings","setup","run","status","stop","steer","list"],"commands":["/subagents"],"hooks":["prompt/context"],"subcommands":["subagents"]})
}
fn widget(demo: bool) -> Result<Value> {
    let mut rows = Vec::new();
    if demo {
        rows.push(AgentRow {
            name: "Scout".into(),
            description: "Find authentication entry points".into(),
            stats: "14.2s".into(),
            state: AgentState::Running {
                activity: "searching…".into(),
            },
        });
        rows.push(AgentRow {
            name: "Reviewer".into(),
            description: "Check session handling".into(),
            stats: "38.4s".into(),
            state: AgentState::Running {
                activity: "reading…".into(),
            },
        });
    } else {
        let cwd = std::env::current_dir()?;
        let dir = home()?.join("subagents/runs");
        let mut managed_pids = std::collections::HashSet::new();
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
                let same_cwd = job["cwd"].as_str().map(PathBuf::from).as_ref() == Some(&cwd);
                let in_tree = active
                    && job["child_pid"]
                        .as_u64()
                        .is_some_and(|pid| visible_in_session(pid));
                if !same_cwd && !in_tree {
                    continue;
                }
                if !active && now - job["finished"].as_f64().unwrap_or(0.0) > 30.0 {
                    continue;
                }
                let state = match status {
                    "running" | "stopping" => AgentState::Running {
                        activity: if status == "stopping" {
                            "stopping…".into()
                        } else {
                            let a = job["activity"].as_str().unwrap_or("");
                            if a.is_empty() {
                                "working…".into()
                            } else {
                                a.into()
                            }
                        },
                    },
                    "completed" => AgentState::Completed,
                    "stopped" => AgentState::Stopped,
                    _ => AgentState::Failed {
                        error: job["error"].as_str().unwrap_or(status).into(),
                    },
                };
                rows.push(AgentRow {
                    name: job["name"]
                        .as_str()
                        .or_else(|| job["agent"].as_str())
                        .unwrap_or("agent")
                        .into(),
                    description: job["task"].as_str().unwrap_or("").into(),
                    stats: format!(
                        "{:.1}s{}",
                        (job["finished"].as_f64().unwrap_or(now)
                            - job["created"].as_f64().unwrap_or(now))
                        .max(0.0),
                        job["model"]
                            .as_str()
                            .filter(|m| !m.is_empty())
                            .map(|m| format!(" · {}", m.rsplit('/').next().unwrap_or(m)))
                            .unwrap_or_default()
                    ),
                    state,
                });
            }
        }
        rows.extend(foreign_workers(&managed_pids));
    }
    let text = render_widget(&rows);
    let shimmer: Vec<usize> = text
        .lines()
        .enumerate()
        .filter_map(|(i, l)| l.contains("⬡").then_some(i))
        .collect();
    Ok(json!({"version":1,"text":text,"shimmer_lines":shimmer}))
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
fn session_root() -> Option<u64> {
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

/// `gray -p` workers no run record owns: mcp `gray_prompt` children and plain
/// scripted fan-outs never register, so the widget adopts them by scanning
/// /proc. Managed children are excluded by pid; workers from other sessions
/// are excluded by lineage.
fn foreign_workers(managed: &std::collections::HashSet<u64>) -> Vec<AgentRow> {
    let mut rows = Vec::new();
    let Some(uptime) = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
    else {
        return rows;
    };
    let self_pid = std::process::id() as u64;
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return rows;
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
        let desc: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        let desc: String = desc.chars().take(60).collect();
        let elapsed = start_ticks(pid)
            .map(|t| (uptime - t as f64 / 100.0).max(0.0))
            .unwrap_or(0.0);
        let cwd_base = std::fs::read_link(entry.path().join("cwd"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into());
        rows.push(AgentRow {
            name: "worker".into(),
            description: if desc.is_empty() {
                "gray -p".into()
            } else {
                desc
            },
            stats: format!("{elapsed:.0}s · {cwd_base} · pid {pid}"),
            state: AgentState::Running {
                activity: format!("unmanaged — `stop {pid}` kills it"),
            },
        });
    }
    rows.sort_by(|a, b| a.stats.cmp(&b.stats));
    rows
}

/// `stop <pid>` for an adopted (unmanaged) worker: TERM, then KILL if it
/// lingers. Only fires on pid-shaped args that resolve to a `gray -p` process
/// in this session's tree — anything else falls through to the registry.
fn foreign_stop(rid: &str) -> Option<Value> {
    if !(rid.len() <= 7 && !rid.is_empty() && rid.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let pid: u64 = rid.parse().ok()?;
    let argv = proc_argv(pid)?;
    gray_p_prompt(&argv)?;
    if !visible_in_session(pid) {
        return None;
    }
    let kill = |sig: &str| {
        Command::new("kill")
            .args([sig, &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    kill("-TERM");
    for _ in 0..20 {
        if proc_stat(pid).is_none() {
            return Some(json!({"content": format!("killed unmanaged worker {pid}")}));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    kill("-KILL");
    std::thread::sleep(std::time::Duration::from_millis(50));
    Some(json!({"content": if proc_stat(pid).is_none() {
        format!("killed unmanaged worker {pid} (SIGKILL)")
    } else {
        format!("worker {pid} did not exit after SIGKILL")
    }}))
}

const EFFORTS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

fn check_effort(effort: &str) -> Result<String> {
    let value = effort.trim().to_lowercase();
    anyhow::ensure!(
        EFFORTS.contains(&value.as_str()),
        "effort must be one of: {}",
        EFFORTS.join(", ")
    );
    Ok(value)
}

/// Resolve a `--model` value against $GRAY_HOME/models.json: exact
/// (case-insensitive) id, then unique prefix, then unique substring.
/// Unknown specs fail with up to 4 catalog candidates. No catalog → pass
/// through and let the child validate.
fn resolve_model(spec: &str) -> Result<String> {
    let spec = spec.trim();
    anyhow::ensure!(
        !spec.is_empty() && spec.len() <= 256 && !spec.contains('\0'),
        "model must be a 1-256 character string"
    );
    let Ok(raw) = std::fs::read(home()?.join("models.json")) else {
        return Ok(spec.to_string());
    };
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&raw) else {
        return Ok(spec.to_string());
    };
    let low = spec.to_lowercase();
    for id in map.keys() {
        if id.to_lowercase() == low {
            return Ok(id.clone());
        }
    }
    let prefixed: Vec<&String> = map
        .keys()
        .filter(|k| k.to_lowercase().starts_with(&low))
        .collect();
    if prefixed.len() == 1 {
        return Ok(prefixed[0].clone());
    }
    let mut hits: Vec<String> = if prefixed.is_empty() {
        map.keys()
            .filter(|k| k.to_lowercase().contains(&low))
            .cloned()
            .collect()
    } else {
        prefixed.into_iter().cloned().collect()
    };
    if hits.len() == 1 {
        return Ok(hits.remove(0));
    }
    hits.sort();
    hits.truncate(4);
    if hits.is_empty() {
        anyhow::bail!("unknown model '{spec}' — no match in the model catalog");
    }
    anyhow::bail!("unknown model '{spec}' — did you mean: {}", hits.join(", "));
}

const USAGE: &str = "Subagents are managed through Bash: gray subagents run [--name NAME] [--agent scout] [--model PROVIDER/MODEL] [--effort LEVEL] 'task'; gray subagents status [ID]; gray subagents steer ID 'follow-up'; gray subagents stop ID; gray subagents settings --model PROVIDER/MODEL [--effort LEVEL]. Per-run --model/--effort override profile frontmatter and the global setting; profiles in ~/.gray/subagents/agents/*.md may open with a `---` block carrying `model:`/`effort:` defaults. Every run gets a readable name (or use --name); ID accepts a name, unique hex prefix, or 'last'. Runs are detached — they keep going if this session ends. Do not invent subagent tools.";

fn execute(action: Action, session: Option<&Value>) -> Result<Value> {
    match action {
        Action::Settings {
            model,
            effort,
            max_running,
        } => settings(model, max_running, effort),
        Action::Setup => backend("setup", json!({}), None, None),
        Action::Run {
            task,
            agent,
            model,
            effort,
            name,
        } => {
            let mut args = json!({"task":task.join(" "),"agent":agent});
            if let Some(model) = model {
                args["model"] = json!(resolve_model(&model)?);
            }
            if let Some(effort) = effort {
                args["effort"] = json!(check_effort(&effort)?);
            }
            if let Some(name) = name {
                args["name"] = json!(name);
            }
            if let Some(session) = session {
                args["session"] = session.clone();
            }
            backend("run", args, None, None)
        }
        Action::Steer { run_id, message } => backend(
            "steer",
            json!({"run_id":run_id,"message":message.join(" ")}),
            None,
            None,
        ),
        Action::Status { run_id } => backend("status", json!({"run_id":run_id}), None, None),
        Action::Stop { run_id } => match foreign_stop(&run_id) {
            Some(v) => Ok(v),
            None => backend("stop", json!({"run_id":run_id}), None, None),
        },
        Action::List => backend("list", json!({}), None, None),
        Action::Widget { demo } => widget(demo),
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
        let result = match req["method"].as_str() {
            Some("plugin/manifest") => Ok(manifest()),
            Some("prompt/context") => {
                if std::env::var_os("GRAY_SUBAGENTS_ACTIVE").is_some() {
                    Ok(json!({"text":""}))
                } else {
                    let cwd = req["params"]["cwd"].as_str().unwrap_or("");
                    let notices = backend("context", json!({"cwd":cwd}), None, None)
                        .ok()
                        .and_then(|v| v["text"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    Ok(json!({"text":format!("{USAGE}\n{notices}")}))
                }
            }
            Some("command/run") => {
                let args = req["params"]["argv"]
                    .as_array()
                    .context("command argv must be an array")?;
                let mut argv = vec!["gray-subagents".to_string()];
                argv.extend(args.iter().filter_map(|v| v.as_str().map(str::to_owned)));
                if argv.len() == 1 {
                    argv.push("status".into());
                }
                let session = req["params"].get("session").cloned();
                Cli::try_parse_from(argv)
                    .map_err(anyhow::Error::from)
                    .and_then(|cli| execute(cli.command.unwrap(), session.as_ref()))
                    .map(|v| json!({"text":present(&v)}))
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
fn main() -> Result<()> {
    if let Some(action) = Cli::parse().command {
        println!("{}", serde_json::to_string_pretty(&execute(action, None)?)?);
        Ok(())
    } else {
        sidecar()
    }
}

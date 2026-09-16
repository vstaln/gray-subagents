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
        max_running: Option<u32>,
    },
    Setup,
    Run {
        task: String,
        #[arg(long, default_value = "scout")]
        agent: String,
        #[arg(long)]
        model: Option<String>,
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
fn settings(model: Option<String>, cap: Option<u32>) -> Result<Value> {
    let path = home()?.join("subagents/settings.json");
    let mut value: Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({"max_running":4,"model":null}),
        Err(e) => return Err(e.into()),
    };
    anyhow::ensure!(value.is_object(), "settings must be an object");
    let changed = model.is_some() || cap.is_some();
    if let Some(model) = model {
        anyhow::ensure!(!model.trim().is_empty(), "model cannot be empty");
        value["model"] = json!(model);
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
fn backend(action: &str, args: Value, model: Option<&str>) -> Result<Value> {
    let cfg = settings(None, None)?;
    let script = concat!(
        "import sys,json\nns={'__name__':'gray_subagents_backend','__file__':sys.argv[3]}\nexec(sys.argv[1],ns)\nargs=json.loads(sys.argv[2])\naction=sys.argv[4]\n",
        "if action=='run': result=ns['spawn_jobs'](args,{'cwd':__import__('os').getcwd()})\n",
        "elif action=='status': result=ns['status_job'](args.get('run_id'))\n",
        "elif action=='stop': result=ns['stop_job'](args['run_id'])\n",
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
    json!({"name":"subagents","version":env!("CARGO_PKG_VERSION"),"protocol":"1.1","widget":true,"tools":[],"completion":["settings","setup","run","status","stop","list"],"commands":["/subagent","/subagents"],"hooks":["prompt/context"],"subcommands":["subagents"]})
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
                if job["cwd"].as_str().map(PathBuf::from).as_ref() != Some(&cwd) {
                    continue;
                }
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
                if !matches!(status, "running" | "stopping")
                    && now - job["finished"].as_f64().unwrap_or(0.0) > 30.0
                {
                    continue;
                }
                let state = match status {
                    "running" | "stopping" => AgentState::Running {
                        activity: if status == "stopping" {
                            "stopping…"
                        } else {
                            "working…"
                        }
                        .into(),
                    },
                    "completed" => AgentState::Completed,
                    "stopped" => AgentState::Stopped,
                    _ => AgentState::Failed {
                        error: job["error"].as_str().unwrap_or(status).into(),
                    },
                };
                rows.push(AgentRow {
                    name: job["agent"].as_str().unwrap_or("agent").into(),
                    description: job["task"].as_str().unwrap_or("").into(),
                    stats: format!(
                        "{:.1}s",
                        (job["finished"].as_f64().unwrap_or(now)
                            - job["created"].as_f64().unwrap_or(now))
                        .max(0.0)
                    ),
                    state,
                });
            }
        }
    }
    let text = render_widget(&rows);
    let shimmer: Vec<usize> = text
        .lines()
        .enumerate()
        .filter_map(|(i, l)| l.contains("⬡").then_some(i))
        .collect();
    Ok(json!({"version":1,"text":text,"shimmer_lines":shimmer}))
}
fn execute(action: Action) -> Result<Value> {
    match action {
        Action::Settings { model, max_running } => settings(model, max_running),
        Action::Setup => backend("setup", json!({}), None),
        Action::Run { task, agent, model } => {
            backend("run", json!({"task":task,"agent":agent}), model.as_deref())
        }
        Action::Status { run_id } => backend("status", json!({"run_id":run_id}), None),
        Action::Stop { run_id } => backend("stop", json!({"run_id":run_id}), None),
        Action::List => backend("list", json!({}), None),
        Action::Widget { demo } => widget(demo),
        Action::Manifest => Ok(manifest()),
    }
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
            Some("prompt/context") => Ok(
                json!({"text":"Subagents are managed through Bash: gray subagents run --agent scout 'task'; gray subagents status; gray subagents stop RUN_ID; gray subagents settings --model PROVIDER/MODEL. Do not invent subagent tools."}),
            ),
            Some("command/run") => {
                let args = req["params"]["argv"]
                    .as_array()
                    .context("command argv must be an array")?;
                let mut argv = vec!["gray-subagents".to_string()];
                argv.extend(args.iter().filter_map(|v| v.as_str().map(str::to_owned)));
                if argv.len() == 1 {
                    argv.push("status".into());
                }
                Cli::try_parse_from(argv)
                    .map_err(anyhow::Error::from)
                    .and_then(|cli| execute(cli.command.unwrap()))
                    .map(|v| json!({"text":serde_json::to_string_pretty(&v).unwrap()}))
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
        println!("{}", serde_json::to_string_pretty(&execute(action)?)?);
        Ok(())
    } else {
        sidecar()
    }
}

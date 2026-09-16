#!/usr/bin/env python3
"""gray sidecar: subagents — background agent delegation (protocol 1.1).

Spawns child `gray -p` runs (profile instructions prepended to the task) as background jobs.
The 30 s host `tool/call` TTL rules out long blocking delegation, so `subagent`
returns a run_id immediately; finished runs are reported once via the
`prompt/context` hook, and `subagents_status` / `subagents_stop` manage them.

Job state lives in $GRAY_HOME/subagents/runs/<run_id>.json. A detached
supervisor records completion even after the sidecar exits. Linux is required.
Env: GRAY_SUBAGENTS_BIN (default `gray`), GRAY_SUBAGENTS_TIMEOUT_SECS (600),
GRAY_SUBAGENTS_MAX_RUNNING (4). GRAY_SUBAGENTS_ACTIVE=1 is the recursion
guard: sidecars inside child runs refuse to spawn further subagents.
"""
import contextlib
import fcntl
import json
import math
import os
import re
import selectors
import shutil
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

BUILTIN_PROFILES = {
    "scout": "# Scout\n\nInspect the codebase without editing files. Report relevant paths, entry points, data flow, and risks. Be fast and concrete; flag uncertainties.",
    "worker": "# Worker\n\nImplement the assigned task. Edit files, validate your work, and escalate unapproved decisions instead of guessing. Finish with what changed and how it was verified.",
    "reviewer": "# Reviewer\n\nReview the change or code in question. Report correctness issues, missing tests, edge cases, and unnecessary complexity. Small fixes are allowed; say which you made.",
    "oracle": "# Oracle\n\nProvide a second opinion. Challenge assumptions, point out what might be missing, and argue the strongest counter-position. Do not edit files.",
}

TOOLS = [
    {
        "name": "subagent",
        "description": (
            "Delegate a task to a named subagent profile. Runs in the background:"
            " returns a run_id immediately; the result is reported automatically at"
            " the start of your next turn. Do not poll — keep working or end your"
            " turn. Use subagents_status to list runs or subagents_stop to cancel."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "agent": {"type": "string", "description": "Profile name (see subagents_list). Default: scout."},
                "task": {"type": "string", "description": "Complete, self-contained instructions for one subagent."},
                "tasks": {"type": "array", "minItems": 1, "maxItems": 8,
                          "description": "Parallel tasks; use instead of top-level task/agent.",
                          "items": {"type": "object", "properties": {"agent": {"type": "string"}, "task": {"type": "string"}}, "required": ["task"]}},
            },
            "oneOf": [{"required": ["task"]}, {"required": ["tasks"]}],
        },
    },
    {
        "name": "subagents_status",
        "description": "List subagent runs (most recent first), or show one run when run_id is given.",
        "parameters": {
            "type": "object",
            "properties": {"run_id": {"type": "string", "description": "Optional run to inspect."}},
        },
    },
    {
        "name": "subagents_stop",
        "description": "Stop a running subagent.",
        "parameters": {
            "type": "object",
            "properties": {"run_id": {"type": "string"}},
            "required": ["run_id"],
        },
    },
    {
        "name": "subagents_list",
        "description": "List available subagent profiles (name, source, description).",
        "parameters": {"type": "object", "properties": {}},
    },
]

MANIFEST = {
    "name": "subagents",
    "version": "0.2.0",
    "protocol": "1.1",
    "tools": TOOLS,
    "commands": ["/subagents"],
    "subcommands": ["subagents"],
    "hooks": ["prompt/context"],
}


# ---------------------------------------------------------------------------
# Persistent jobs. Linux/POSIX: flock for admission; /proc birth time prevents
# signalling a reused PID. A detached supervisor owns each child and its pipes.
# ---------------------------------------------------------------------------
OUTPUT_LIMIT = 256 * 1024
CONTENT_LIMIT = 40000
ACTIVE = {"running", "stopping"}
SUPERVISORS = []
NAME = re.compile(r"[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}\Z")
RUN_ID = re.compile(r"[0-9a-f]{32}\Z")


def home():
    return Path(os.environ.get("GRAY_HOME") or Path.home() / ".gray").expanduser().resolve()


def root():
    return home() / "subagents"


def agents_dir():
    return root() / "agents"


def runs_dir():
    return root() / "runs"


@contextlib.contextmanager
def registry():
    runs_dir().mkdir(parents=True, exist_ok=True, mode=0o700)
    with (runs_dir() / ".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def job_path(rid):
    if not isinstance(rid, str) or not RUN_ID.fullmatch(rid):
        raise ValueError("unknown or invalid run_id")
    return runs_dir() / f"{rid}.json"


def read_job(rid):
    try:
        job = json.loads(job_path(rid).read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise ValueError(f"unknown run_id {rid}") from None
    if not isinstance(job, dict) or job.get("run_id") != rid:
        raise ValueError(f"invalid job record {rid}")
    return job


def write_job(job):
    # All writers hold the registry lock; rename never exposes partial JSON.
    path = job_path(job["run_id"])
    tmp = path.with_suffix(".tmp")
    with tmp.open("w", encoding="utf-8", newline="\n") as stream:
        os.chmod(tmp, 0o600)
        json.dump(job, stream, ensure_ascii=True)
        stream.write("\n")
    tmp.replace(path)


def all_jobs():
    return sorted((read_job(p.stem) for p in runs_dir().glob("*.json")),
                  key=lambda job: job["created"], reverse=True)


def identity(pid):
    """Linux process birth tick; zombies and missing processes are not alive."""
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
        return fields[19] if fields[0] != "Z" else None
    except (OSError, IndexError):
        return None


def kill_orphan(job):
    pid = job.get("child_pid")
    birth = job.get("child_birth")
    if pid and birth and identity(pid) == birth:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def sweep():
    for proc in SUPERVISORS[:]:
        if proc.poll() is not None:
            SUPERVISORS.remove(proc)
    for job in all_jobs():
        if job["status"] in ACTIVE and identity(job.get("supervisor_pid")) != job.get("supervisor_birth"):
            kill_orphan(job)
            job.update(status="lost", error="supervisor exited before recording a result", finished=time.time())
            write_job(job)


def profile_list():
    names = set(BUILTIN_PROFILES)
    if agents_dir().is_dir():
        names.update(p.stem for p in agents_dir().glob("*.md") if p.is_file() and NAME.fullmatch(p.stem))
    return sorted(names)


def load_profile(name):
    if not isinstance(name, str) or not NAME.fullmatch(name):
        raise ValueError("invalid agent name")
    path = agents_dir() / f"{name}.md"
    try:
        with path.open(encoding="utf-8") as stream:
            text = stream.read(65537)
    except FileNotFoundError:
        if name not in BUILTIN_PROFILES:
            raise ValueError(f"unknown agent '{name}'") from None
        text = BUILTIN_PROFILES[name]
    if not text.strip() or len(text) > 65536:
        raise ValueError(f"profile '{name}' must contain 1–65536 characters")
    return text.strip()


def setup_profiles():
    agents_dir().mkdir(parents=True, exist_ok=True)
    created, kept = [], []
    for name, body in BUILTIN_PROFILES.items():
        try:
            with (agents_dir() / f"{name}.md").open("x", encoding="utf-8", newline="\n") as stream:
                stream.write(body + "\n")
            created.append(name)
        except FileExistsError:
            kept.append(name)
    return f"Profiles in {agents_dir()}: created {', '.join(created) or 'none'}; kept {', '.join(kept) or 'none'}"


def list_profiles():
    return {"content": "\n".join(
        f"{name} ({'user' if (agents_dir() / (name + '.md')).is_file() else 'bundled'}): "
        + next((line.strip() for line in load_profile(name).splitlines() if line.strip() and not line.startswith('#')), name)
        for name in profile_list())}


def limited(text, limit=CONTENT_LIMIT):
    raw = text.encode("utf-8")
    if len(raw) <= limit:
        return text
    return raw[:limit].decode("utf-8", errors="ignore") + "\n[truncated; complete captured output is in the private run record]"


def job_view(job):
    view = {key: job.get(key, "") for key in ("run_id", "agent", "task", "status", "result", "error")}
    view["content"] = limited(f"{job['run_id']} [{job['status']}] {job['agent']}\n{job.get('error', '')}\n{job.get('result', '')}")
    return view


def status_job(rid=None):
    with registry():
        sweep()
        if rid is not None:
            return job_view(read_job(rid))
        jobs = all_jobs()
    return {"content": limited("\n".join(f"{j['run_id']} [{j['status']}] {j['agent']}: {j['task'][:100]}" for j in jobs)) or "no subagent runs",
            "jobs": [{k: j[k] for k in ("run_id", "agent", "status", "created")} for j in jobs]}


def stop_job(rid):
    with registry():
        sweep()
        job = read_job(rid)
        if job["status"] not in ACTIVE:
            raise ValueError(f"run {rid} already {job['status']}")
        job["status"] = "stopping"
        write_job(job)
    return {"content": f"Cancellation requested for {rid}", "status": "stopping"}


def settings():
    ttl = float(os.environ.get("GRAY_SUBAGENTS_TIMEOUT_SECS", "600"))
    cap = int(os.environ.get("GRAY_SUBAGENTS_MAX_RUNNING", "4"))
    if not math.isfinite(ttl) or not 0 < ttl <= 86400 or not 1 <= cap <= 32:
        raise ValueError("timeout must be >0 and <=86400 seconds; max running must be 1–32")
    return ttl, cap


def spawn_jobs(args, session):
    if os.environ.get("GRAY_SUBAGENTS_ACTIVE"):
        raise ValueError("recursion guard: nested delegation is disabled")
    if "tasks" in args:
        if "task" in args or "agent" in args:
            raise ValueError("use either task/agent or tasks, not both")
        tasks = args["tasks"]
        if not isinstance(tasks, list) or not 1 <= len(tasks) <= 8:
            raise ValueError("tasks must contain 1–8 items")
    else:
        tasks = [args]
    cwd = session.get("cwd") or os.getcwd()
    if not isinstance(cwd, str) or not Path(cwd).is_dir():
        raise ValueError("session cwd must be an existing directory")
    cwd = str(Path(cwd).resolve())
    ttl, cap = settings()
    binary = shutil.which(os.environ.get("GRAY_SUBAGENTS_BIN") or "gray")
    if not binary:
        raise ValueError("Gray executable not found; set GRAY_SUBAGENTS_BIN")
    prepared = []
    for item in tasks:
        if not isinstance(item, dict):
            raise ValueError("each task must be an object")
        task, agent = item.get("task"), item.get("agent", "scout")
        if not isinstance(task, str) or not task.strip() or len(task) > 32000 or "\0" in task:
            raise ValueError("task must contain 1–32000 nonempty characters and no NUL")
        body = load_profile(agent)
        prompt = (f"You are the '{agent}' subagent of a parent Gray session.\n\n{body}\n\n"
                  f"# Task\n\n{task}\n\nWork only on this assignment. Do not spawn more agents. "
                  "If blocked, report the blocker; do not guess unapproved decisions. "
                  "Return findings, changed paths, and verification in your final answer.")
        if "\0" in prompt:
            raise ValueError("profile contains NUL")
        prepared.append((agent, task, prompt))
    started = []
    with registry():
        sweep()
        running = sum(j["status"] in ACTIVE for j in all_jobs())
        if running + len(prepared) > cap:
            raise ValueError(f"{running} jobs running; batch would exceed running cap {cap}")
        for agent, task, prompt in prepared:
            job = dict(run_id=uuid.uuid4().hex, agent=agent, task=task, prompt=prompt,
                       cwd=cwd, session_id=session.get("id", ""), binary=str(Path(binary).resolve()),
                       timeout=ttl, created=time.time(), status="running", result="", error="", noticed=False)
            try:
                proc = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--supervise", job["run_id"]],
                                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                        start_new_session=True, env=dict(os.environ, GRAY_HOME=str(home())))
                SUPERVISORS.append(proc)
                job.update(supervisor_pid=proc.pid, supervisor_birth=identity(proc.pid))
                write_job(job)
            except OSError:
                # Stop any accepted part of the batch rather than leave it unnoticed.
                for rid in started:
                    prior = read_job(rid)
                    prior["status"] = "stopping"
                    write_job(prior)
                raise
            started.append(job["run_id"])
    text = "Started subagent runs: " + ", ".join(started) + ". Results arrive at your next turn; status/stop are available."
    return {"content": text, "run_ids": started, **({"run_id": started[0]} if "tasks" not in args else {})}


def completion_notices(cwd):
    if os.environ.get("GRAY_SUBAGENTS_ACTIVE"):
        return ""
    cwd = str(Path(cwd or os.getcwd()).resolve())
    lines, size = [], 0
    with registry():
        sweep()
        for job in reversed(all_jobs()):
            if job["status"] in ACTIVE or job.get("noticed") or job["cwd"] != cwd:
                continue
            text = limited(job_view(job)["content"], 6000)
            if size + len(text.encode()) > CONTENT_LIMIT:
                break
            lines.append(text)
            size += len(text.encode())
            job["noticed"] = True
            write_job(job)
    return "Subagent results (child output; verify before relying on it):\n" + "\n".join(lines) if lines else ""


def supervise(rid):
    """Survives plugin EOF/shutdown; owns exit status, timeout and bounded pipes."""
    proc = None
    output = bytearray()
    status, error = "failed", "supervisor did not finish"
    try:
        with registry():
            job = read_job(rid)
            if job["status"] == "stopping":
                status, error = "stopped", "stopped before launch"
                return
            env = dict(os.environ, GRAY_SUBAGENTS_ACTIVE="1", NO_COLOR="1", TERM="dumb", GRAY_SHOW_REASONING="0")
            proc = subprocess.Popen([job["binary"], "-p", job["prompt"]], cwd=job["cwd"], env=env,
                                    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                    start_new_session=True)
            job.update(child_pid=proc.pid, child_birth=identity(proc.pid))
            write_job(job)
        deadline = time.monotonic() + job["timeout"]
        with selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ)
            while True:
                with registry():
                    stopping = read_job(rid)["status"] == "stopping"
                if stopping:
                    status, error = "stopped", "stopped by request"
                    break
                if time.monotonic() >= deadline:
                    status, error = "timeout", f"timed out after {job['timeout']:g}s; side effects may already have occurred"
                    break
                events = selector.select(.05)
                if events:
                    chunk = os.read(proc.stdout.fileno(), 65536)
                    room = OUTPUT_LIMIT - len(output)
                    output.extend(chunk[:room])
                    if len(chunk) > room:
                        status, error = "failed", "child exceeded output limit (256 KiB)"
                        break
                    if not chunk:
                        selector.unregister(proc.stdout)
                code = proc.poll()
                if code is not None:
                    # Don't let a surviving descendant keep stdout open forever.
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    if not selector.get_map():
                        status = "completed" if code == 0 and output.strip() else "failed"
                        error = "" if status == "completed" else (f"child exited {code}" if code else "child returned empty output")
                        break
    except Exception as exc:
        error = f"supervisor failed: {type(exc).__name__}: {exc}"
    finally:
        if proc is not None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()
            proc.stdout.close()
        text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", output.decode("utf-8", errors="replace"))
        with registry():
            job = read_job(rid)
            if job["status"] == "stopping":
                status, error = "stopped", "stopped by request"
            job.update(status=status, error=error, result=text.strip(), finished=time.time())
            write_job(job)


def dispatch(method, params):
    if not isinstance(params, dict):
        raise ValueError("params must be an object")
    if method == "plugin/manifest":
        return MANIFEST
    if method == "prompt/context":
        return {"text": completion_notices(params.get("cwd"))}
    if method == "tool/call":
        args = params.get("args", {})
        if not isinstance(args, dict):
            raise ValueError("tool args must be an object")
        name = params.get("name")
        if name == "subagent":
            session = params.get("session", {})
            if not isinstance(session, dict):
                raise ValueError("session must be an object")
            return spawn_jobs(args, session)
        if name == "subagents_status":
            return status_job(args.get("run_id"))
        if name == "subagents_stop":
            return stop_job(args.get("run_id"))
        if name == "subagents_list":
            return list_profiles()
        raise ValueError(f"unknown tool {name}")
    if method == "command/run":
        if params.get("name") not in ("/subagents", "subagents"):
            raise ValueError("unknown command")
        argv = params.get("argv", [])
        if argv == ["setup"]:
            return {"text": setup_profiles()}
        if argv == ["list"]:
            return {"text": list_profiles()["content"]}
        if argv == ["status"]:
            return {"text": status_job()["content"]}
        return {"text": "Usage: /subagents setup|list|status"}
    raise ValueError(f"unknown method {method}")


def main():
    if sys.argv[1:2] == ["--supervise"] and len(sys.argv) == 3:
        supervise(sys.argv[2])
        return
    if sys.argv[1:] == ["setup"]:
        print(setup_profiles())
        return
    if sys.argv[1:]:
        raise ValueError("usage: mod [setup]")
    for line in sys.stdin:
        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(request, dict):
            continue
        method = request.get("method")
        if method == "plugin/shutdown":
            return
        if "id" not in request:
            continue
        reply = {"id": request["id"]}
        try:
            reply["result"] = dispatch(method, request.get("params", {}))
        except (OSError, ValueError, TypeError) as exc:
            if method == "tool/call":
                reply["result"] = {"content": str(exc), "is_error": True}
            else:
                reply["error"] = str(exc)
        print(json.dumps(reply), flush=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError) as exc:
        print(str(exc), file=sys.stderr)
        sys.exit(1)

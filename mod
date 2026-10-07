#!/usr/bin/env python3
"""gray sidecar: subagents — background agent delegation (protocol 1.1).

Spawns child `gray -p` runs (profile instructions prepended to the task) as background jobs.
The 30 s host `tool/call` TTL rules out long blocking delegation, so `subagent`
returns a run_id immediately; finished runs are reported once via the
`prompt/context` hook, and `subagents_status` / `subagents_stop` manage them.

Job state lives in $GRAY_HOME/subagents/runs/<run_id>.json. A detached
supervisor records completion even after the sidecar exits. Steering queues a
follow-up on the run; the supervisor delivers it as a `--session` resume turn
in the same child session between phases. Linux is required.
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
        "name": "subagents_steer",
        "description": (
            "Send a follow-up message to a subagent run, in its own session."
            " While the run is active the message queues and lands when the"
            " current turn ends; on a finished or stopped run it resumes the"
            " child session immediately. Use subagents_stop first to redirect"
            " a runaway turn."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "run_id": {"type": "string"},
                "message": {"type": "string", "description": "Follow-up instructions for the same child session."},
            },
            "required": ["run_id", "message"],
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


def resolve_rid(rid):
    """Accept a full 32-hex id, an unambiguous >=4-char prefix, or 'last'."""
    if not isinstance(rid, str):
        raise ValueError("unknown or invalid run_id")
    if RUN_ID.fullmatch(rid):
        return rid
    if rid == "last":
        jobs = all_jobs()
        if not jobs:
            raise ValueError("no subagent runs yet")
        return jobs[0]["run_id"]
    if not re.fullmatch(r"[0-9a-f]{4,31}", rid):
        raise ValueError("unknown or invalid run_id")
    hits = [p.stem for p in runs_dir().glob(f"{rid}*.json")]
    if len(hits) != 1:
        raise ValueError(f"run_id '{rid}' matches {len(hits)} runs; give more digits")
    return hits[0]


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
    view["activity"] = job.get("activity", "")
    view["child_session"] = job.get("child_session", "")
    view["steer_queue"] = len(job.get("steer_queue") or [])
    view["phases"] = len(job.get("phases") or [])
    queue = f" · {view['steer_queue']} steered" if view["steer_queue"] else ""
    activity = f"\n{job.get('activity', '')}" if job["status"] in ACTIVE and job.get("activity") else ""
    view["content"] = limited(f"{job['run_id']} [{job['status']}{queue}] {job['agent']}{activity}\n{job.get('error', '')}\n{job.get('result', '')}")
    return view


def status_job(rid=None):
    with registry():
        sweep()
        if rid is not None:
            return job_view(read_job(resolve_rid(rid)))
        jobs = all_jobs()
    return {"content": limited("\n".join(f"{j['run_id'][:8]} [{j['status']}] {j['agent']}: {j['task'][:100]}" for j in jobs)) or "no subagent runs",
            "jobs": [{k: j[k] for k in ("run_id", "agent", "status", "created")} for j in jobs]}


def stop_job(rid):
    with registry():
        sweep()
        job = read_job(resolve_rid(rid))
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
                       timeout=ttl, created=time.time(), status="running", result="", error="",
                       noticed=False, activity="", child_session="", steer_queue=[], phases=[])
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
    text = ("Started subagent runs: " + ", ".join(started) +
            ". Results arrive at your next turn; status/stop are available, "
            "and steer (gray subagents steer RUN_ID 'follow-up') redirects the same child session.")
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


def steer_job(rid, message):
    """Queue a follow-up into the child's own session. Running jobs pick the
    message up between phases; finished jobs are revived by a new supervisor
    that resumes the recorded child session."""
    if os.environ.get("GRAY_SUBAGENTS_ACTIVE"):
        raise ValueError("recursion guard: nested delegation is disabled")
    if not isinstance(message, str) or not message.strip() or len(message) > 16000 or "\0" in message:
        raise ValueError("message must contain 1-16000 nonempty characters and no NUL")
    with registry():
        sweep()
        job = read_job(resolve_rid(rid))
        if job["status"] == "stopping":
            raise ValueError(f"run {rid} is stopping")
        queue = job.setdefault("steer_queue", [])
        if len(queue) >= 16:
            raise ValueError("steer queue is full (16 pending)")
        queue.append(message)
        job["noticed"] = False
        if job["status"] in ACTIVE:
            write_job(job)
            where = "queued; lands when the current turn ends"
        else:
            if not job.get("child_session"):
                raise ValueError(f"run {rid} has no resumable child session")
            job.update(status="running", error="", finished=None,
                       created=time.time(), activity="")
            mod = str(Path(__file__).resolve())
            try:
                proc = subprocess.Popen([sys.executable, mod, "--supervise", rid],
                                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                        stderr=subprocess.DEVNULL, start_new_session=True,
                                        env=dict(os.environ, GRAY_HOME=str(home())))
                SUPERVISORS.append(proc)
                job.update(supervisor_pid=proc.pid, supervisor_birth=identity(proc.pid))
            except OSError:
                job["steer_queue"].pop()
                job["status"] = "failed"
                write_job(job)
                raise
            write_job(job)
            where = "resuming the child session now"
    return {"content": f"Steer for {job['run_id']}: {where}", "queued": len(queue)}


def describe_row(row):
    """One-line live activity from a `--json` progress row (widget/status)."""
    phase, label, detail = row.get("phase"), row.get("label") or row.get("tool") or "", row.get("detail") or ""
    if phase in ("tool_started", "tool_ran", "tool_finished"):
        text = f"{label} {detail}".strip()
    elif phase == "thinking":
        text = "thinking"
    elif phase == "provider_retry":
        text = f"provider retry {detail}".strip()
    elif phase == "compacted":
        text = "compacting context"
    elif phase == "text":
        text = "replying"
    elif phase == "persisting":
        text = "finishing"
    else:
        text = phase or "working"
    return text[:120]


def run_phase(rid, prompt, resume_sid, binary, cwd, timeout):
    """One child invocation: `gray [--session sid] -p <prompt> --json`.

    Protocol rows (protocol==1) yield the child session id, live activity and
    the final text; any other stdout is kept verbatim as the result fallback
    so older/plain children still work. Returns (status, error, result)."""
    env = dict(os.environ, GRAY_SUBAGENTS_ACTIVE="1", NO_COLOR="1",
               TERM="dumb", GRAY_SHOW_REASONING="0")
    argv = [binary] + (["--session", resume_sid] if resume_sid else []) + ["-p", prompt, "--json"]
    proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            start_new_session=True)
    with registry():
        job = read_job(rid)
        job.update(child_pid=proc.pid, child_birth=identity(proc.pid))
        write_job(job)
    raw, pending = bytearray(), b""
    st = {"sid": "", "activity": "", "result": "", "error_row": "", "plain": bytearray()}
    status, error = "failed", "supervisor did not finish"

    def feed(line):
        try:
            row = json.loads(line)
        except ValueError:
            row = None
        if isinstance(row, dict) and row.get("protocol") == 1:
            if row.get("session_id"):
                st["sid"] = row["session_id"]
            if row.get("type") == "progress":
                st["activity"] = describe_row(row)
            elif row.get("type") == "result":
                st["result"] = row.get("text") or ""
            elif row.get("type") == "error":
                st["error_row"] = row.get("message") or str(row.get("code") or "error")
        else:
            st["plain"].extend(line + b"\n")

    try:
        deadline = time.monotonic() + timeout
        with selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ)
            while True:
                with registry():
                    job = read_job(rid)
                    stopping = job["status"] == "stopping"
                    changed = False
                    if st["sid"] and job.get("child_session") != st["sid"]:
                        job["child_session"] = st["sid"]
                        changed = True
                    if st["activity"] and job.get("activity") != st["activity"]:
                        job["activity"] = st["activity"]
                        changed = True
                    if changed:
                        write_job(job)
                if stopping:
                    status, error = "stopped", "stopped by request"
                    break
                if time.monotonic() >= deadline:
                    status, error = "timeout", f"timed out after {timeout:g}s; side effects may already have occurred"
                    break
                events = selector.select(.05)
                if events:
                    chunk = os.read(proc.stdout.fileno(), 65536)
                    room = OUTPUT_LIMIT - len(raw)
                    raw.extend(chunk[:room])
                    if len(chunk) > room:
                        status, error = "failed", "child exceeded output limit (256 KiB)"
                        break
                    if not chunk:
                        selector.unregister(proc.stdout)
                    pending += chunk[:room]
                    while b"\n" in pending:
                        line, pending = pending.split(b"\n", 1)
                        feed(line)
                code = proc.poll()
                if code is not None:
                    # Don't let a surviving descendant keep stdout open forever.
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    if not selector.get_map():
                        if status == "failed" and error == "supervisor did not finish":
                            status = "exited"
                        break
        if pending.strip():
            feed(pending)
        result = st["result"].strip() or re.sub(
            r"\x1b\[[0-?]*[ -/]*[@-~]", "",
            st["plain"].decode("utf-8", errors="replace")).strip()
        if status == "exited":
            if code == 0 and result:
                status, error = "ok", ""
            else:
                status = "failed"
                error = st["error_row"] or (f"child exited {code}" if code else "child returned empty output")
        return status, error, result
    finally:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait()
        proc.stdout.close()


def supervise(rid):
    """Survives plugin EOF/shutdown; owns exit status, timeout and bounded pipes.
    Loops phases: the task turn, then each queued steer as a --session resume
    turn in the same child session. A failed phase ends the run."""
    final_status, final_error = "failed", "supervisor did not finish"
    try:
        while True:
            with registry():
                job = read_job(rid)
                if job["status"] == "stopping":
                    job["steer_queue"] = []
                    write_job(job)
                    return
                phases = job.setdefault("phases", [])
                if not phases:
                    prompt, resume_sid, kind = job["prompt"], None, "task"
                else:
                    queue = job.setdefault("steer_queue", [])
                    if not queue:
                        final_status, final_error = "completed", ""
                        return
                    prompt = queue.pop(0)
                    resume_sid = job.get("child_session")
                    if not resume_sid:
                        final_status, final_error = "failed", "child session unavailable for steering"
                        return
                    kind = "steer"
                phases.append({"kind": kind, "prompt": prompt[:2000],
                               "started": time.time(), "status": "running",
                               "result": "", "error": ""})
                job["activity"] = ""
                write_job(job)
            pst, perr, ptext = run_phase(rid, prompt, resume_sid, job["binary"], job["cwd"], job["timeout"])
            with registry():
                job = read_job(rid)
                phase = job["phases"][-1]
                phase.update(status=pst, error=perr, result=ptext, finished=time.time())
                if ptext:
                    job["result"] = ptext
                if job["status"] == "stopping":
                    job["steer_queue"] = []
                    write_job(job)
                    return
                job["error"] = perr
                write_job(job)
            if pst == "stopped":
                return
            if pst != "ok":
                final_status, final_error = pst, perr
                return
    except Exception as exc:
        final_status, final_error = "failed", f"supervisor failed: {type(exc).__name__}: {exc}"
    finally:
        with registry():
            job = read_job(rid)
            if job["status"] == "stopping":
                final_status, final_error = "stopped", "stopped by request"
            job.update(status=final_status, error=final_error,
                       finished=time.time(), noticed=False)
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
        if name == "subagents_steer":
            return steer_job(args.get("run_id"), args.get("message"))
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
        if argv[:1] == ["status"] and len(argv) == 2:
            return {"text": status_job(argv[1])["content"]}
        if argv[:1] == ["stop"] and len(argv) == 2:
            return {"text": stop_job(argv[1])["content"]}
        if argv[:1] == ["steer"] and len(argv) >= 3:
            return {"text": steer_job(argv[1], " ".join(argv[2:]))["content"]}
        return {"text": "Usage: /subagents setup|list|status [RUN_ID]|stop RUN_ID|steer RUN_ID MESSAGE"}
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

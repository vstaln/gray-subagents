"""Wire-level tests for the subagents sidecar (protocol 1.1 contract).

Persistent sidecars mirror the host. Detached supervisors own children; tests
also exercise shutdown, a fresh observer, and concurrent sidecar instances.
"""
import json, os, pathlib, re, select, signal, subprocess, tempfile, time, unittest
import importlib.machinery, importlib.util
from unittest import mock

REPO = pathlib.Path(__file__).resolve().parent.parent
MOD = REPO / "mod"
FAKE_GRAY = REPO / "tests" / "fixtures" / "fake-gray"


class Sidecar:
    def __init__(self, env):
        self.proc = subprocess.Popen(
            [str(MOD)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            text=True, env=env)

    def rpc(self, method, params=None, rid=1):
        req = {"id": rid, "method": method}
        if params is not None:
            req["params"] = params
        self.proc.stdin.write(json.dumps(req) + "\n")
        self.proc.stdin.flush()
        ready, _, _ = select.select([self.proc.stdout], [], [], 30)
        assert ready, "sidecar did not reply within 30s"
        return json.loads(self.proc.stdout.readline())

    def call(self, name, args, rid=2):
        reply = self.rpc("tool/call", {"name": name, "args": args}, rid)
        assert "result" in reply, reply
        result = reply["result"]
        assert isinstance(result.get("content"), str), result  # real Gray requires content
        return result

    def close(self):
        if self.proc.poll() is None:
            self.proc.stdin.write('{"method":"plugin/shutdown"}\n')
            self.proc.stdin.flush()
            self.proc.wait(timeout=5)
        self.proc.stdin.close()
        self.proc.stdout.close()


class SidecarCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = pathlib.Path(self.tmp.name) / "home"
        self.run_dir = self.home / "subagents" / "runs"
        # The suite itself may run inside a subagent or a live session: every
        # GRAY_* var the caller exports (session id, model, recursion guards,
        # binary/provider/context overrides) must not leak into spawned test
        # runs — the tests assert defaults.
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith("GRAY_")}
        self.env.update(
            GRAY_HOME=str(self.home),
            # Pinned session-less: present-but-empty beats the ancestor walk
            # (owner_session), so the suite resolves no session even when it
            # runs inside a live one.
            GRAY_SESSION_ID="",
            GRAY_SUBAGENTS_BIN=str(FAKE_GRAY),
            FAKE_LOG=str(pathlib.Path(self.tmp.name) / "log.txt"),
            GRAY_SUBAGENTS_TIMEOUT_SECS="6",
            GRAY_SUBAGENTS_MAX_RUNNING="2",
        )
        self.sc = Sidecar(self.env)
        self.addCleanup(self.tmp.cleanup)
        self.addCleanup(self.sc.close)
        self.addCleanup(self.stop_all)

    def stop_all(self):
        with_sc = Sidecar(self.env)
        try:
            reply = with_sc.call("subagents_status", {})
            for job in reply.get("jobs", []):
                if job["status"] in ("running", "stopping", "queued"):
                    with_sc.call("subagents_stop", {"run_id": job["run_id"]})
                    for _ in range(150):
                        st = with_sc.call("subagents_status", {"run_id": job["run_id"]})
                        if st.get("status") not in ("running", "stopping", "queued"):
                            break
                        time.sleep(.02)
        finally:
            with_sc.close()

    def wait_status(self, rid, want, tries=200):
        for _ in range(tries):
            st = self.sc.call("subagents_status", {"run_id": rid})
            if st.get("status") == want:
                return st
            time.sleep(0.05)
        self.fail(f"run {rid} never reached {want}: {st}")

    def spawn(self, task="do work", agent="scout", env_extra=None, args_extra=None, cwd=None):
        env = dict(self.env)
        env.update(env_extra or {})
        proc = Sidecar(env)
        self.addCleanup(proc.close)
        args = {"agent": agent, "task": task}
        args.update(args_extra or {})
        params = {"name": "subagent", "args": args}
        if cwd is not None:
            params["session"] = {"cwd": str(cwd)}
        reply = proc.rpc("tool/call", params)
        assert "result" in reply, reply
        result = reply["result"]
        assert isinstance(result.get("content"), str), result
        return result

    def git_repo(self):
        repo = pathlib.Path(self.tmp.name) / "repo"
        repo.mkdir()
        (repo / "seed.txt").write_text("seed\n")
        for argv in (["git", "init", "-q"],
                     ["git", "config", "user.email", "test@example.com"],
                     ["git", "config", "user.name", "test"],
                     ["git", "add", "seed.txt"],
                     ["git", "commit", "-qm", "seed"]):
            subprocess.run(argv, cwd=repo, check=True, capture_output=True)
        return repo

    def worktree_path(self, rid):
        return self.home / "subagents" / "worktrees" / rid

    # -- manifest ------------------------------------------------------------
    def test_manifest_declares_tools_command_and_hook(self):
        r = self.sc.rpc("plugin/manifest", rid=1)["result"]
        self.assertEqual(r["name"], "subagents")
        self.assertEqual(r["protocol"], "1.1")
        names = {t["name"] for t in r["tools"]}
        self.assertIn("subagent", names)
        self.assertIn("subagents_status", names)
        self.assertIn("subagents_stop", names)
        self.assertIn("subagents_list", names)
        self.assertIn("/subagents", r["commands"])
        self.assertIn("prompt/context", r["hooks"])

    # -- delegation ----------------------------------------------------------
    def test_subagent_spawns_job_and_completes(self):
        r = self.sc.call("subagent", {"agent": "scout", "task": "find auth code"})
        self.assertNotIn("is_error", r)
        rid = r["run_id"]
        st = self.wait_status(rid, "completed")
        self.assertTrue(st["result"])
        payload = json.loads(st["result"])
        self.assertIn("# Scout", payload["prompt"])
        self.assertIn("find auth code", payload["prompt"])
        self.assertEqual(payload["active"], "1")

    def test_subagent_unknown_agent_is_tool_error(self):
        r = self.sc.call("subagent", {"agent": "nope", "task": "x"})
        self.assertTrue(r.get("is_error"))
        self.assertIn("nope", r["content"])

    def test_subagent_empty_task_is_tool_error(self):
        r = self.sc.call("subagent", {"agent": "scout", "task": "   "})
        self.assertTrue(r.get("is_error"))

    def test_recursion_guard_blocks_child_from_spawning(self):
        r = self.spawn(task="x", env_extra={"GRAY_SUBAGENTS_ACTIVE": "1"})
        self.assertTrue(r.get("is_error"))
        self.assertIn("recursion", r["content"].lower())

    def read_record(self, rid):
        return json.loads((self.run_dir / f"{rid}.json").read_text(encoding="utf-8"))

    def test_running_cap_queues_fifo_and_starts_as_slots_free(self):
        env = dict(FAKE_MODE="slow", FAKE_SLEEP="20", GRAY_SUBAGENTS_TIMEOUT_SECS="60")
        busy = [self.spawn(task=f"busy{i}", env_extra=env)["run_id"] for i in range(2)]
        first = self.spawn(task="third", env_extra=env)
        self.assertNotIn("is_error", first)
        self.assertIn("queued (cap 2; 0 ahead)", first["content"])
        second = self.spawn(task="fourth", env_extra=env)
        self.assertIn("queued (cap 2; 1 ahead)", second["content"])
        q1, q2 = first["run_id"], second["run_id"]
        try:
            self.assertEqual(self.sc.call("subagents_status", {"run_id": q1})["status"], "queued")
            self.assertNotIn("child_pid", self.read_record(q1))
            self.sc.call("subagents_stop", {"run_id": busy[0]})
            self.wait_status(q1, "running")  # the oldest queued run takes the freed slot
            self.assertEqual(self.sc.call("subagents_status", {"run_id": q2})["status"], "queued")
        finally:
            for rid in busy + [q1, q2]:
                self.sc.call("subagents_stop", {"run_id": rid})

    def test_stop_cancels_a_queued_run_without_launching_it(self):
        env = dict(FAKE_MODE="slow", FAKE_SLEEP="20", GRAY_SUBAGENTS_TIMEOUT_SECS="60")
        busy = [self.spawn(task=f"busy{i}", env_extra=env)["run_id"] for i in range(2)]
        waiting = self.spawn(task="never", env_extra=env)["run_id"]
        try:
            self.assertEqual(self.sc.call("subagents_status", {"run_id": waiting})["status"], "queued")
            self.assertNotIn("is_error", self.sc.call("subagents_stop", {"run_id": waiting}))
            st = self.sc.call("subagents_status", {"run_id": waiting})
            self.assertEqual(st["status"], "stopped")
            self.assertIn("cancelled before start", st["error"])
            self.assertNotIn("child_pid", self.read_record(waiting))
        finally:
            for rid in busy:
                self.sc.call("subagents_stop", {"run_id": rid})

    def test_batch_items_accept_prompt_as_task(self):
        r = self.sc.call("subagent", {"tasks": [{"prompt": "alpha work"},
                                                {"prompt": "beta work", "agent": "oracle"}]})
        self.assertNotIn("is_error", r)
        prompts = [json.loads(self.wait_status(rid, "completed")["result"])["prompt"]
                   for rid in r["run_ids"]]
        self.assertTrue(any("alpha work" in p for p in prompts), prompts)
        self.assertTrue(any("beta work" in p for p in prompts), prompts)

    def test_batch_error_names_the_item_and_its_keys(self):
        bad = self.sc.call("subagent", {"tasks": [{"prompt": "fine"},
                                                  {"agent": "scout", "note": "no task here"}]})
        self.assertTrue(bad["is_error"])
        self.assertIn("tasks[1]", bad["content"])
        self.assertIn("item keys: agent, note", bad["content"])
        self.assertEqual(self.sc.call("subagents_status", {})["jobs"], [])

    def write_caches(self, listings=None, provider_cache=None, config=None):
        self.home.mkdir(parents=True, exist_ok=True)
        for name, data in (("provider_models.json", listings), ("config.json", config)):
            if data is not None:
                (self.home / name).write_text(json.dumps(data), encoding="utf-8")
        if provider_cache is not None:
            (self.home / "plugins").mkdir(exist_ok=True)
            (self.home / "plugins" / "provider-cache.json").write_text(
                json.dumps(provider_cache), encoding="utf-8")

    def run_model(self, model):
        """Model the child actually received for a run with this --model."""
        rid = self.sc.call("subagent", {"task": "go", "model": model})["run_id"]
        return json.loads(self.wait_status(rid, "completed")["result"])["model"]

    def test_family_alias_picks_newest_version_from_provider_cache(self):
        self.write_caches(listings={"plugin:claude:claude-subscription": {"fetched_at": 1, "rows": [
            {"id": "claude-haiku-4-5-20251001", "name": "Haiku 4.5"},
            {"id": "claude-haiku-5-5", "name": "Haiku 5.5"},
            {"id": "claude-haiku-5-5-medium", "name": "Haiku 5.5 medium"},
            {"id": "haiku", "name": "Haiku"}]}})
        self.assertEqual(self.run_model("haiku"), "claude-haiku-5-5")
        self.assertEqual(self.run_model("claude-haiku-4-5-20251001"), "claude-haiku-4-5-20251001")

    def test_exact_provider_model_id_resolves_to_its_qualified_spelling(self):
        self.write_caches(listings={
            "plugin:claude:claude-subscription": {"rows": [{"id": "claude-haiku-5-5"}]},
            "plugin:devin-sub:devin-subscription": {"rows": [{"id": "swe-2"}, {"id": "claude-haiku-5-5"}]}})
        self.assertEqual(self.run_model("devin-subscription/swe-2"), "devin-subscription/swe-2")
        self.assertEqual(self.run_model("Claude-Haiku-5-5"), "claude-haiku-5-5")

    def test_unbreakable_tie_is_an_error_listing_candidates(self):
        self.write_caches(listings={"plugin:a:alpha-sub": {"rows": [{"id": "vendor-a/widget-2"}]},
                                    "plugin:b:beta-sub": {"rows": [{"id": "vendor-b/widget-2"}]}})
        bad = self.sc.call("subagent", {"task": "go", "model": "widget"})
        self.assertTrue(bad["is_error"])
        self.assertIn("ambiguous", bad["content"])
        self.assertIn("vendor-a/widget-2", bad["content"])

    def test_configured_provider_breaks_an_otherwise_equal_tie(self):
        self.write_caches(listings={"plugin:a:alpha-sub": {"rows": [{"id": "vendor-a/widget-2"}]},
                                    "plugin:b:beta-sub": {"rows": [{"id": "vendor-b/widget-2"}]}},
                          config={"provider_id": "b:beta-sub"})
        self.assertEqual(self.run_model("widget"), "vendor-b/widget-2")

    def test_provider_cache_model_lists_join_the_catalog(self):
        self.write_caches(provider_cache={"schema": 1, "plugins": {"acme": {"enabled": True, "providers": [
            {"id": "acme-sub", "models": [{"id": "acme-9-1"}, "acme-9-0"]}]}}})
        self.assertEqual(self.run_model("acme-9"), "acme-9-1")

    # -- provider routing -----------------------------------------------------
    def write_provider_home(self):
        """lock.json + manifests + listings + config for a claude-parent home
        where devin-subscription owns the swe-2 family."""
        plugins = self.home / "plugins"
        plugins.mkdir(parents=True, exist_ok=True)
        (plugins / "lock.json").write_text(json.dumps({"schema": 1, "plugins": {
            "devin-sub": {"enabled": True}, "claude": {"enabled": True},
            "other-sub": {"enabled": True}, "dead-sub": {"enabled": False}}}))
        for key, pid, name, auth in (
                ("devin-sub", "devin-subscription", "Devin subscription", "devin-login"),
                ("claude", "claude-subscription", "Claude subscription", "claude-login"),
                ("other-sub", "other-subscription", "Other subscription", "other-login"),
                ("dead-sub", "dead-subscription", "Dead subscription", "dead-login")):
            (plugins / f"{key}-manifest.json").write_text(json.dumps({"providers": [{
                "id": pid, "name": name,
                "auth_methods": [{"id": auth, "kind": "api_key"}]}]}))
        self.write_caches(
            listings={
                "plugin:devin-sub:devin-subscription":
                    {"rows": [{"id": "swe-2"}, {"id": "swe-2-max"}, {"id": "duo-1"}]},
                "plugin:claude:claude-subscription": {"rows": [{"id": "claude-haiku-5-5"}]},
                "plugin:other-sub:other-subscription": {"rows": [{"id": "duo-1"}]},
                "plugin:dead-sub:dead-subscription": {"rows": [{"id": "ghost-1"}]}},
            config={"provider_id": "claude:claude-subscription",
                    "auth_ref": "plugin:claude:claude-subscription:claude-login",
                    "base_url": "https://127.0.0.1:1/"})

    def run_payload(self, model, env_extra=None):
        """The env fake-gray reports for a run spawned with this model spec."""
        rid = self.spawn(task="go", env_extra=env_extra,
                         args_extra={"model": model})["run_id"]
        return json.loads(self.wait_status(rid, "completed")["result"])

    def test_qualified_plugin_provider_routes_child_env(self):
        self.write_provider_home()
        r = self.spawn(task="go", args_extra={"model": "devin-subscription/swe-2"})
        self.assertIn("→ Devin subscription · swe-2", r["content"])
        payload = json.loads(self.wait_status(r["run_id"], "completed")["result"])
        self.assertEqual(payload["model"], "swe-2")
        self.assertEqual(payload["provider"], "devin-sub:devin-subscription")
        self.assertEqual(payload["auth"],
                         "plugin:devin-sub:devin-subscription:devin-login")

    def test_provider_alias_and_named_auth_route(self):
        self.write_provider_home()
        payload = self.run_payload("devin/swe-2-max")
        self.assertEqual(payload["model"], "swe-2-max")
        self.assertEqual(payload["provider"], "devin-sub:devin-subscription")
        self.assertEqual(payload["auth"],
                         "plugin:devin-sub:devin-subscription:devin-login")
        payload = self.run_payload("devin-sub:devin-subscription:devin-login/swe-2")
        self.assertEqual(payload["model"], "swe-2")
        self.assertEqual(payload["auth"],
                         "plugin:devin-sub:devin-subscription:devin-login")

    def test_bare_id_unique_to_one_plugin_provider_routes(self):
        self.write_provider_home()
        payload = self.run_payload("swe-2")
        self.assertEqual(payload["model"], "swe-2")
        self.assertEqual(payload["provider"], "devin-sub:devin-subscription")
        self.assertEqual(payload["auth"],
                         "plugin:devin-sub:devin-subscription:devin-login")

    def test_same_provider_model_runs_without_override(self):
        self.write_provider_home()
        payload = self.run_payload("claude-haiku-5-5")
        self.assertEqual(payload["model"], "claude-haiku-5-5")
        self.assertIsNone(payload["provider"])
        self.assertIsNone(payload["auth"])

    def test_bare_id_ambiguous_across_providers_errors(self):
        self.write_provider_home()
        r = self.sc.call("subagent", {"task": "go", "model": "duo-1"})
        self.assertTrue(r["is_error"])
        self.assertIn("ambiguous across providers", r["content"])
        self.assertIn("devin-subscription/duo-1", r["content"])
        self.assertIn("other-subscription/duo-1", r["content"])

    def test_disabled_provider_cannot_be_a_route(self):
        self.write_provider_home()
        payload = self.run_payload("ghost-1")
        self.assertEqual(payload["model"], "ghost-1")
        self.assertIsNone(payload["provider"])

    def test_routed_run_clears_inherited_base_url(self):
        self.write_provider_home()
        payload = self.run_payload("devin-subscription/swe-2",
                                   env_extra={"GRAY_BASE_URL": "https://stale.example/"})
        self.assertIsNone(payload["base_url"])
        self.assertEqual(payload["provider"], "devin-sub:devin-subscription")
        payload = self.run_payload("claude-haiku-5-5",
                                   env_extra={"GRAY_BASE_URL": "https://stale.example/"})
        self.assertEqual(payload["base_url"], "https://stale.example/")

    # -- context ceiling (GRAY_COMPACT_AT) ---------------------------------

    def write_ctx_home(self):
        """A catalog where a Claude id has a [1m] sibling route."""
        self.home.mkdir(parents=True, exist_ok=True)
        (self.home / "models.json").write_text(json.dumps({"gpt-4o": {}}))
        (self.home / "provider_models.json").write_text(json.dumps({
            "https://api.example.test/v1": {"rows": [
                "claude-haiku-5-5", "claude-haiku-5-5[1m]", "gpt-4o"]}}))

    def ctx_payload(self, r):
        st = self.wait_status(r["run_id"], "completed")
        return json.loads(st["result"])

    def test_context_param_reaches_child_env(self):
        r = self.spawn(args_extra={"context": "90k"})
        self.assertEqual(self.ctx_payload(r)["compact_at"], "90k")

    def test_context_env_is_the_settings_tier(self):
        # The settings write path exports GRAY_COMPACT_AT on the supervisor
        # env: a spawn without the knob inherits it; an explicit value wins.
        r = self.spawn(env_extra={"GRAY_COMPACT_AT": "300k"})
        self.assertEqual(self.ctx_payload(r)["compact_at"], "300k")
        r = self.spawn(env_extra={"GRAY_COMPACT_AT": "300k"},
                       args_extra={"context": "auto"})
        self.assertEqual(self.ctx_payload(r)["compact_at"], "auto")
        r = self.spawn()
        self.assertIsNone(self.ctx_payload(r)["compact_at"])

    def test_context_profile_beats_env_and_param_beats_profile(self):
        agents = self.home / "subagents" / "agents"
        agents.mkdir(parents=True, exist_ok=True)
        (agents / "ctxy.md").write_text("---\ncontext: 500k\n---\n# Ctx\n\nDo it.\n")
        r = self.spawn(agent="ctxy", env_extra={"GRAY_COMPACT_AT": "300k"})
        self.assertEqual(self.ctx_payload(r)["compact_at"], "500k")
        r = self.spawn(agent="ctxy", env_extra={"GRAY_COMPACT_AT": "300k"},
                       args_extra={"context": "90k"})
        self.assertEqual(self.ctx_payload(r)["compact_at"], "90k")

    def test_context_above_200k_on_claude_takes_the_1m_route(self):
        self.write_ctx_home()
        r = self.spawn(args_extra={"model": "haiku", "context": "300k"})
        self.assertEqual(self.ctx_payload(r)["model"], "claude-haiku-5-5[1m]")
        # Under the threshold and on non-Claude ids the model stays put.
        r = self.spawn(args_extra={"model": "haiku", "context": "90k"})
        self.assertEqual(self.ctx_payload(r)["model"], "claude-haiku-5-5")
        r = self.spawn(args_extra={"model": "gpt-4o", "context": "300k"})
        self.assertEqual(self.ctx_payload(r)["model"], "gpt-4o")

    def test_context_rejects_junk(self):
        r = self.spawn(args_extra={"context": "lots"})
        self.assertTrue(r["is_error"])
        self.assertIn("context", r["content"])

    def test_unknown_auth_method_errors(self):
        self.write_provider_home()
        r = self.sc.call("subagent", {"task": "go", "model": "devin-subscription:bogus/swe-2"})
        self.assertTrue(r["is_error"])
        self.assertIn("unknown auth method 'bogus'", r["content"])

    # -- status --------------------------------------------------------------
    def test_status_lists_all_jobs_most_recent_first(self):
        a = self.sc.call("subagent", {"agent": "scout", "task": "alpha"})["run_id"]
        time.sleep(0.05)
        b = self.sc.call("subagent", {"agent": "scout", "task": "beta"})["run_id"]
        jobs = self.sc.call("subagents_status", {})["jobs"]
        self.assertEqual([j["run_id"] for j in jobs[:2]], [b, a])

    def test_status_unknown_run_id_is_error(self):
        r = self.sc.call("subagents_status", {"run_id": "deadbeef"})
        self.assertTrue(r.get("is_error"))

    def test_runs_show_only_in_their_owner_session(self):
        # Full stack through the real sidecar: a run admitted by session A is
        # listed and notified for A alone; B sees it only with all=true.
        def tool(name, args, session, rid):
            reply = self.sc.rpc("tool/call", {"name": name, "args": args, "session": session}, rid)
            self.assertIn("result", reply, reply)
            return reply["result"]
        cwd = os.getcwd()
        a, b = {"id": "sess-a", "cwd": cwd}, {"id": "sess-b", "cwd": cwd}
        rid = tool("subagent", {"agent": "scout", "task": "owned by a"}, a, 10)["run_id"]
        for _ in range(100):
            if tool("subagents_status", {"run_id": rid}, a, 11)["status"] == "completed":
                break
            time.sleep(0.1)
        # The finished report lands in A's cron inbox only; B's never exists.
        inbox_a = self.home / "cron" / "inbox" / "sess-a"
        for _ in range(100):
            if inbox_a.is_dir() and any(inbox_a.glob("*.json")):
                break
            time.sleep(0.1)
        self.assertTrue(any(inbox_a.glob("*.json")))
        self.assertIn(rid, "".join(p.read_text() for p in inbox_a.glob("*.json")))
        self.assertFalse((self.home / "cron" / "inbox" / "sess-b").exists())
        ids = lambda reply: [j["run_id"] for j in reply["jobs"]]
        self.assertIn(rid, ids(tool("subagents_status", {}, a, 12)))
        self.assertNotIn(rid, ids(tool("subagents_status", {}, b, 13)))
        self.assertIn(rid, ids(tool("subagents_status", {"all": True}, b, 14)))

    def test_spawn_without_wire_session_is_owned_by_exported_env(self):
        # No wire session id: the run is owned by the exported GRAY_SESSION_ID,
        # never recorded session-less (which would show to every folder peer).
        self.spawn(env_extra={"GRAY_SESSION_ID": "sess-env"})
        runs = self.home / "subagents" / "runs"
        owners = [json.loads(p.read_text()).get("session_id") for p in runs.glob("*.json")]
        self.assertEqual(owners, ["sess-env"])

    # -- stop ----------------------------------------------------------------
    def test_stop_kills_running_job(self):
        env = dict(FAKE_MODE="slow", FAKE_SLEEP="30")
        rid = self.spawn(task="long", env_extra=env)["run_id"]
        self.assertIn(self.sc.call("subagents_stop", {"run_id": rid})["status"], ("stopping", "stopped"))
        st = self.wait_status(rid, "stopped")
        r2 = self.sc.call("subagents_stop", {"run_id": rid})
        self.assertTrue(r2.get("is_error"))

    def test_run_id_prefix_and_last_resolution(self):
        rid = self.sc.call("subagent", {"agent": "scout", "task": "short me"})["run_id"]
        self.wait_status(rid, "completed")
        st = self.sc.call("subagents_status", {"run_id": rid[:8]})
        self.assertEqual(st["run_id"], rid)
        st = self.sc.call("subagents_status", {"run_id": "last"})
        self.assertEqual(st["run_id"], rid)
        r = self.sc.call("subagents_stop", {"run_id": "abc"})
        self.assertTrue(r.get("is_error"))

    def test_run_names_generated_and_resolvable(self):
        r = self.sc.call("subagent", {"agent": "scout", "task": "named work"})
        rid, name = r["run_id"], r["names"][0]
        self.assertRegex(name, r"^[a-z]+-[a-z]+(-\d+)?$")
        self.wait_status(rid, "completed")
        st = self.sc.call("subagents_status", {"run_id": name})
        self.assertEqual(st["run_id"], rid)
        self.assertEqual(st["name"], name)
        listing = self.sc.call("subagents_status", {})["content"]
        self.assertIn(name, listing)
        r = self.sc.call("subagents_steer", {"run_id": name, "message": "again by name"})
        self.assertNotIn("is_error", r)
        self.assertIn(name, r["content"])
        self.wait_status(rid, "completed")

    def test_custom_name_and_reserved_names(self):
        r = self.sc.call("subagent", {"agent": "scout", "task": "t", "name": "bobby"})
        rid = r["run_id"]
        self.assertEqual(r["names"], ["bobby"])
        self.wait_status(rid, "completed")
        st = self.sc.call("subagents_status", {"run_id": "bobby"})
        self.assertEqual(st["run_id"], rid)
        for bad in ("last", "a" * 32, "has spaces", "", "-lead"):
            r = self.sc.call("subagent", {"task": "x", "name": bad})
            self.assertTrue(r.get("is_error"), bad)

    def test_duplicate_name_resolves_to_newest(self):
        a = self.sc.call("subagent", {"task": "one", "name": "dup"})["run_id"]
        self.wait_status(a, "completed")
        b = self.sc.call("subagent", {"task": "two", "name": "dup"})["run_id"]
        self.wait_status(b, "completed")
        st = self.sc.call("subagents_status", {"run_id": "dup"})
        self.assertEqual(st["run_id"], b)

    def test_stop_unknown_run_id_is_error(self):
        r = self.sc.call("subagents_stop", {"run_id": "deadbeef"})
        self.assertTrue(r.get("is_error"))

    # -- completion notices via prompt/context --------------------------------
    def test_prompt_context_reports_finished_jobs_once(self):
        rid = self.sc.call("subagent", {"agent": "scout", "task": "report"})["run_id"]
        self.wait_status(rid, "completed")
        c1 = self.sc.rpc("prompt/context", {"cwd": os.getcwd()}, rid=5)["result"]["text"]
        self.assertIn(rid, c1)
        c2 = self.sc.rpc("prompt/context", {"cwd": os.getcwd()}, rid=6)["result"]["text"]
        self.assertNotIn(rid, c2)

    def test_prompt_context_quiet_when_no_jobs(self):
        r = self.sc.rpc("prompt/context", {"cwd": os.getcwd()}, rid=5)["result"]
        self.assertNotIn("Subagent results", r["text"])
        self.assertIn("Profiles:", r["text"])

    # -- crash recovery -------------------------------------------------------
    def test_orphaned_run_is_marked_lost_on_next_status(self):
        env = dict(FAKE_MODE="slow", FAKE_SLEEP="30")
        rid = self.spawn(task="vanish", env_extra=env)["run_id"]
        job_path = self.run_dir / f"{rid}.json"
        job = json.loads(job_path.read_text())
        os.kill(job["supervisor_pid"], signal.SIGKILL)
        self.wait_status(rid, "lost")

    # -- profiles -------------------------------------------------------------
    def test_setup_creates_builtins_and_preserves_edits(self):
        agents = self.home / "subagents" / "agents"
        out1 = self.sc.rpc("command/run", {"name": "/subagents", "argv": ["setup"]}, rid=1)
        self.assertNotIn("is_error", out1["result"])
        self.assertTrue((agents / "scout.md").exists())
        (agents / "scout.md").write_text("CUSTOM\n")
        self.sc.rpc("command/run", {"name": "/subagents", "argv": ["setup"]}, rid=2)
        self.assertEqual((agents / "scout.md").read_text(), "CUSTOM\n")

    def test_list_reflects_user_profiles(self):
        self.sc.rpc("command/run", {"name": "/subagents", "argv": ["setup"]}, rid=1)
        agents = self.home / "subagents" / "agents"
        agents.mkdir(parents=True, exist_ok=True)
        (agents / "auditor.md").write_text("# Auditor\n\nCheck things.\n")
        out = self.sc.call("subagents_list", {})["content"]
        self.assertIn("scout", out)
        self.assertIn("auditor", out)

    def test_timeout_reaps_stuck_child(self):
        env = dict(FAKE_MODE="slow", FAKE_SLEEP="30", GRAY_SUBAGENTS_TIMEOUT_SECS="1")
        rid = self.spawn(task="hang", env_extra=env)["run_id"]
        st = self.wait_status(rid, "timeout", tries=200)
        self.assertIn("timed out", st["error"])

    def test_background_job_finishes_after_parent_exits(self):
        runner = Sidecar(dict(self.env, FAKE_MODE="slow", FAKE_SLEEP="0.5"))
        self.addCleanup(runner.close)
        rid = runner.call("subagent", {"task": "survive"})["run_id"]
        runner.close()
        self.wait_status(rid, "completed")

    def test_nonzero_exit_is_failed_not_success_from_output(self):
        rid = self.spawn(env_extra={"FAKE_MODE": "fail"})["run_id"]
        st = self.wait_status(rid, "failed")
        self.assertIn("7", st["error"])
        self.assertIn("deliberate child failure", st["result"])

    def test_empty_success_is_failed(self):
        rid = self.spawn(env_extra={"FAKE_MODE": "empty"})["run_id"]
        self.wait_status(rid, "failed")

    def test_output_limit_stops_run(self):
        rid = self.spawn(env_extra={"FAKE_MODE": "flood"})["run_id"]
        st = self.wait_status(rid, "failed")
        self.assertIn("output limit", st["error"])
        self.assertLessEqual(len(st["result"].encode()), 262144)

    def test_output_volume_does_not_kill_run(self):
        rid = self.spawn(env_extra={"FAKE_MODE": "verbose"})["run_id"]
        st = self.wait_status(rid, "completed")
        self.assertTrue(st["result"].strip())

    def test_parallel_batch_and_no_partial_launch_on_invalid_task(self):
        bad = self.sc.call("subagent", {"tasks": [{"task": "first"}, {"agent": "missing", "task": "second"}]})
        self.assertTrue(bad["is_error"])
        self.assertEqual(self.sc.call("subagents_status", {})["jobs"], [])
        result = self.sc.call("subagent", {"tasks": [{"task": "alpha"}, {"agent": "oracle", "task": "beta"}]})
        self.assertEqual(len(set(result["run_ids"])), 2)
        for rid in result["run_ids"]:
            self.wait_status(rid, "completed")

    def test_cwd_from_host_session_is_used(self):
        reply = self.sc.rpc("tool/call", {"name": "subagent", "args": {"task": "where"},
                            "session": {"id": "test", "cwd": self.tmp.name}})["result"]
        st = self.wait_status(reply["run_id"], "completed")
        self.assertEqual(json.loads(st["result"])["cwd"], self.tmp.name)

    def test_child_context_does_not_consume_parent_notices(self):
        rid = self.sc.call("subagent", {"task": "parent result"})["run_id"]
        self.wait_status(rid, "completed")
        child = Sidecar(dict(self.env, GRAY_SUBAGENTS_ACTIVE="1"))
        self.addCleanup(child.close)
        self.assertEqual(child.rpc("prompt/context", {})["result"]["text"], "")
        self.assertIn(rid, self.sc.rpc("prompt/context", {})["result"]["text"])

    def test_invalid_arguments_are_tool_errors_not_protocol_errors(self):
        for args in ([], {"task": 4}, {"task": "ok", "agent": "../escape"},
                     {"tasks": []}, {"tasks": [{"task": "a"}], "task": "b"}):
            with self.subTest(args=args):
                self.assertTrue(self.sc.call("subagent", args)["is_error"])

    def test_timeout_kills_descendants(self):
        rid = self.spawn(env_extra={"FAKE_MODE": "descendant", "GRAY_SUBAGENTS_TIMEOUT_SECS": "1"})["run_id"]
        self.wait_status(rid, "timeout")
        pid = int(pathlib.Path(self.env["FAKE_LOG"]).read_text())
        stat = pathlib.Path(f"/proc/{pid}/stat")
        for _ in range(100):
            if not stat.exists() or stat.read_text().rsplit(") ", 1)[1].split()[0] == "Z":
                return
            time.sleep(.02)
        self.fail("descendant still running")

    # -- steering -------------------------------------------------------------
    def test_steer_after_completion_resumes_child_session(self):
        rid = self.sc.call("subagent", {"agent": "scout", "task": "first"})["run_id"]
        self.wait_status(rid, "completed")
        r = self.sc.call("subagents_steer", {"run_id": rid, "message": "now do this"})
        self.assertNotIn("is_error", r)
        st = self.wait_status(rid, "completed")
        payload = json.loads(st["result"])
        self.assertEqual(payload["resume"], "fake-child-session")
        self.assertIn("now do this", payload["prompt"])
        self.assertEqual(st["phases"], 2)
        self.assertEqual(st["child_session"], "fake-child-session")

    def test_steer_while_running_queues_for_next_phase(self):
        rid = self.spawn(task="slow first", env_extra={
            "FAKE_MODE": "slow", "FAKE_SLEEP": "1"})["run_id"]
        r = self.sc.call("subagents_steer", {"run_id": rid, "message": "queued follow-up"})
        self.assertIn("queued", r["content"])
        st = self.wait_status(rid, "completed")
        self.assertEqual(st["phases"], 2)
        self.assertEqual(json.loads(st["result"])["resume"], "fake-child-session")

    def test_steer_rejects_unresumable_and_bad_input(self):
        rid = self.spawn(env_extra={"FAKE_MODE": "fail"})["run_id"]
        self.wait_status(rid, "failed")
        r = self.sc.call("subagents_steer", {"run_id": rid, "message": "again"})
        self.assertTrue(r.get("is_error"))
        self.assertIn("session", r["content"])
        for bad in ({"run_id": "deadbeef", "message": "x"},
                    {"run_id": rid, "message": "   "}):
            r = self.sc.call("subagents_steer", bad)
            self.assertTrue(r.get("is_error"), bad)

    def test_live_activity_is_recorded_while_running(self):
        rid = self.spawn(task="watch me", env_extra={
            "FAKE_MODE": "chatty", "FAKE_SLEEP": "1.5"})["run_id"]
        try:
            seen = ""
            for _ in range(100):
                st = self.sc.call("subagents_status", {"run_id": rid})
                if st.get("activity"):
                    seen = st["activity"]
                    break
                time.sleep(.05)
            self.assertIn("Bash", seen)
        finally:
            self.wait_status(rid, "completed")

    def test_steer_before_any_session_rejects(self):
        rid = self.spawn(task="wander", env_extra={
            "FAKE_MODE": "slow", "FAKE_SLEEP": "10"})["run_id"]
        self.sc.call("subagents_stop", {"run_id": rid})
        self.wait_status(rid, "stopped")
        r = self.sc.call("subagents_steer", {"run_id": rid, "message": "redirect"})
        self.assertTrue(r.get("is_error"))
        self.assertIn("session", r["content"])

    def test_stop_mid_steer_then_steer_redirects(self):
        rid = self.spawn(task="leg one", env_extra={
            "FAKE_MODE": "slow", "FAKE_SLEEP": "2"})["run_id"]
        self.wait_status(rid, "completed")
        self.sc.call("subagents_steer", {"run_id": rid, "message": "leg two"})
        # phase 2 sleeps ~2s — stop lands inside it
        self.sc.call("subagents_stop", {"run_id": rid})
        self.wait_status(rid, "stopped")
        r = self.sc.call("subagents_steer", {"run_id": rid, "message": "leg three"})
        self.assertNotIn("is_error", r)
        st = self.wait_status(rid, "completed")
        payload = json.loads(st["result"])
        self.assertEqual(payload["resume"], "fake-child-session")
        self.assertIn("leg three", payload["prompt"])

    # -- budgets, stall guard, audit trail ------------------------------------
    def test_max_phases_budget_ends_revived_run(self):
        rid = self.sc.call("subagent", {"task": "one turn", "max_phases": 1})["run_id"]
        self.wait_status(rid, "completed")
        self.sc.call("subagents_steer", {"run_id": rid, "message": "again"})
        st = self.wait_status(rid, "failed")
        self.assertIn("budget", st["error"])

    def test_stalled_after_two_phases_without_tool_calls(self):
        # The steered supervisor inherits the steering sidecar's env — the
        # FAKE_MODE must ride on it so the revived phase stays tool-less.
        sc = Sidecar(dict(self.env, FAKE_MODE="quiet"))
        self.addCleanup(sc.close)
        rid = sc.call("subagent", {"agent": "scout", "task": "do work"})["run_id"]
        self.wait_status(rid, "completed")
        sc.call("subagents_steer", {"run_id": rid, "message": "continue"})
        st = self.wait_status(rid, "stalled")
        self.assertIn("stalled", st["error"])

    def test_same_error_three_times_marks_stalled(self):
        sc = Sidecar(dict(self.env, FAKE_MODE="sessfail"))
        self.addCleanup(sc.close)
        rid = sc.call("subagent", {"agent": "scout", "task": "do work"})["run_id"]
        self.wait_status(rid, "failed")
        sc.call("subagents_steer", {"run_id": rid, "message": "retry"})
        self.wait_status(rid, "failed")
        sc.call("subagents_steer", {"run_id": rid, "message": "retry"})
        self.wait_status(rid, "stalled")

    def test_events_log_records_transitions_without_content(self):
        rid = self.sc.call("subagent", {"task": "audit-marker-xyzzy"})["run_id"]
        self.wait_status(rid, "completed")
        lines = (self.run_dir / f"{rid}.events.jsonl").read_text().splitlines()
        events = [json.loads(line) for line in lines]
        self.assertEqual([e["ev"] for e in events],
                         ["enqueued", "started", "phase_started", "phase_ended", "completed"])
        self.assertTrue(all(isinstance(e.get("ts"), (int, float)) for e in events))
        self.assertNotIn("audit-marker-xyzzy", "\n".join(lines))

    # -- model / effort delegation -------------------------------------------
    def test_per_run_model_and_effort_reach_child_env(self):
        rid = self.sc.call("subagent", {"task": "work", "model": "vendor/model-x",
                                        "effort": "low"})["run_id"]
        st = self.wait_status(rid, "completed")
        payload = json.loads(st["result"])
        self.assertEqual(payload["model"], "vendor/model-x")
        self.assertEqual(payload["effort"], "low")
        self.assertEqual(st["model"], "vendor/model-x")
        self.assertEqual(st["effort"], "low")

    def test_batch_tasks_carry_individual_models(self):
        r = self.sc.call("subagent", {"tasks": [
            {"task": "a", "model": "vendor/a"},
            {"task": "b", "model": "vendor/b", "effort": "high"}]})
        self.assertNotIn("is_error", r)
        got = {}
        for rid in r["run_ids"]:
            st = self.wait_status(rid, "completed")
            got[json.loads(st["result"])["model"]] = json.loads(st["result"])["effort"]
        self.assertEqual(got, {"vendor/a": None, "vendor/b": "high"})

    def test_profile_frontmatter_sets_model_default(self):
        agents = self.home / "subagents" / "agents"
        agents.mkdir(parents=True)
        (agents / "cheap.md").write_text(
            "---\nmodel: vendor/cheap-model\neffort: minimal\n---\n\n# Cheap\n\nDo it fast.\n")
        rid = self.sc.call("subagent", {"agent": "cheap", "task": "go"})["run_id"]
        st = self.wait_status(rid, "completed")
        payload = json.loads(st["result"])
        self.assertEqual(payload["model"], "vendor/cheap-model")
        self.assertEqual(payload["effort"], "minimal")
        self.assertNotIn("vendor/cheap-model", payload["prompt"])

    def test_run_arg_model_beats_profile_default(self):
        agents = self.home / "subagents" / "agents"
        agents.mkdir(parents=True)
        (agents / "cheap.md").write_text("---\nmodel: vendor/cheap\n---\n# Cheap\n\nDo it.\n")
        rid = self.sc.call("subagent", {"agent": "cheap", "task": "go",
                                        "model": "vendor/strong"})["run_id"]
        st = self.wait_status(rid, "completed")
        self.assertEqual(json.loads(st["result"])["model"], "vendor/strong")

    def test_invalid_model_and_effort_are_tool_errors(self):
        for args in ({"task": "x", "model": ""}, {"task": "x", "effort": "ludicrous"},
                     {"task": "x", "model": 42}, {"task": "x", "model": "a" * 300}):
            with self.subTest(args=args):
                self.assertTrue(self.sc.call("subagent", args)["is_error"])

    def test_model_catalog_resolves_and_suggests(self):
        self.home.mkdir(parents=True)
        (self.home / "models.json").write_text(json.dumps(
            {"vendor/good-model": {}, "vendor/other-model": {}, "acme/good-tool": {}}))
        rid = self.sc.call("subagent", {"task": "go", "model": "good-model"})["run_id"]
        st = self.wait_status(rid, "completed")
        self.assertEqual(json.loads(st["result"])["model"], "vendor/good-model")
        bad = self.sc.call("subagent", {"task": "go", "model": "good"})
        self.assertTrue(bad["is_error"])
        self.assertIn("vendor/good-model", bad["content"])
        miss = self.sc.call("subagent", {"task": "go", "model": "nope-zzz"})
        self.assertTrue(miss["is_error"])
        self.assertIn("nope-zzz", miss["content"])

    def test_steer_phase_keeps_job_model(self):
        rid = self.sc.call("subagent", {"task": "leg one", "model": "vendor/leg",
                                        "effort": "low"})["run_id"]
        self.wait_status(rid, "completed")
        self.sc.call("subagents_steer", {"run_id": rid, "message": "leg two"})
        st = self.wait_status(rid, "completed")
        payload = json.loads(st["result"])
        self.assertEqual(payload["model"], "vendor/leg")
        self.assertEqual(payload["effort"], "low")

    # -- timeout policy -------------------------------------------------------
    def test_no_timeout_by_default(self):
        env = dict(self.env)
        env.pop("GRAY_SUBAGENTS_TIMEOUT_SECS", None)
        env.update(FAKE_MODE="slow", FAKE_SLEEP="0.5")
        runner = Sidecar(env)
        self.addCleanup(runner.close)
        rid = runner.call("subagent", {"agent": "scout", "task": "uncapped"})["run_id"]
        job = json.loads((self.run_dir / f"{rid}.json").read_text())
        self.assertIsNone(job["timeout"])
        self.wait_status(rid, "completed")

    def test_explicit_timeout_arg_is_enforced(self):
        rid = self.spawn(task="hang", env_extra={"FAKE_MODE": "slow", "FAKE_SLEEP": "30"},
                         args_extra={"timeout": 1})["run_id"]
        st = self.wait_status(rid, "timeout", tries=200)
        self.assertIn("timed out", st["error"])

    def test_invalid_timeout_arg_is_tool_error(self):
        for bad in (0, -1, 86401, "soon", True):
            with self.subTest(timeout=bad):
                r = self.spawn(task="x", args_extra={"timeout": bad})
                self.assertTrue(r.get("is_error"))
                self.assertIn("timeout", r["content"])

    # -- worktree cleanup ------------------------------------------------------
    def test_timed_out_run_keeps_clean_worktree(self):
        repo = self.git_repo()
        # chatty reports its session id before hanging, so the run is resumable.
        rid = self.spawn(task="hang", env_extra={"FAKE_MODE": "chatty", "FAKE_SLEEP": "30"},
                         args_extra={"isolation": "worktree", "timeout": 1},
                         cwd=repo)["run_id"]
        st = self.wait_status(rid, "timeout", tries=200)
        self.assertTrue(self.worktree_path(rid).is_dir())
        self.assertIn("worktree kept", st["content"])
        self.assertIn("steer", st["content"])

    def test_failed_run_collects_empty_worktree(self):
        repo = self.git_repo()
        # "fail" leaves no child session and nothing lands on the branch —
        # nothing to inspect, so the tree and its empty ref are reclaimed.
        rid = self.spawn(task="boom", env_extra={"FAKE_MODE": "fail"},
                         args_extra={"isolation": "worktree"}, cwd=repo)["run_id"]
        st = self.wait_status(rid, "failed")
        self.assertFalse(self.worktree_path(rid).exists())
        self.assertNotIn("worktree kept", st["content"])
        branches = subprocess.run(["git", "-C", str(repo), "branch", "--list", "sub/*"],
                                  capture_output=True, text=True).stdout
        self.assertEqual(branches.strip(), "")

    def test_timed_out_run_keeps_dirty_worktree(self):
        repo = self.git_repo()
        # slow sleeps through its timeout window — the run ends unresumable,
        # but uncommitted changes in the tree are the thing to inspect.
        rid = self.spawn(task="hang", env_extra={"FAKE_MODE": "slow", "FAKE_SLEEP": "30"},
                         args_extra={"isolation": "worktree", "timeout": 1},
                         cwd=repo)["run_id"]
        (self.worktree_path(rid) / "leftover.txt").write_text("partial\n")
        st = self.wait_status(rid, "timeout", tries=200)
        self.assertTrue((self.worktree_path(rid) / "leftover.txt").is_file())
        self.assertIn("worktree kept", st["content"])

    def test_merged_branch_sweeps_kept_worktree(self):
        repo = self.git_repo()
        # slow never prints a session before the timeout kills it — terminal
        # and unresumable — but a commit lands on its branch during the run.
        rid = self.spawn(task="hang", env_extra={"FAKE_MODE": "slow", "FAKE_SLEEP": "30"},
                         args_extra={"isolation": "worktree", "timeout": 1},
                         cwd=repo)["run_id"]
        wt = self.worktree_path(rid)
        branch = subprocess.run(["git", "-C", str(wt), "rev-parse", "--abbrev-ref", "HEAD"],
                                capture_output=True, text=True).stdout.strip()
        self.assertTrue(branch.startswith("sub/"))
        (wt / "work.txt").write_text("kept\n")
        for argv in (["git", "add", "work.txt"], ["git", "commit", "-qm", "sub work"]):
            subprocess.run(argv, cwd=wt, check=True, capture_output=True)
        st = self.wait_status(rid, "timeout", tries=200)
        self.assertTrue(wt.is_dir())
        self.assertIn("unmerged", st["content"])
        # Once the work merges, the next sweep reclaims both tree and ref.
        subprocess.run(["git", "-C", str(repo), "merge", "--no-ff", "-qm", "merge", branch],
                       check=True, capture_output=True)
        stamp = self.home / "subagents" / "worktrees" / ".gc-stamp"
        stamp.unlink(missing_ok=True)
        self.sc.call("subagents_status", {})
        self.assertFalse(wt.exists())
        branches = subprocess.run(["git", "-C", str(repo), "branch", "--list", "sub/*"],
                                  capture_output=True, text=True).stdout
        self.assertEqual(branches.strip(), "")

    def test_orphan_worktree_dir_is_collected(self):
        repo = self.git_repo()
        # No run record points here: a registered worktree on a collectible
        # sub/* branch is litter — while a plain look-alike dir is not ours.
        orphan = self.home / "subagents" / "worktrees" / "deadbeef-orphan"
        orphan.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "-C", str(repo), "worktree", "add", str(orphan),
                        "-b", "sub/stale-orphan"], check=True, capture_output=True)
        # Backdate past the admission grace — a run that crashed mid-spawn
        # leaves a dir this old, not a fresh one.
        old_mtime = time.time() - 300
        os.utime(orphan, (old_mtime, old_mtime))
        stranger = self.home / "subagents" / "worktrees" / "gray-acp-bridge"
        stranger.mkdir(exist_ok=True)
        stamp = self.home / "subagents" / "worktrees" / ".gc-stamp"
        stamp.unlink(missing_ok=True)
        self.sc.call("subagents_status", {})
        self.assertFalse(orphan.exists())
        self.assertTrue(stranger.is_dir())
        branches = subprocess.run(["git", "-C", str(repo), "branch", "--list", "sub/stale-orphan"],
                                  capture_output=True, text=True).stdout
        self.assertEqual(branches.strip(), "")

    def test_successful_run_removes_clean_worktree(self):
        repo = self.git_repo()
        rid = self.spawn(task="clean run", args_extra={"isolation": "worktree"},
                         cwd=repo)["run_id"]
        st = self.wait_status(rid, "completed")
        self.assertFalse(self.worktree_path(rid).exists())
        self.assertNotIn("worktree kept", st["content"])
        branches = subprocess.run(["git", "-C", str(repo), "branch", "--list", "sub/*"],
                                  capture_output=True, text=True).stdout
        self.assertEqual(branches.strip(), "")

class ResolverTable(unittest.TestCase):
    """Model resolution over a fixture cache. The Rust twin (src/model_catalog.rs)
    resolves the same files to the same answers; keep the two tables in step."""

    FIXTURE_MODELS = {"claude-haiku-4-5-20251001": {}, "claude-sonnet-9-9": {}, "gpt-4o": {}}
    FIXTURE_LISTINGS = {
        "plugin:claude:claude-subscription": {"fetched_at": 1, "rows": [
            {"id": "claude-haiku-4-5"}, {"id": "claude-haiku-5-5"},
            "claude-sonnet-4-6", "claude-opus-4-6-thinking", "claude-opus-4-6-fast"]},
        "https://api.example.test/v1": {"rows": [["claude-sonnet-5-5", "label"]]},
    }
    FIXTURE_CACHE = {"plugins": {"opencode-sub": {"providers": [
        {"id": "opencode-zen", "models": ["zen-x-1"]}]}}}
    FIXTURE_CONFIG = {"provider_id": "claude:claude-subscription"}

    @classmethod
    def setUpClass(cls):
        loader = importlib.machinery.SourceFileLoader("gray_subagents_mod", str(REPO / "mod"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        cls.mod = importlib.util.module_from_spec(spec)
        loader.exec_module(cls.mod)

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        home = pathlib.Path(self.tmp.name)
        (home / "models.json").write_text(json.dumps(self.FIXTURE_MODELS))
        (home / "provider_models.json").write_text(json.dumps(self.FIXTURE_LISTINGS))
        (home / "plugins").mkdir()
        (home / "plugins" / "provider-cache.json").write_text(json.dumps(self.FIXTURE_CACHE))
        (home / "config.json").write_text(json.dumps(self.FIXTURE_CONFIG))
        patch = mock.patch.dict(os.environ, {"GRAY_HOME": str(home)})
        patch.start()
        self.addCleanup(patch.stop)

    def test_shared_table(self):
        # claude-sonnet-9-9 is only in models.json (the stale catalog): never picked.
        table = [
            ("haiku", "claude-haiku-5-5"),
            ("Claude-Haiku-5-5", "claude-haiku-5-5"),
            ("claude-subscription/claude-haiku-5-5", "claude-subscription/claude-haiku-5-5"),
            ("sonnet", "claude-sonnet-5-5"),
            ("claude-s", "claude-sonnet-5-5"),
            ("zen", "zen-x-1"),
            ("gpt", "gpt-4o"),
            ("haiku-5", "claude-haiku-5-5"),
            ("5-5", "claude-haiku-5-5"),
            ("mystery/model", "mystery/model"),
        ]
        for spec, want in table:
            with self.subTest(spec=spec):
                self.assertEqual(self.mod.resolve_model(spec), want)

    def test_errors_name_the_newest_candidates(self):
        with self.assertRaises(ValueError) as opus:
            self.mod.resolve_model("opus")
        self.assertIn("ambiguous", str(opus.exception))
        self.assertIn("claude-opus-4-6-thinking, claude-opus-4-6-fast", str(opus.exception))
        with self.assertRaises(ValueError) as nope:
            self.mod.resolve_model("nope")
        self.assertEqual(str(nope.exception), "unknown model 'nope' (no catalog match)")

    def test_version_parse_matches_the_rust_twin(self):
        version_of = self.mod.version_of
        self.assertEqual(version_of("claude-haiku-4-5-20251001"), (4, 5))
        self.assertEqual(version_of("claude-haiku-5-5"), (5, 5))
        self.assertEqual(version_of("step-3.5-flash-2603"), (3, 5))
        self.assertEqual(version_of("qwen3.8-flash"), (3, 8))
        self.assertEqual(version_of("gpt-4o"), ())


class ManifestLabels(unittest.TestCase):
    """Every tool the supervisor declares carries a transcript label. gray headlines
    a tool with its label and only humanizes the wire name when none is set."""

    @classmethod
    def setUpClass(cls):
        loader = importlib.machinery.SourceFileLoader("gray_subagents_manifest", str(REPO / "mod"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        cls.mod = importlib.util.module_from_spec(spec)
        loader.exec_module(cls.mod)

    def test_every_declared_tool_has_a_clean_label(self):
        tools = self.mod.dispatch("plugin/manifest", {})["tools"]
        self.assertTrue(tools)
        for tool in tools:
            with self.subTest(tool=tool.get("name")):
                label = tool.get("label")
                self.assertIsInstance(label, str)
                self.assertTrue(label.strip(), "label must be non-empty")
                self.assertNotIn("_", label, "label must not be a raw wire name")

    def test_preview_templates_reference_declared_params(self):
        """A {field} or {a.b|filter} placeholder renders empty when the field
        is not a call arg; every head path must be a declared parameter."""
        for tool in self.mod.dispatch("plugin/manifest", {})["tools"]:
            with self.subTest(tool=tool.get("name")):
                preview = tool.get("preview")
                if not isinstance(preview, str):
                    continue
                props = tool.get("parameters", {}).get("properties", {})
                for inner in re.findall(r"\{([^{}]+)\}", preview):
                    head = inner.split("|", 1)[0].split(".")[0].strip()
                    if head:
                        self.assertIn(
                            head, props,
                            f"preview references undeclared param {head!r}",
                        )


if __name__ == "__main__":
    unittest.main()

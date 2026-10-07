"""Wire-level tests for the subagents sidecar (protocol 1.1 contract).

Persistent sidecars mirror the host. Detached supervisors own children; tests
also exercise shutdown, a fresh observer, and concurrent sidecar instances.
"""
import json, os, pathlib, select, signal, subprocess, tempfile, time, unittest

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
        self.env = dict(
            os.environ,
            GRAY_HOME=str(self.home),
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
                if job["status"] in ("running", "stopping"):
                    with_sc.call("subagents_stop", {"run_id": job["run_id"]})
                    for _ in range(150):
                        st = with_sc.call("subagents_status", {"run_id": job["run_id"]})
                        if st.get("status") not in ("running", "stopping"):
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

    def spawn(self, task="do work", agent="scout", env_extra=None):
        env = dict(self.env)
        env.update(env_extra or {})
        proc = Sidecar(env)
        self.addCleanup(proc.close)
        return proc.call("subagent", {"agent": agent, "task": task})

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

    def test_running_cap_rejects_jobs_over_limit(self):
        env = dict(FAKE_MODE="slow", FAKE_SLEEP="20", GRAY_SUBAGENTS_MAX_RUNNING="2")
        ids = [self.spawn(task=f"t{i}", env_extra=env)["run_id"] for i in range(2)]
        try:
            for i in ids:
                self.assertTrue((self.run_dir / f"{i}.json").exists())
            r = self.spawn(task="third", env_extra=env)
            self.assertTrue(r.get("is_error"))
            self.assertIn("running", r["content"])
        finally:
            for i in ids:
                self.sc.call("subagents_stop", {"run_id": i})

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
        self.assertEqual(r["text"], "")

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

if __name__ == "__main__":
    unittest.main()

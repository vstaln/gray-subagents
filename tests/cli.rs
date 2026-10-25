use std::process::Command;

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gray-subagents"));
    // The suite may run inside a subagent or a live session: every GRAY_* var
    // the caller exports (session id, model, recursion guards, binary and
    // provider overrides) must not leak into the binary, since the tests
    // assert defaults. Mirrors setUp in tests/test_subagents.py.
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("GRAY_")) {
            cmd.env_remove(key);
        }
    }
    cmd.env("GRAY_SESSION_ID", "");
    cmd
}

#[test]
fn cli_settings_preserve_fields_and_widget_is_empty_without_jobs() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = bin()
            .args(args)
            .env("GRAY_HOME", home.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    run(&["settings", "--max-running", "2"]);
    run(&["settings", "--model", "test/model"]);
    let config: serde_json::Value = serde_json::from_str(&run(&["settings"])).unwrap();
    assert_eq!(config["max_running"], 2);
    assert_eq!(config["model"], "test/model");
    let panel: serde_json::Value = serde_json::from_str(&run(&["widget"])).unwrap();
    assert_eq!(panel["text"], "");
}

#[test]
fn manifest_exposes_folded_tool_surface() {
    // gray-narrow and the subagent tools are folded in: the manifest carries their
    // model-facing tools (narrow_delegate + the subagent_* supervisor surface)
    // plus the Agents-view widget override and both plugin commands.
    let home = tempfile::tempdir().unwrap();
    let out = bin()
        .arg("manifest")
        .env("GRAY_HOME", home.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let manifest: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let tools: Vec<&str> = manifest["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for name in [
        "narrow_delegate",
        "subagent_spawn",
        "subagent_check",
        "subagent_message",
        "subagent_stop",
        "subagent_note",
    ] {
        assert!(tools.contains(&name), "missing tool {name}");
    }
    assert_eq!(manifest["widget"], serde_json::json!(true));
    let commands = manifest["commands"].as_array().unwrap();
    for cmd in ["/subagents", "/narrow", "/rlm"] {
        assert!(commands.contains(&serde_json::json!(cmd)), "missing {cmd}");
    }
    let caps = manifest["capabilities"].as_array().unwrap();
    assert!(caps.contains(&serde_json::json!("widget.override")));
    assert!(caps.contains(&serde_json::json!("host.say")));
}

#[test]
fn widget_demo_uses_real_rust_renderer() {
    let out = bin().args(["widget", "--demo"]).output().unwrap();
    assert!(out.status.success());
    let panel: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let text = panel["text"].as_str().unwrap();
    assert!(!text.contains("Agents\n"));
    assert!(text.contains("2 subagents"));
    assert!(text.contains("● auth-survey"));
    assert!(text.contains("searching"), "{text}");
}

// The child is a `#!/bin/sh` script exec'd via its mode bits — unix only.
#[cfg(unix)]
#[test]
fn bash_cli_runs_child_with_selected_model_and_returns_status() {
    let home = tempfile::tempdir().unwrap();
    let child = home.path().join("gray-child");
    std::fs::write(
        &child,
        "#!/bin/sh\nprintf 'child model=%s\\n' \"$GRAY_MODEL\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = bin()
        .args(["run", "Inspect", "--model", "test/selected"])
        .env("GRAY_HOME", home.path())
        .env("GRAY_SUBAGENTS_BIN", &child)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = result["run_id"].as_str().unwrap();
    for _ in 0..50 {
        let out = bin()
            .args(["status", id])
            .env("GRAY_HOME", home.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if status["status"] == "completed" {
            assert!(
                status["result"]
                    .as_str()
                    .unwrap()
                    .contains("child model=test/selected")
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("child never completed");
}

#[test]
fn run_rejects_invalid_effort_and_unknown_model() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("models.json"),
        r#"{"vendor/good-model": {}, "vendor/other": {}}"#,
    )
    .unwrap();
    let bad_effort = bin()
        .args(["run", "x", "--effort", "ludicrous"])
        .env("GRAY_HOME", home.path())
        .output()
        .unwrap();
    assert!(!bad_effort.status.success());
    assert!(String::from_utf8_lossy(&bad_effort.stderr).contains("effort"));

    let bad_model = bin()
        .args(["run", "x", "--model", "nope-zzz"])
        .env("GRAY_HOME", home.path())
        .output()
        .unwrap();
    assert!(!bad_model.status.success());
    assert!(String::from_utf8_lossy(&bad_model.stderr).contains("unknown model"));

    let ambiguous = bin()
        .args(["run", "x", "--model", "vendor"])
        .env("GRAY_HOME", home.path())
        .output()
        .unwrap();
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("vendor/good-model"));
}

// Same `#!/bin/sh` child — unix only.
#[cfg(unix)]
#[test]
fn run_joins_unquoted_task_words_and_keeps_flags() {
    let home = tempfile::tempdir().unwrap();
    let child = home.path().join("gray-child");
    std::fs::write(&child, "#!/bin/sh\necho \"child argv:$*\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = bin()
        .args(["run", "Inspect", "the", "codebase", "--name", "inspect-run"])
        .env("GRAY_HOME", home.path())
        .env("GRAY_SUBAGENTS_BIN", &child)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = result["run_id"].as_str().unwrap();
    for _ in 0..50 {
        let out = bin()
            .args(["status", id])
            .env("GRAY_HOME", home.path())
            .output()
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if status["status"] == "completed" {
            let result = status["result"].as_str().unwrap();
            assert!(result.contains("Inspect the codebase"), "{result}");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("child never completed");
}

#[test]
fn steer_requires_a_known_run() {
    let home = tempfile::tempdir().unwrap();
    let out = bin()
        .args(["steer", "deadbeef", "do", "more"])
        .env("GRAY_HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("run_id"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Write a synthetic registry job; `collect_entries` picks up session-less
/// runs by cwd, so no gray session is needed to exercise the menu.
fn seed_job(home: &std::path::Path, rid: &str, fields: serde_json::Value) {
    let dir = home.join("subagents/runs");
    std::fs::create_dir_all(&dir).unwrap();
    let mut job = serde_json::json!({
        "run_id": rid, "name": "", "task": "t",
        "status": "completed", "created": 100.0, "finished": 150.0,
        "error": "", "model": "", "session_id": "", "child_session": "",
        "activity": "", "cwd": std::env::current_dir().unwrap(),
        "result": "", "phases": [{"kind": "task"}],
    });
    let map = job.as_object_mut().unwrap();
    for (k, v) in fields.as_object().unwrap() {
        map.insert(k.clone(), v.clone());
    }
    std::fs::write(
        dir.join(format!("{rid}.json")),
        serde_json::to_string(&job).unwrap(),
    )
    .unwrap();
}

fn content(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["content"].as_str().unwrap().to_string()
}

#[test]
fn menu_numbers_rows_and_open_selects_them() {
    let home = tempfile::tempdir().unwrap();
    seed_job(
        home.path(),
        &"a".repeat(32),
        serde_json::json!({"name": "done-survey", "child_session": "tidal-quark-photon"}),
    );
    seed_job(
        home.path(),
        &"b".repeat(32),
        serde_json::json!({"name": "bad-que", "status": "failed", "error": "turn failed"}),
    );
    std::fs::create_dir_all(home.path().join("sessions")).unwrap();
    std::fs::write(
        home.path().join("sessions/tidal-quark-photon.jsonl"),
        "{\"model\":\"x\"}\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        bin()
            .args(args)
            .env("GRAY_HOME", home.path())
            .output()
            .unwrap()
    };

    let menu = content(run(&["menu"]));
    assert!(menu.starts_with("1 \u{2717} bad-que"), "{menu}");
    assert!(!menu.contains("\u{2b22} Agents"), "{menu}");
    assert!(menu.contains("2 \u{2713} done-survey"), "{menu}");
    assert!(menu.contains("N|NAME selects"), "{menu}");

    // Bare word and `open` agree; finished runs hand over /resume.
    let one = content(run(&["done-survey"]));
    let two = content(run(&["open", "2"]));
    let resume = "/resume tidal-quark-photon";
    assert!(one.contains(resume), "{one}");
    assert!(two.contains(resume), "{two}");

    // Failed run with no session points at the transcript.
    let bad = content(run(&["open", "1"]));
    assert!(bad.contains("bad-que"), "{bad}");
    assert!(bad.contains("view bad-que"), "{bad}");
}

#[test]
fn bare_status_is_the_menu_and_all_is_the_table() {
    let home = tempfile::tempdir().unwrap();
    seed_job(
        home.path(),
        &"c".repeat(32),
        serde_json::json!({"name": "menu-run"}),
    );
    let run = |args: &[&str]| {
        bin()
            .args(args)
            .env("GRAY_HOME", home.path())
            .output()
            .unwrap()
    };
    let bare = content(run(&["status"]));
    // The results-notice preamble may precede the menu rows.
    assert!(bare.contains("1 \u{2713} menu-run"), "{bare}");
    let all = content(run(&["status", "--all"]));
    assert!(all.contains("menu-run"), "{all}");
    assert!(all.contains("[completed]"), "{all}");
}

#[test]
fn entries_scope_runs_to_their_owner_session() {
    let home = tempfile::tempdir().unwrap();
    for i in 0..3 {
        seed_job(
            home.path(),
            &format!("{:032x}", 0x1000 + i),
            serde_json::json!({"name": format!("orphan-{i}")}),
        );
    }
    seed_job(
        home.path(),
        &"a".repeat(32),
        serde_json::json!({"name": "a-run", "session_id": "sess-a"}),
    );
    let names = |args: &[&str], sid: Option<&str>| {
        let mut cmd = bin();
        cmd.args(args).env("GRAY_HOME", home.path());
        if let Some(sid) = sid {
            cmd.env("GRAY_SESSION_ID", sid);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>()
    };
    let seeded = ["orphan-0", "orphan-1", "orphan-2", "a-run"];
    let other = names(&["entries"], Some("sess-b"));
    assert!(
        !seeded.iter().any(|n| other.iter().any(|e| e == n)),
        "{other:?}"
    );
    let owned = names(&["entries"], Some("sess-a"));
    assert_eq!(owned, ["a-run"], "{owned:?}");
    let all = names(&["entries", "--all"], Some("sess-b"));
    assert!(seeded.iter().all(|n| all.iter().any(|e| e == n)), "{all:?}");
    let orphan_view = names(&["entries"], None);
    assert!(
        ["orphan-0", "orphan-1", "orphan-2"]
            .iter()
            .all(|n| orphan_view.iter().any(|e| e == n))
            && !orphan_view.iter().any(|e| e == "a-run"),
        "{orphan_view:?}"
    );
}

#[test]
fn empty_menu_invites_a_run() {
    let home = tempfile::tempdir().unwrap();
    let out = bin()
        .arg("menu")
        .env("GRAY_HOME", home.path())
        .output()
        .unwrap();
    let text = content(out);
    assert!(text.contains("no runs yet"), "{text}");
}

#[test]
fn legacy_rlm_tool_names_dispatch_but_are_not_advertised() {
    // Sessions recorded before the rename replay rlm_* calls. The sidecar must
    // still route them to the subagent_* handlers, while the manifest lists
    // only the canonical names.
    use std::io::Write;
    let home = tempfile::tempdir().unwrap();
    let manifest_out = bin()
        .arg("manifest")
        .env("GRAY_HOME", home.path())
        .output()
        .unwrap();
    assert!(manifest_out.status.success());
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_out.stdout).unwrap();
    let tools: Vec<&str> = manifest["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for legacy in [
        "rlm_spawn",
        "rlm_collect",
        "rlm_send",
        "rlm_delete",
        "rlm_progress_note",
    ] {
        assert!(!tools.contains(&legacy), "legacy {legacy} is advertised");
    }
    let mut sidecar = bin()
        .arg("sidecar")
        .env("GRAY_HOME", home.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let requests = concat!(
        r#"{"id":1,"method":"tool/call","params":{"name":"rlm_send","args":{}}}"#,
        "\n",
        r#"{"id":2,"method":"tool/call","params":{"name":"subagent_message","args":{}}}"#,
        "\n",
    );
    sidecar
        .stdin
        .take()
        .unwrap()
        .write_all(requests.as_bytes())
        .unwrap();
    let out = sidecar.wait_with_output().unwrap();
    assert!(out.status.success());
    let replies: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 2, "{replies:?}");
    for reply in &replies {
        assert_eq!(reply["result"]["content"], "subagent_message requires name");
        assert_eq!(reply["result"]["is_error"], serde_json::json!(true));
    }
}

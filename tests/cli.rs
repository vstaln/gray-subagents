use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gray-subagents"))
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
fn manifest_exposes_no_model_tools() {
    let home = tempfile::tempdir().unwrap();
    let out = bin()
        .arg("manifest")
        .env("GRAY_HOME", home.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let manifest: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(manifest["tools"], serde_json::json!([]));
    assert_eq!(manifest["widget"], serde_json::json!(false));
    assert!(
        manifest["commands"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("/subagents"))
    );
}

#[test]
fn widget_demo_uses_real_rust_renderer() {
    let out = bin().args(["widget", "--demo"]).output().unwrap();
    assert!(out.status.success());
    let panel: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let text = panel["text"].as_str().unwrap();
    assert!(text.starts_with("⬢ Agents\n"));
    assert!(text.contains("⬡ auth-scout"));
    assert!(text.contains("⎿"));
}

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

#[test]
fn run_joins_unquoted_task_words_and_keeps_flags() {
    let home = tempfile::tempdir().unwrap();
    let child = home.path().join("gray-child");
    std::fs::write(&child, "#!/bin/sh\necho \"child argv:$*\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = bin()
        .args(["run", "Inspect", "the", "codebase", "--agent", "reviewer"])
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
            assert!(result.contains("'reviewer'"), "{result}");
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
        "run_id": rid, "name": "", "agent": "scout", "task": "t",
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
        &format!("{}", "a".repeat(32)),
        serde_json::json!({"name": "done-scout", "child_session": "tidal-quark-photon"}),
    );
    seed_job(
        home.path(),
        &format!("{}", "b".repeat(32)),
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
    assert!(menu.starts_with("\u{2b22} Agents"), "{menu}");
    assert!(
        menu.contains("\u{251c}\u{2500} 1 \u{2717} bad-que"),
        "{menu}"
    );
    assert!(menu.contains("2 \u{2713} done-scout"), "{menu}");
    assert!(menu.contains("N|NAME selects"), "{menu}");

    // Bare word and `open` agree; finished runs hand over /resume.
    let one = content(run(&["done-scout"]));
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
        &format!("{}", "c".repeat(32)),
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
    assert!(bare.starts_with("\u{2b22} Agents"), "{bare}");
    assert!(bare.contains("1 \u{2713} menu-run"), "{bare}");
    let all = content(run(&["status", "--all"]));
    assert!(all.contains("menu-run"), "{all}");
    assert!(all.contains("[completed]"), "{all}");
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

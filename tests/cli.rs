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
    assert!(
        manifest["commands"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("/subagent"))
    );
}

#[test]
fn widget_demo_uses_real_rust_renderer() {
    let out = bin().args(["widget", "--demo"]).output().unwrap();
    assert!(out.status.success());
    let panel: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let text = panel["text"].as_str().unwrap();
    assert!(text.starts_with("⬢ Agents\n"));
    assert!(text.contains("⬡ Scout"));
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

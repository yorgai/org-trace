//! Smoke tests for the trimmed Brick CLI/MCP surface.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_brick");

fn unique_tmp(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("brick-mcp-it-{tag}-{nanos}-{}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn setup_repo(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = unique_tmp(tag);
    let home = root.join("home");
    let repo = root.join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    assert!(Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["add", "-A"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["commit", "-qm", "init"])
        .current_dir(&repo)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .unwrap()
        .success());
    (root, home, repo)
}

fn brick(home: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("BRICK_HOME", home)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run brick")
}

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    id: i64,
}

impl Mcp {
    fn spawn(home: &Path, cwd: &Path) -> Self {
        Self::spawn_inner(home, cwd, false)
    }

    fn spawn_planning(home: &Path, cwd: &Path) -> Self {
        Self::spawn_inner(home, cwd, true)
    }

    fn spawn_inner(home: &Path, cwd: &Path, planning: bool) -> Self {
        let mut command = Command::new(BIN);
        command.arg("mcp-serve");
        if planning {
            command.arg("--planning");
        }
        let mut child = command
            .current_dir(cwd)
            .env("BRICK_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mcp-serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut mcp = Self {
            child,
            stdin: Some(stdin),
            stdout,
            id: 0,
        };
        let _ = mcp.rpc("initialize", json!({}));
        mcp
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let req = json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params});
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{req}").expect("write rpc");
        stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read rpc");
        serde_json::from_str(&line).expect("parse rpc response")
    }

    fn call(&mut self, tool: &str, args: Value) -> Value {
        let resp = self.rpc("tools/call", json!({"name": tool, "arguments": args}));
        if let Some(err) = resp.get("error") {
            return json!({ "_rpc_error": err });
        }
        if resp["result"]["isError"].as_bool() == Some(true) {
            let msg = resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("tool error");
            return json!({ "_rpc_error": msg });
        }
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text content");
        serde_json::from_str(text).expect("parse tool payload")
    }

    fn tool_names(&mut self) -> Vec<String> {
        let resp = self.rpc("tools/list", json!({}));
        resp["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn main_surface_is_explain_only() {
    let (root, home, repo) = setup_repo("surface");
    let mut m = Mcp::spawn(&home, &repo);

    let mut tools = m.tool_names();
    tools.sort();
    assert_eq!(tools, vec!["explain".to_string()]);

    let retired = m.call("search", json!({}));
    assert_eq!(
        retired.get("error").and_then(Value::as_str),
        Some("tool_retired")
    );

    // `link` is now retired too — it must report a migration hint, not run.
    let retired_link = m.call("link", json!({}));
    assert_eq!(
        retired_link.get("error").and_then(Value::as_str),
        Some("tool_retired")
    );

    let unknown = m.call("totally_made_up_tool", json!({}));
    assert!(unknown.get("_rpc_error").is_some());

    let resp = m.rpc("tools/list", json!({}));
    let desc = resp["result"]["tools"][0]["description"]
        .as_str()
        .expect("tool description");
    assert!(desc.contains("If `causal_chain` is non-empty"));
    assert!(desc.contains("follow `next_action`"));

    drop(m);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn planning_surface_exposes_planning_tools() {
    let (root, home, repo) = setup_repo("planning-surface");
    let mut m = Mcp::spawn_planning(&home, &repo);

    let mut tools = m.tool_names();
    tools.sort();
    assert_eq!(
        tools,
        vec![
            "artifact_add".to_string(),
            "artifact_attach".to_string(),
            "mission".to_string(),
            "mission_list".to_string(),
            "show_mission".to_string(),
        ]
    );
    assert!(!tools.contains(&"explain".to_string()));

    drop(m);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cli_explain_returns_no_record_json() {
    let (root, home, repo) = setup_repo("cli-explain");
    let out = brick(&home, &repo, &["explain", "src/main.rs"]);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).expect("json output");
    assert!(value.get("anchor").is_some());
    assert!(value.get("causal_chain").is_some());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cli_link_command_is_removed() {
    let (root, home, repo) = setup_repo("cli-link");
    // `brick link` no longer exists — clap should reject it as an unknown
    // subcommand (Brick is now a read-only file-history timeline).
    let out = brick(&home, &repo, &["link", "--note", "record why"]);
    assert!(
        !out.status.success(),
        "`brick link` must no longer be a valid command"
    );
    let _ = std::fs::remove_dir_all(root);
}

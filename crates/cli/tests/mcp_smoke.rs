//! End-to-end integration test for the MCP capability kit.
//!
//! Spawns the real `brick mcp-serve` binary (built by cargo as
//! `CARGO_BIN_EXE_brick`) and drives it over the real stdio JSON-RPC protocol —
//! the exact surface a Claude Code / Codex / ORGII MCP client speaks. Two native
//! source profiles (codex_app + claude_code) are backed by real transcript files
//! in their native on-disk formats, so this exercises real session discovery,
//! liveness probing, FTS5 search, and the planning/coordination loop end to end.
//!
//! Everything runs under a private temp `BRICK_HOME` and a throwaway git repo, so
//! it never touches the developer's real Brick home or working tree. No network,
//! no LLM, fully deterministic.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_brick");

/// A unique temp dir for one test run; mirrors the std-only convention in
/// `crates/core/tests` (no `tempfile` dependency).
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

/// Runs a `brick` subcommand to completion under the given home/cwd.
fn brick(home: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("BRICK_HOME", home)
        .output()
        .expect("run brick")
}

/// Extracts a `key=value` line value from CLI stdout.
fn extract(output: &std::process::Output, key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Near-now RFC3339 UTC timestamp so transcripts land inside the liveness
/// ACTIVE_WINDOW (120s).
fn iso(offset_secs: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(offset_secs)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A persistent `brick mcp-serve` stdio JSON-RPC session.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    id: i64,
}

impl Mcp {
    fn spawn(home: &Path, cwd: &Path) -> Self {
        let mut child = Command::new(BIN)
            .arg("mcp-serve")
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

    /// Calls a tool and returns its parsed JSON payload (the text content block).
    fn call(&mut self, tool: &str, args: Value) -> Value {
        let resp = self.rpc("tools/call", json!({"name": tool, "arguments": args}));
        if let Some(err) = resp.get("error") {
            return json!({ "_error": err });
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
        // Closing stdin ends the blocking serve loop so the child exits cleanly;
        // kill is a belt-and-suspenders fallback if it is still alive.
        self.stdin.take();
        let _ = self.child.wait();
        let _ = self.child.kill();
    }
}

fn write_lines(path: &Path, lines: &[Value]) {
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(path, body).expect("write transcript");
}

/// A live Codex session (open turn) that patched a real repo file.
fn write_codex(dir: &Path, sid: &str, repo: &Path, file: &str) {
    let patch = format!("diff --git a/{file} b/{file}\n+++ b/{file}\n+// cache git status\n");
    let lines = vec![
        json!({"timestamp":iso(-30),"payload":{"type":"user_message","message":"Cache git status lookups in commands_git","cwd":repo.display().to_string(),"model":"gpt-5"}}),
        json!({"timestamp":iso(-25),"payload":{"type":"task_started"}}),
        json!({"timestamp":iso(-20),"payload":{"type":"agent_message","message":"Adding a cache layer"}}),
        json!({"timestamp":iso(-15),"payload":{"type":"function_call","call_id":"c1","name":"apply_patch","arguments": json!({"patch": patch}).to_string()}}),
        json!({"timestamp":iso(-14),"payload":{"type":"function_call_output","call_id":"c1","output":"applied"}}),
        // No task_complete → the turn is still open → Active.
    ];
    write_lines(&dir.join(format!("{sid}.jsonl")), &lines);
}

/// A finished Claude session (assistant stop_reason set → Idle).
fn write_claude(dir: &Path, sid: &str, repo: &Path, file: &str) {
    let lines = vec![
        json!({"type":"user","timestamp":iso(-300),"message":{"role":"user","content":format!("Review {file}"),"cwd":repo.display().to_string()}}),
        json!({"type":"assistant","timestamp":iso(-280),"message":{"content":[{"type":"text","text":"Done"}],"stop_reason":"end_turn"}}),
    ];
    write_lines(&dir.join(format!("{sid}.jsonl")), &lines);
}

#[test]
fn mcp_capability_kit_end_to_end() {
    let root = unique_tmp("e2e");
    let home = root.join("home");
    let repo = root.join("repo");
    let codex_dir = root.join("codex");
    let claude_dir = root.join("claude");
    for d in [&home, &repo, &codex_dir, &claude_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    // A real git repo with real tracked source files the agents reference.
    let file_codex = "src/commands_git.rs";
    let file_claude = "src/commands_memory.rs";
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join(file_codex), "// real file\n").unwrap();
    std::fs::write(repo.join(file_claude), "// real file\n").unwrap();
    assert!(Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());

    brick(&home, &repo, &["init"]);
    brick(
        &home,
        &repo,
        &[
            "source",
            "configure",
            "--name",
            "codex_app",
            "--app-id",
            "codex_app",
            "--actor-id",
            "codex-agent",
            "--actor-type",
            "agent",
            "--session-log-path",
            codex_dir.to_str().unwrap(),
        ],
    );
    brick(
        &home,
        &repo,
        &[
            "source",
            "configure",
            "--name",
            "claude_code",
            "--app-id",
            "claude_code",
            "--actor-id",
            "claude-agent",
            "--actor-type",
            "agent",
            "--session-log-path",
            claude_dir.to_str().unwrap(),
        ],
    );
    write_codex(&codex_dir, "codex-live-001", &repo, file_codex);
    write_claude(&claude_dir, "claude-done-002", &repo, file_claude);

    let org = extract(
        &brick(&home, &repo, &["org", "create", "SmokeOrg"]),
        "org_id",
    )
    .expect("org_id");
    let proj = extract(
        &brick(
            &home,
            &repo,
            &["project", "create", "--org", &org, "SmokeProject"],
        ),
        "project_id",
    )
    .expect("project_id");

    let mut m = Mcp::spawn(&home, &repo);

    // ---- tools/list: all 13 present ----
    let tools = m.tool_names();
    for want in [
        "explore_memory",
        "recall_file",
        "search_sessions",
        "read_session",
        "current_context",
        "list_missions",
        "show_mission",
        "manage_mission",
        "record_artifact",
        "attach_evidence",
        "live_sessions",
        "announce_work",
        "list_announcements",
    ] {
        assert!(
            tools.contains(&want.to_string()),
            "missing tool {want}; got {tools:?}"
        );
    }

    // ---- live_sessions: sees running Codex, not the finished Claude ----
    let live = m.call("live_sessions", json!({}));
    let sessions = live["sessions"].as_array().cloned().unwrap_or_default();
    let live_ids: Vec<&str> = sessions
        .iter()
        .filter_map(|s| s["external_session_id"].as_str())
        .collect();
    assert!(
        live_ids.contains(&"codex-live-001"),
        "codex not live: {live_ids:?}"
    );
    assert!(
        !live_ids.contains(&"claude-done-002"),
        "finished claude should not be live: {live_ids:?}"
    );
    // Real git work_scope resolution: the repo dir name appears in the row.
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    let codex_row = sessions
        .iter()
        .find(|s| s["external_session_id"] == "codex-live-001")
        .unwrap();
    assert!(
        codex_row.to_string().contains(repo_name),
        "work_scope not resolved: {codex_row}"
    );

    // ---- search_sessions: FTS5 tokenized (out-of-order) + substring ----
    let sr = m.call("search_sessions", json!({"query": "git status cache"}));
    assert!(
        sr["match_count"].as_u64().unwrap_or(0) >= 1,
        "out-of-order query failed: {sr}"
    );
    let sr2 = m.call("search_sessions", json!({"query": "commands_git"}));
    assert!(
        sr2["match_count"].as_u64().unwrap_or(0) >= 1,
        "substring file query failed: {sr2}"
    );

    // ---- recall_file / read_session / explore_memory ----
    let rc = m.call("recall_file", json!({"path": file_codex}));
    assert!(
        rc["session_count"].as_u64().unwrap_or(0) >= 1
            || rc.to_string().to_lowercase().contains("commands_git"),
        "recall_file found nothing: {rc}"
    );
    let rs = m.call(
        "read_session",
        json!({"source": "codex_app", "session_id": "codex-live-001"}),
    );
    assert!(rs.get("_error").is_none(), "read_session error: {rs}");
    let em = m.call(
        "explore_memory",
        json!({"question": "how did we speed up git status"}),
    );
    assert!(em.get("_error").is_none(), "explore_memory error: {em}");

    // ---- planning loop ----
    let cc = m.call("current_context", json!({}));
    assert!(
        cc.get("counts").is_some(),
        "current_context missing counts: {cc}"
    );
    let created = m.call("manage_mission", json!({"action":"create","project":proj,"title":"Cache git status","status":"active","source":"codex_app"}));
    let mid = created["mission_id"]
        .as_str()
        .expect("mission_id")
        .to_string();
    assert_eq!(created["created"], json!(true));
    let lm = m.call("list_missions", json!({"status":"active"}));
    assert!(
        lm["missions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["title"] == "Cache git status"),
        "mission not listed: {lm}"
    );
    let art = m.call(
        "record_artifact",
        json!({"title":"PR: cache","kind":"patch","mission":mid,"source":"codex_app"}),
    );
    let aid = art["artifact_id"]
        .as_str()
        .expect("artifact_id")
        .to_string();
    assert_eq!(art["recorded"], json!(true));
    let ev = m.call(
        "attach_evidence",
        json!({"artifact":aid,"path":file_codex,"source":"codex_app"}),
    );
    assert_eq!(ev["attached"], json!(true));
    let sm = m.call("show_mission", json!({"mission":mid}));
    assert!(
        sm["artifact_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == &json!(aid)),
        "artifact not under mission: {sm}"
    );
    let upd = m.call(
        "manage_mission",
        json!({"action":"update","mission":mid,"status":"completed"}),
    );
    assert_eq!(upd["updated"], json!(true));

    // ---- announce_work + liveness-aware retirement ----
    m.call("announce_work", json!({"scope":file_codex,"message":"editing","source":"codex_app","session_id":"codex-live-001"}));
    m.call(
        "announce_work",
        json!({"scope":"src/ghost.rs","message":"bare mcp","source":"mcp","session_id":"ghost"}),
    );
    m.call("announce_work", json!({"scope":file_claude,"message":"reviewing","source":"claude_code","session_id":"claude-done-002"}));
    let la = m.call("list_announcements", json!({}));
    let scopes: Vec<&str> = la["announcements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["scope"].as_str())
        .collect();
    assert!(
        scopes.contains(&file_codex),
        "live codex claim should be kept: {scopes:?}"
    );
    assert!(
        scopes.contains(&"src/ghost.rs"),
        "unprobeable bare-mcp claim should be kept (TTL): {scopes:?}"
    );
    assert!(
        !scopes.contains(&file_claude),
        "dead claude-session claim should be retired: {scopes:?}"
    );
    let rc2 = m.call("recall_file", json!({"path": file_codex}));
    assert!(
        rc2.to_string().contains("active_claims"),
        "recall_file should surface active_claims: {rc2}"
    );

    // ---- cross-tool: a Claude view reads Codex-authored mission/artifact ----
    let sm2 = m.call("show_mission", json!({"mission":mid}));
    assert_eq!(sm2["title"], "Cache git status");
    assert!(sm2["artifact_ids"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a == &json!(aid)));

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

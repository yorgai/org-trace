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
    write_codex_at(dir, sid, repo, file, 0, false);
}

/// Writes a Codex transcript with controllable freshness and turn state.
///
/// `base_offset` shifts every embedded timestamp into the past (seconds), which
/// drives the liveness ACTIVE_WINDOW gate because `session_updated_at` is parsed
/// from these timestamps — not the file mtime. `closed` appends a `task_complete`
/// so the turn-signal parser reports the turn as finished (Idle).
fn write_codex_at(dir: &Path, sid: &str, repo: &Path, file: &str, base_offset: i64, closed: bool) {
    let patch = format!("diff --git a/{file} b/{file}\n+++ b/{file}\n+// cache git status\n");
    let mut lines = vec![
        json!({"timestamp":iso(base_offset - 30),"payload":{"type":"user_message","message":"Cache git status lookups in commands_git","cwd":repo.display().to_string(),"model":"gpt-5"}}),
        json!({"timestamp":iso(base_offset - 25),"payload":{"type":"task_started"}}),
        json!({"timestamp":iso(base_offset - 20),"payload":{"type":"agent_message","message":"Adding a cache layer"}}),
        json!({"timestamp":iso(base_offset - 15),"payload":{"type":"function_call","call_id":"c1","name":"apply_patch","arguments": json!({"patch": patch}).to_string()}}),
        json!({"timestamp":iso(base_offset - 14),"payload":{"type":"function_call_output","call_id":"c1","output":"applied"}}),
    ];
    if closed {
        lines.push(json!({"timestamp":iso(base_offset - 12),"payload":{"type":"task_complete"}}));
    }
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

/// A live Claude session (trailing assistant with no stop_reason → Active).
fn write_claude_live(dir: &Path, sid: &str, repo: &Path, file: &str) {
    let lines = vec![
        json!({"type":"user","timestamp":iso(-20),"message":{"role":"user","content":format!("Refactor {file}"),"cwd":repo.display().to_string()}}),
        json!({"type":"assistant","timestamp":iso(-10),"message":{"content":[{"type":"text","text":"Working"}],"stop_reason":Value::Null}}),
    ];
    write_lines(&dir.join(format!("{sid}.jsonl")), &lines);
}

/// Spins up two configured native source profiles under one `BRICK_HOME` and a
/// temp git repo. Returns `(root, home, repo, codex_dir, claude_dir)`. Shared by
/// the behavioral tests so each gets an isolated, fully-initialized Brick world.
fn setup_world(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let root = unique_tmp(tag);
    let home = root.join("home");
    let repo = root.join("repo");
    let codex_dir = root.join("codex");
    let claude_dir = root.join("claude");
    for d in [&home, &repo, &codex_dir, &claude_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/commands_git.rs"), "// real file\n").unwrap();
    std::fs::write(repo.join("src/commands_memory.rs"), "// real file\n").unwrap();
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
    (root, home, repo, codex_dir, claude_dir)
}

/// External session ids visible in a `live_sessions` response.
fn live_ids(m: &mut Mcp) -> Vec<String> {
    let live = m.call("live_sessions", json!({}));
    live["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s["external_session_id"].as_str().map(str::to_string))
        .collect()
}

/// Active-claim scopes visible in a `list_announcements` response.
fn claim_scopes(m: &mut Mcp) -> Vec<String> {
    let la = m.call("list_announcements", json!({}));
    la["announcements"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|a| a["scope"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn mcp_capability_kit_end_to_end() {
    let (root, home, repo, codex_dir, claude_dir) = setup_world("e2e");
    let file_codex = "src/commands_git.rs";
    let file_claude = "src/commands_memory.rs";
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

/// Liveness is recomputed every call, never cached: a session that is live while
/// its turn is open must drop out of `live_sessions` the moment the transcript
/// gains a completion marker — on the SAME long-lived mcp-serve process. A static
/// one-shot snapshot cannot tell "recomputed" apart from "cached after first
/// scan"; only this in-place flip proves the architectural claim.
#[test]
fn liveness_flips_when_turn_completes_same_process() {
    let (root, home, repo, codex_dir, _claude_dir) = setup_world("flip-turn");
    let file = "src/commands_git.rs";
    write_codex_at(&codex_dir, "codex-turn", &repo, file, 0, false); // open turn, fresh

    let mut m = Mcp::spawn(&home, &repo);
    assert!(
        live_ids(&mut m).contains(&"codex-turn".to_string()),
        "open fresh turn should be live"
    );

    // Same session id, same freshness, but now the turn is complete.
    write_codex_at(&codex_dir, "codex-turn", &repo, file, 0, true);
    assert!(
        !live_ids(&mut m).contains(&"codex-turn".to_string()),
        "completed turn must drop out of live_sessions on the next call (recomputed, not cached)"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

/// The 120s ACTIVE_WINDOW is real: an open-turn session that is fresh shows as
/// live, but the identical transcript aged past the window must read as not-live
/// even though its turn is still open — proving recency gates before turn signals.
#[test]
fn liveness_respects_active_window_same_process() {
    let (root, home, repo, codex_dir, _claude_dir) = setup_world("flip-window");
    let file = "src/commands_git.rs";
    write_codex_at(&codex_dir, "codex-fresh", &repo, file, 0, false); // open + fresh

    let mut m = Mcp::spawn(&home, &repo);
    assert!(
        live_ids(&mut m).contains(&"codex-fresh".to_string()),
        "fresh open turn should be live"
    );

    // Push every timestamp ~200s into the past (> ACTIVE_WINDOW = 120s). Still an
    // open turn, but stale → must be Idle without even consulting turn signals.
    write_codex_at(&codex_dir, "codex-fresh", &repo, file, -200, false);
    assert!(
        !live_ids(&mut m).contains(&"codex-fresh".to_string()),
        "an aged session must fall out of the active window regardless of open turn"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

/// Two independent mcp-serve processes — modeling Codex and Claude Code running
/// side by side against the same machine — share one BRICK_HOME. Work announced
/// by one process is immediately visible to the other (real cross-client
/// coordination), and when the announcing session ends, its claim is retired on
/// the peer's next read (liveness-aware retirement across process boundaries).
#[test]
fn cross_client_announcement_visibility_and_retirement() {
    let (root, home, repo, codex_dir, claude_dir) = setup_world("cross-client");
    let codex_file = "src/commands_git.rs";

    // A live Codex session (process A's "self") and a live Claude session whose
    // claim we will later retire by ending it.
    write_codex_at(&codex_dir, "codex-A", &repo, codex_file, 0, false);
    write_claude_live(&claude_dir, "claude-B", &repo, "src/commands_memory.rs");

    let mut client_a = Mcp::spawn(&home, &repo); // pretend: Codex's MCP client
    let mut client_b = Mcp::spawn(&home, &repo); // pretend: Claude Code's MCP client

    // Client A announces work tied to its live Codex session.
    client_a.call("announce_work", json!({
        "scope": codex_file, "message": "refactoring", "source": "codex_app", "session_id": "codex-A"
    }));
    // Client B announces work tied to its live Claude session.
    client_b.call("announce_work", json!({
        "scope": "src/commands_memory.rs", "message": "reviewing", "source": "claude_code", "session_id": "claude-B"
    }));

    // Cross-client visibility: B sees A's claim and vice versa.
    let seen_by_b = claim_scopes(&mut client_b);
    assert!(
        seen_by_b.contains(&codex_file.to_string()),
        "B must see A's claim: {seen_by_b:?}"
    );
    assert!(
        seen_by_b.contains(&"src/commands_memory.rs".to_string()),
        "B must see its own claim: {seen_by_b:?}"
    );
    let seen_by_a = claim_scopes(&mut client_a);
    assert!(
        seen_by_a.contains(&"src/commands_memory.rs".to_string()),
        "A must see B's claim: {seen_by_a:?}"
    );

    // End Claude session B: its transcript becomes a finished turn. The claim is
    // now owned by a dead session and must be retired on the next peer read.
    write_claude(&claude_dir, "claude-B", &repo, "src/commands_memory.rs");
    let after = claim_scopes(&mut client_a);
    assert!(
        after.contains(&codex_file.to_string()),
        "A's claim (live session) must survive: {after:?}"
    );
    assert!(
        !after.contains(&"src/commands_memory.rs".to_string()),
        "B's claim must be retired once its session ended, seen from the peer process: {after:?}"
    );

    drop(client_a);
    drop(client_b);
    let _ = std::fs::remove_dir_all(&root);
}

/// End-to-end line-level AI blame: an agent session captures a working diff that
/// adds two lines to a file, then `blame_file` (over the real mcp-serve binary)
/// must attribute exactly those current line numbers to that session/actor/
/// mission, while untouched lines stay unattributed. Proves the whole chain —
/// unified hunk capture → event log → blame replay — closes the provenance loop.
#[test]
fn blame_file_attributes_changed_lines_to_agent_session() {
    let root = unique_tmp("blame");
    let home = root.join("home");
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();

    // A real git repo with a committed baseline file.
    assert!(git(&repo, &["init", "-q"]).success());
    assert!(git(&repo, &["config", "user.email", "t@t.com"]).success());
    assert!(git(&repo, &["config", "user.name", "t"]).success());
    std::fs::write(repo.join("src/main.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
    assert!(git(&repo, &["add", "-A"]).success());
    assert!(git(&repo, &["commit", "-qm", "init"]).success());

    brick(&home, &repo, &["init"]);
    let org = extract(
        &brick(&home, &repo, &["org", "create", "BlameOrg"]),
        "org_id",
    )
    .expect("org id");
    let project = extract(
        &brick(
            &home,
            &repo,
            &["project", "create", "--org", &org, "BlameProj"],
        ),
        "project_id",
    )
    .expect("project id");
    let mission = extract(
        &brick(
            &home,
            &repo,
            &[
                "--actor-type",
                "agent",
                "--actor-id",
                "codex-bot",
                "mission",
                "create",
                "--project",
                &project,
                "Add y",
                "--status",
                "active",
            ],
        ),
        "mission_id",
    )
    .expect("mission id");
    let session = extract(
        &brick(
            &home,
            &repo,
            &[
                "--actor-type",
                "agent",
                "--actor-id",
                "codex-bot",
                "session",
                "start",
                "--mission",
                &mission,
                "--name",
                "s1",
            ],
        ),
        "session_id",
    )
    .expect("session id");
    let artifact = extract(
        &brick(
            &home,
            &repo,
            &[
                "--actor-type",
                "agent",
                "--actor-id",
                "codex-bot",
                "artifact",
                "create",
                "--mission",
                &mission,
                "--kind",
                "patch",
                "y patch",
            ],
        ),
        "artifact_id",
    )
    .expect("artifact id");

    // The agent edits the file (adds lines 3 and 4) and captures a working diff
    // bound to its session — this is the line-level provenance event.
    std::fs::write(
        repo.join("src/main.rs"),
        "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
    )
    .unwrap();
    let captured = brick(
        &home,
        &repo,
        &[
            "--actor-type",
            "agent",
            "--actor-id",
            "codex-bot",
            "evidence",
            "diff",
            "--artifact",
            &artifact,
            "--session",
            &session,
            "--mission",
            &mission,
            "--target",
            "working",
        ],
    );
    assert!(
        captured.status.success(),
        "evidence diff failed: {}",
        String::from_utf8_lossy(&captured.stderr)
    );

    // Blame over the real mcp-serve binary, cwd = repo.
    let mut m = Mcp::spawn(&home, &repo);
    let blame = m.call("blame_file", json!({"path": "src/main.rs"}));
    let lines = blame["lines"].as_array().expect("lines array");
    // Owner mode: line_count is the whole file, but only owned lines are returned.
    assert_eq!(blame["line_count"], json!(5), "file has 5 lines: {blame}");
    assert_eq!(blame["mode"], json!("owner"), "default owner mode: {blame}");

    let owned =
        |n: u64| -> Option<&Value> { lines.iter().find(|line| line["line_no"] == json!(n)) };
    // Lines 3 and 4 are the agent's additions → attributed to its session.
    for n in [3u64, 4] {
        let line = owned(n).unwrap_or_else(|| panic!("owned line {n} missing: {blame}"));
        assert_eq!(
            line["session_id"],
            json!(session),
            "line {n} should attribute to the capturing session: {line}"
        );
        assert_eq!(
            line["actor_id"],
            json!("codex-bot"),
            "line {n} actor: {line}"
        );
        assert_eq!(
            line["mission_id"],
            json!(mission),
            "line {n} mission: {line}"
        );
        assert_eq!(
            line["confidence"],
            json!("working"),
            "line {n} confidence: {line}"
        );
    }
    // Line 1 (`fn main() {`) was untouched → not an owned line, so it is absent
    // from the owner-mode response (collapsed into skipped_lines).
    assert!(
        owned(1).is_none(),
        "untouched line 1 must not appear in owner mode: {blame}"
    );
    assert_eq!(
        blame["owner_lines"],
        json!(2),
        "exactly 2 owner lines: {blame}"
    );
    assert_eq!(
        blame["skipped_lines"],
        json!(3),
        "remaining 3 lines skipped: {blame}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

/// Runs a git subcommand in `repo`, returning its exit status.
fn git(repo: &Path, args: &[&str]) -> std::process::ExitStatus {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("run git")
}

/// Bootstraps a git repo + brick home + an active agent mission/session/artifact
/// for blame tests. Returns `(root, home, repo, session, mission, artifact)`.
fn blame_world(tag: &str, seed_files: &[(&str, &str)]) -> BlameWorld {
    let root = unique_tmp(tag);
    let home = root.join("home");
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    assert!(git(&repo, &["init", "-q"]).success());
    assert!(git(&repo, &["config", "user.email", "t@t.com"]).success());
    assert!(git(&repo, &["config", "user.name", "t"]).success());
    for (path, body) in seed_files {
        std::fs::write(repo.join(path), body).unwrap();
    }
    assert!(git(&repo, &["add", "-A"]).success());
    assert!(git(&repo, &["commit", "-qm", "init"]).success());

    brick(&home, &repo, &["init"]);
    let org = extract(&brick(&home, &repo, &["org", "create", "O"]), "org_id").expect("org");
    let project = extract(
        &brick(&home, &repo, &["project", "create", "--org", &org, "P"]),
        "project_id",
    )
    .expect("project");
    let mission = extract(
        &brick(
            &home,
            &repo,
            &[
                "--actor-type",
                "agent",
                "--actor-id",
                "codex-bot",
                "mission",
                "create",
                "--project",
                &project,
                "m",
                "--status",
                "active",
            ],
        ),
        "mission_id",
    )
    .expect("mission");
    let session = extract(
        &brick(
            &home,
            &repo,
            &[
                "--actor-type",
                "agent",
                "--actor-id",
                "codex-bot",
                "session",
                "start",
                "--mission",
                &mission,
                "--name",
                "s1",
            ],
        ),
        "session_id",
    )
    .expect("session");
    let artifact = extract(
        &brick(
            &home,
            &repo,
            &[
                "--actor-type",
                "agent",
                "--actor-id",
                "codex-bot",
                "artifact",
                "create",
                "--mission",
                &mission,
                "--kind",
                "patch",
                "p",
            ],
        ),
        "artifact_id",
    )
    .expect("artifact");
    BlameWorld {
        root,
        home,
        repo,
        session,
        mission,
        artifact,
    }
}

struct BlameWorld {
    root: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    session: String,
    mission: String,
    artifact: String,
}

impl BlameWorld {
    /// Captures a working diff for the whole tree, bound to the agent session.
    fn capture_working(&self) {
        let out = brick(
            &self.home,
            &self.repo,
            &[
                "--actor-type",
                "agent",
                "--actor-id",
                "codex-bot",
                "evidence",
                "diff",
                "--artifact",
                &self.artifact,
                "--session",
                &self.session,
                "--mission",
                &self.mission,
                "--target",
                "working",
            ],
        );
        assert!(
            out.status.success(),
            "capture failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn blame(&self, path: &str) -> Value {
        let mut m = Mcp::spawn(&self.home, &self.repo);
        let v = m.call("blame_file", json!({ "path": path }));
        drop(m);
        v
    }
}

/// Returns the actor_id attributed to a given line, or None.
fn line_actor(blame: &Value, line_no: u64) -> Option<String> {
    blame["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["line_no"] == json!(line_no))
        .and_then(|l| l["actor_id"].as_str().map(str::to_string))
}

fn line_confidence(blame: &Value, line_no: u64) -> Option<String> {
    blame["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["line_no"] == json!(line_no))
        .and_then(|l| l["confidence"].as_str().map(str::to_string))
}

/// Real-workflow regression: after the agent's change is COMMITTED, blame must
/// still attribute the agent's lines — now via the commit → per-file patch-id
/// path (confidence "commit"), not the stale working overlay. Guards the bug
/// where post-commit attribution was silently lost.
#[test]
fn blame_survives_commit_via_per_file_patch_id() {
    let w = blame_world(
        "blame-commit",
        &[("src/main.rs", "fn main() {\n    let x = 1;\n}\n")],
    );
    std::fs::write(
        w.repo.join("src/main.rs"),
        "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
    )
    .unwrap();
    w.capture_working();
    assert!(git(&w.repo, &["add", "-A"]).success());
    assert!(git(&w.repo, &["commit", "-qm", "add y"]).success());

    let blame = w.blame("src/main.rs");
    for n in [3u64, 4] {
        assert_eq!(
            line_actor(&blame, n).as_deref(),
            Some("codex-bot"),
            "line {n}: {blame}"
        );
        assert_eq!(
            line_confidence(&blame, n).as_deref(),
            Some("commit"),
            "line {n}: {blame}"
        );
    }
    assert_eq!(blame["owner_lines"], json!(2), "{blame}");
    let _ = std::fs::remove_dir_all(&w.root);
}

/// Real-workflow regression: a LATER unrelated edit inserts a line above the
/// agent's committed change, shifting its line numbers. Blame must follow the
/// drift (git blame maps current lines to commits) and must NOT mis-attribute
/// the inserted line or the shifted-but-unrelated lines. Guards the stale
/// working-hunk-coordinate bug found in live testing.
#[test]
fn blame_follows_line_drift_after_later_edit() {
    let w = blame_world(
        "blame-drift",
        &[("src/main.rs", "fn main() {\n    let x = 1;\n}\n")],
    );
    std::fs::write(
        w.repo.join("src/main.rs"),
        "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
    )
    .unwrap();
    w.capture_working();
    assert!(git(&w.repo, &["add", "-A"]).success());
    assert!(git(&w.repo, &["commit", "-qm", "A: add y"]).success());

    // A different later commit inserts a comment near the top → agent's lines
    // shift from 3,4 down to 4,5.
    std::fs::write(
        w.repo.join("src/main.rs"),
        "fn main() {\n    // inserted later\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
    )
    .unwrap();
    assert!(git(&w.repo, &["add", "-A"]).success());
    assert!(git(&w.repo, &["commit", "-qm", "B: insert comment"]).success());

    let blame = w.blame("src/main.rs");
    // Agent's real lines are now 4 and 5.
    assert_eq!(
        line_actor(&blame, 4).as_deref(),
        Some("codex-bot"),
        "drifted line 4: {blame}"
    );
    assert_eq!(
        line_actor(&blame, 5).as_deref(),
        Some("codex-bot"),
        "drifted line 5: {blame}"
    );
    // The inserted comment (line 2) and the original code must NOT be attributed.
    assert!(
        line_actor(&blame, 2).is_none(),
        "inserted comment must be unattributed: {blame}"
    );
    assert!(
        line_actor(&blame, 3).is_none(),
        "shifted `let x` must be unattributed: {blame}"
    );
    assert_eq!(blame["owner_lines"], json!(2), "{blame}");
    let _ = std::fs::remove_dir_all(&w.root);
}

/// Real-workflow regression: the agent edits TWO files and lands them in ONE
/// multi-file commit (plus brick's own .gitignore). Per-file patch-id must
/// attribute each file independently — the whole-commit patch-id matches
/// neither file's captured slice. Guards the multi-file-commit bug.
#[test]
fn blame_attributes_each_file_in_a_multi_file_commit() {
    let w = blame_world(
        "blame-multi",
        &[("src/a.rs", "fn a() {\n}\n"), ("src/b.rs", "fn b() {\n}\n")],
    );
    std::fs::write(
        w.repo.join("src/a.rs"),
        "fn a() {\n    let ax = 1;\n    let ay = 2;\n}\n",
    )
    .unwrap();
    std::fs::write(w.repo.join("src/b.rs"), "fn b() {\n    let bx = 9;\n}\n").unwrap();
    w.capture_working();
    assert!(git(&w.repo, &["add", "-A"]).success());
    assert!(git(&w.repo, &["commit", "-qm", "feat: both files"]).success());

    let blame_a = w.blame("src/a.rs");
    assert_eq!(
        line_actor(&blame_a, 2).as_deref(),
        Some("codex-bot"),
        "a.rs L2: {blame_a}"
    );
    assert_eq!(
        line_actor(&blame_a, 3).as_deref(),
        Some("codex-bot"),
        "a.rs L3: {blame_a}"
    );
    assert_eq!(blame_a["owner_lines"], json!(2), "a.rs: {blame_a}");

    let blame_b = w.blame("src/b.rs");
    assert_eq!(
        line_actor(&blame_b, 2).as_deref(),
        Some("codex-bot"),
        "b.rs L2: {blame_b}"
    );
    assert_eq!(blame_b["owner_lines"], json!(1), "b.rs: {blame_b}");
    let _ = std::fs::remove_dir_all(&w.root);
}

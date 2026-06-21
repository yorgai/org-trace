//! End-to-end integration test for the MCP surface after the CTP reshape.
//!
//! Spawns the real `brick mcp-serve` binary (built by cargo as
//! `CARGO_BIN_EXE_brick`) and drives it over the real stdio JSON-RPC protocol —
//! the exact surface a Claude Code / Codex / ORGII MCP client speaks.
//!
//! The main coding-agent surface is exactly two tools: `explain` (read WHY,
//! subsumes blame's WHO) and `link` (write a causal edge). Planning tools live
//! behind `mcp-serve --planning` for a dedicated planning agent. The nine former
//! query/coordination tools are retired and return an actionable migration hint.
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

    /// Calls a tool and returns its parsed JSON payload (the text content block).
    /// A tool-level failure (MCP `isError=true`, plain-text message) surfaces as
    /// `{"_rpc_error": <message>}`; a transport error surfaces the rpc error.
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

/// Spins up two configured native source profiles under one `BRICK_HOME` and a
/// temp git repo. Returns `(root, home, repo, codex_dir, claude_dir)`.
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
            "source", "configure", "--name", "codex_app", "--app-id", "codex_app", "--actor-id",
            "codex-agent", "--actor-type", "agent", "--session-log-path",
            codex_dir.to_str().unwrap(),
        ],
    );
    brick(
        &home,
        &repo,
        &[
            "source", "configure", "--name", "claude_code", "--app-id", "claude_code",
            "--actor-id", "claude-agent", "--actor-type", "agent", "--session-log-path",
            claude_dir.to_str().unwrap(),
        ],
    );
    (root, home, repo, codex_dir, claude_dir)
}

/// Runs a git subcommand in `repo`, returning its exit status.
fn git(repo: &Path, args: &[&str]) -> std::process::ExitStatus {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("run git")
}

// ---------------------------------------------------------------------------
// Surface shape: explain + link only; planning behind a flag; retired hints.
// ---------------------------------------------------------------------------

#[test]
fn main_surface_is_explain_and_link_only() {
    let (root, home, repo, _codex_dir, _claude_dir) = setup_world("surface");
    let mut m = Mcp::spawn(&home, &repo);

    let mut tools = m.tool_names();
    tools.sort();
    assert_eq!(
        tools,
        vec!["explain".to_string(), "link".to_string()],
        "main surface must be exactly explain + link; got {tools:?}"
    );

    // Every retired tool name returns an actionable migration hint, not a bare
    // unknown-tool failure.
    for retired in [
        "log_file",
        "recall_file",
        "blame",
        "blame_file",
        "log_line",
        "search",
        "show_session",
        "sessions",
        "live_sessions",
        "claim",
        "claims",
        "status",
        "mission",
        "mission_list",
        "artifact_add",
    ] {
        let resp = m.call(retired, json!({}));
        assert_eq!(
            resp.get("error").and_then(Value::as_str),
            Some("tool_retired"),
            "retired tool {retired} should report tool_retired: {resp}"
        );
        assert!(
            resp.get("hint").and_then(Value::as_str).is_some(),
            "retired tool {retired} should carry a migration hint: {resp}"
        );
    }

    // A genuinely unknown tool is a hard RPC error, not a retired hint.
    let unknown = m.call("totally_made_up_tool", json!({}));
    assert!(
        unknown.get("_rpc_error").is_some(),
        "unknown tool should hard-error: {unknown}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn planning_surface_exposes_planning_tools() {
    let (root, home, repo, _codex_dir, _claude_dir) = setup_world("planning-surface");
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
        ],
        "planning surface tools mismatch: {tools:?}"
    );
    // explain/link are NOT on the planning surface.
    assert!(!tools.contains(&"explain".to_string()));
    assert!(!tools.contains(&"link".to_string()));

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Planning loop over the planning surface (mission → artifact → attach).
// ---------------------------------------------------------------------------

#[test]
fn planning_loop_mission_artifact_attach() {
    let (root, home, repo, _codex_dir, _claude_dir) = setup_world("planning-loop");
    let org = extract(&brick(&home, &repo, &["org", "create", "O"]), "org_id").expect("org");
    let project = extract(
        &brick(&home, &repo, &["project", "create", "--org", &org, "P"]),
        "project_id",
    )
    .expect("project");

    let mut m = Mcp::spawn_planning(&home, &repo);

    let created = m.call(
        "mission",
        json!({"action":"create","project":project,"title":"Cache git status","status":"active","source":"codex_app"}),
    );
    assert_eq!(created["created"], json!(true), "{created}");
    let mid = created["mission_id"].as_str().expect("mission_id").to_string();

    let listed = m.call("mission_list", json!({"status":"active"}));
    assert!(
        listed["missions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["title"] == "Cache git status"),
        "mission not listed: {listed}"
    );

    let art = m.call(
        "artifact_add",
        json!({"title":"PR: cache","kind":"patch","mission":mid,"source":"codex_app"}),
    );
    assert_eq!(art["recorded"], json!(true), "{art}");
    let aid = art["artifact_id"].as_str().expect("artifact_id").to_string();

    let attached = m.call(
        "artifact_attach",
        json!({"artifact":aid,"path":"src/commands_git.rs","source":"codex_app"}),
    );
    assert_eq!(attached["attached"], json!(true), "{attached}");

    let shown = m.call("show_mission", json!({"mission":mid}));
    assert!(
        shown["artifact_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == &json!(aid)),
        "artifact not under mission: {shown}"
    );

    // Planning surface refuses an explain call (wrong surface).
    let wrong = m.call("explain", json!({"anchor":"src/commands_git.rs:1"}));
    assert!(
        wrong.get("_rpc_error").is_some(),
        "explain must not exist on the planning surface: {wrong}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

/// Regression from live ORGII testing: MCP clients spawn the stdio server with
/// `cwd=/`. Planning records (missions / artifacts) have no path anchor, so
/// unlike explain/link they can't recover a repo from their arguments — at
/// `cwd=/` the cwd-derived store pointed at an unwritable root and every
/// `mission`/`artifact_add` crashed on `init()` ("failed to create provenance
/// queue directory"). The fix falls back to a BRICK_HOME-rooted store. This
/// spawns the planning server with an unrelated cwd and asserts create + list +
/// show all work (write/read land in the same fallback store).
#[test]
fn planning_survives_unrelated_cwd_via_brick_home_fallback() {
    let (root, home, repo, _codex_dir, _claude_dir) = setup_world("planning-cwd-robust");
    let org = extract(&brick(&home, &repo, &["org", "create", "O"]), "org_id").expect("org");
    let project = extract(
        &brick(&home, &repo, &["project", "create", "--org", &org, "P"]),
        "project_id",
    )
    .expect("project");

    // A non-repo directory standing in for the `cwd=/` an MCP client would use.
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let mut m = Mcp::spawn_planning(&home, &elsewhere);

    let created = m.call(
        "mission",
        json!({"action":"create","project":project,"title":"Survives cwd=/","status":"active","source":"codex_app"}),
    );
    assert_eq!(
        created["created"],
        json!(true),
        "mission create must not crash at unrelated cwd: {created}"
    );
    let mid = created["mission_id"].as_str().expect("mission_id").to_string();

    // Write/read land in the same fallback store — the mission lists back.
    let listed = m.call("mission_list", json!({"status":"active"}));
    assert!(
        listed["missions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["title"] == "Survives cwd=/"),
        "mission written at unrelated cwd must list back: {listed}"
    );

    let art = m.call(
        "artifact_add",
        json!({"title":"PR: x","kind":"patch","mission":mid,"source":"codex_app"}),
    );
    assert_eq!(
        art["recorded"],
        json!(true),
        "artifact_add must not crash at unrelated cwd: {art}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// explain end-to-end: file:line → blame → causal chain with WHO + WHY.
// ---------------------------------------------------------------------------

/// Bootstraps a git repo + brick home + an active agent mission/session/artifact
/// for explain/blame tests. Returns a handle that can capture diffs and explain.
struct World {
    root: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    session: String,
    mission: String,
    artifact: String,
}

fn world(tag: &str, seed_files: &[(&str, &str)]) -> World {
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
                "--actor-type", "agent", "--actor-id", "codex-bot", "mission", "create",
                "--project", &project, "m", "--status", "active",
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
                "--actor-type", "agent", "--actor-id", "codex-bot", "session", "start",
                "--mission", &mission, "--name", "s1",
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
                "--actor-type", "agent", "--actor-id", "codex-bot", "artifact", "create",
                "--mission", &mission, "--kind", "patch", "p",
            ],
        ),
        "artifact_id",
    )
    .expect("artifact");
    World {
        root,
        home,
        repo,
        session,
        mission,
        artifact,
    }
}

impl World {
    /// Captures a working diff for the whole tree, bound to the agent session.
    fn capture_working(&self) {
        let out = brick(
            &self.home,
            &self.repo,
            &[
                "--actor-type", "agent", "--actor-id", "codex-bot", "evidence", "diff",
                "--artifact", &self.artifact, "--session", &self.session, "--mission",
                &self.mission, "--target", "working",
            ],
        );
        assert!(
            out.status.success(),
            "capture failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn explain(&self, anchor: &str) -> Value {
        let mut m = Mcp::spawn(&self.home, &self.repo);
        let v = m.call("explain", json!({ "anchor": anchor }));
        drop(m);
        v
    }
}

/// Returns the first step in an explain chain attributed to a given actor.
fn step_for_actor<'a>(chain: &'a Value, actor: &str) -> Option<&'a Value> {
    chain["causal_chain"]
        .as_array()?
        .iter()
        .find(|step| step["actor_id"].as_str() == Some(actor))
}

#[test]
fn explain_file_line_resolves_who_via_blame() {
    let w = world(
        "explain-who",
        &[("src/main.rs", "fn main() {\n    let x = 1;\n}\n")],
    );
    // Agent adds lines 3 and 4, captures a working diff bound to its session.
    std::fs::write(
        w.repo.join("src/main.rs"),
        "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
    )
    .unwrap();
    w.capture_working();

    // explain on the agent's added line resolves through blame to its session.
    let chain = w.explain("src/main.rs:3");
    assert!(
        !chain["anchor"]["resolved_events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "anchor must resolve to an event: {chain}"
    );
    assert_eq!(
        chain["anchor"]["blame_confidence"].as_str(),
        Some("working"),
        "uncommitted change → working confidence: {chain}"
    );
    let step = step_for_actor(&chain, "codex-bot")
        .unwrap_or_else(|| panic!("no step for codex-bot: {chain}"));
    assert_eq!(step["session_id"].as_str(), Some(w.session.as_str()), "{step}");
    assert_eq!(step["mission_id"].as_str(), Some(w.mission.as_str()), "{step}");

    let _ = std::fs::remove_dir_all(&w.root);
}

/// Regression from live ORGII testing: MCP clients (Claude Code, Codex, ORGII)
/// spawn the stdio server with `cwd=/` — NOT the agent's workspace. The store is
/// otherwise derived from process cwd, so `cwd=/` made every `explain` crash on
/// `init()` trying to create `/.brick/queue` ("failed to create provenance queue
/// directory"). The fix resolves the store from an absolute-path anchor instead.
/// This spawns the server with a non-repo cwd and asserts an ABSOLUTE anchor
/// still recovers the WHO/WHY, while a RELATIVE anchor degrades to a clean note
/// (never a crash).
#[test]
fn explain_resolves_store_from_absolute_anchor_when_cwd_is_unrelated() {
    let w = world(
        "explain-cwd-robust",
        &[("src/main.rs", "fn main() {\n    let x = 1;\n}\n")],
    );
    std::fs::write(
        w.repo.join("src/main.rs"),
        "fn main() {\n    let x = 1;\n    let y = 2;\n}\n",
    )
    .unwrap();
    w.capture_working();

    // A non-repo directory standing in for the `cwd=/` an MCP client would use.
    let elsewhere = w.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let abs_anchor = format!("{}/src/main.rs", w.repo.display());
    let mut m = Mcp::spawn(&w.home, &elsewhere);

    // Absolute whole-file anchor → store resolved from the anchor's repo, the
    // file's change events (and their WHO) recovered despite the unrelated cwd.
    let chain = m.call("explain", json!({ "anchor": abs_anchor }));
    assert!(
        !chain["anchor"]["resolved_events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "absolute anchor must resolve despite unrelated cwd: {chain}"
    );
    assert!(
        step_for_actor(&chain, "codex-bot").is_some(),
        "absolute anchor must recover the WHO despite unrelated cwd: {chain}"
    );

    // Relative anchor → no repo from this cwd → clean note, NOT a crash.
    let rel = m.call("explain", json!({ "anchor": "src/main.rs:3" }));
    assert!(
        rel.get("_rpc_error").is_none(),
        "relative anchor with unrelated cwd must not hard-error: {rel}"
    );
    assert!(
        rel["causal_chain"].as_array().map(|a| a.is_empty()).unwrap_or(false),
        "relative anchor with no repo resolves to an empty chain: {rel}"
    );
    assert!(
        rel["note"].as_str().unwrap_or_default().contains("No Brick repo resolved"),
        "relative anchor with no repo must carry the actionable note: {rel}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&w.root);
}

#[test]
fn explain_whole_file_anchor_resolves_without_no_record() {
    let w = world(
        "explain-wholefile",
        &[("src/main.rs", "fn main() {\n    let x = 1;\n}\n")],
    );
    std::fs::write(
        w.repo.join("src/main.rs"),
        "fn main() {\n    let x = 1;\n    let y = 2;\n}\n",
    )
    .unwrap();
    w.capture_working();

    // Whole-file anchor: no line number.
    let chain = w.explain("src/main.rs");
    assert_eq!(
        chain["anchor"]["kind"].as_str(),
        Some("file"),
        "whole-file anchor must resolve as kind=file: {chain}"
    );
    assert!(
        !chain["anchor"]["resolved_events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "whole-file anchor must resolve to the file's change events: {chain}"
    );
    assert!(
        chain.get("note").is_none()
            || !chain["note"]
                .as_str()
                .unwrap_or_default()
                .contains("No Brick record"),
        "a tracked file must NOT report 'No Brick record': {chain}"
    );
    let step = step_for_actor(&chain, "codex-bot")
        .unwrap_or_else(|| panic!("whole-file chain missing codex-bot step: {chain}"));
    assert_eq!(
        step["mission_title"].as_str(),
        Some("m"),
        "step must carry the human mission_title: {step}"
    );

    let _ = std::fs::remove_dir_all(&w.root);
}

#[test]
fn explain_survives_commit_via_per_file_patch_id() {
    let w = world(
        "explain-commit",
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

    let chain = w.explain("src/main.rs:3");
    assert_eq!(
        chain["anchor"]["blame_confidence"].as_str(),
        Some("commit"),
        "committed change → commit confidence: {chain}"
    );
    let step = step_for_actor(&chain, "codex-bot")
        .unwrap_or_else(|| panic!("no codex-bot step after commit: {chain}"));
    assert_eq!(step["session_id"].as_str(), Some(w.session.as_str()), "{step}");

    let _ = std::fs::remove_dir_all(&w.root);
}

#[test]
fn explain_follows_line_drift_after_later_edit() {
    let w = world(
        "explain-drift",
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

    // A later unrelated commit inserts a comment, shifting the agent's lines down.
    std::fs::write(
        w.repo.join("src/main.rs"),
        "fn main() {\n    // inserted later\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
    )
    .unwrap();
    assert!(git(&w.repo, &["add", "-A"]).success());
    assert!(git(&w.repo, &["commit", "-qm", "B: insert comment"]).success());

    // The agent's `let y` is now line 4; explain follows the drift to its session.
    let drifted = w.explain("src/main.rs:4");
    assert!(
        step_for_actor(&drifted, "codex-bot").is_some(),
        "drifted line 4 must still attribute to the agent: {drifted}"
    );
    // The inserted comment (line 2) is not the agent's → no attribution.
    let comment = w.explain("src/main.rs:2");
    assert!(
        comment["anchor"]["resolved_events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "inserted comment must not resolve to an agent event: {comment}"
    );

    let _ = std::fs::remove_dir_all(&w.root);
}

// ---------------------------------------------------------------------------
// link: write a causal edge; explain reads the WHY back.
// ---------------------------------------------------------------------------

#[test]
fn link_rationale_then_explain_reads_the_why() {
    let w = world(
        "link-rationale",
        &[("src/auth.rs", "fn refresh() {\n    token();\n}\n")],
    );
    std::fs::write(
        w.repo.join("src/auth.rs"),
        "fn refresh() {\n    lock();\n    token();\n}\n",
    )
    .unwrap();
    w.capture_working();

    let mut m = Mcp::spawn(&w.home, &w.repo);
    // Standalone rationale bound to the agent's most recent diff (no effect arg).
    let linked = m.call(
        "link",
        json!({"relation":"rationale","note":"token refresh had a concurrency race; serialized it","source":"codex_app"}),
    );
    assert_eq!(linked["linked"], json!(true), "{linked}");
    assert_eq!(linked["relation"], json!("rationale"), "{linked}");

    // explain the changed line and recover the WHY.
    let chain = m.call("explain", json!({"anchor":"src/auth.rs:2"}));
    let has_why = chain["causal_chain"]
        .as_array()
        .unwrap()
        .iter()
        .any(|step| {
            step["note"]
                .as_str()
                .map(|note| note.contains("concurrency race"))
                .unwrap_or(false)
        });
    assert!(has_why, "explain must surface the linked rationale: {chain}");

    drop(m);
    let _ = std::fs::remove_dir_all(&w.root);
}

/// Regression from live Claude testing: an agent edits a file with its own
/// tools (no Brick diff event), then calls `link` with no `effect`. The
/// rationale must bind to the file it actually changed — `link` auto-captures
/// the working diff — NOT mis-attribute to some unrelated prior diff. This is
/// the bug where a cache.rs rationale landed on auth.rs's event.
#[test]
fn link_without_effect_auto_captures_the_edited_file() {
    let w = world(
        "link-autocapture",
        &[
            ("src/auth.rs", "fn refresh() {\n    token();\n}\n"),
            ("src/cache.rs", "fn get() {}\n"),
        ],
    );
    // A prior, unrelated diff exists on auth.rs (committed) — the old code would
    // wrongly bind a new rationale to THIS.
    std::fs::write(
        w.repo.join("src/auth.rs"),
        "fn refresh() {\n    lock();\n    token();\n}\n",
    )
    .unwrap();
    w.capture_working();
    assert!(git(&w.repo, &["add", "-A"]).success());
    assert!(git(&w.repo, &["commit", "-q", "-m", "auth"]).success());

    // Now the agent edits cache.rs with its own tools — no Brick event for it.
    std::fs::write(
        w.repo.join("src/cache.rs"),
        "fn get() -> Option<String> {\n    None\n}\n",
    )
    .unwrap();

    let mut m = Mcp::spawn(&w.home, &w.repo);
    let linked = m.call(
        "link",
        json!({"relation":"rationale","note":"get() now expires stale entries via a TTL check","source":"claude_code"}),
    );
    assert_eq!(linked["linked"], json!(true), "{linked}");
    assert_eq!(
        linked["captured_files"],
        json!(["src/cache.rs"]),
        "link must auto-capture the edited file, not an unrelated diff: {linked}"
    );

    // The rationale must be readable from cache.rs...
    let cache_chain = m.call("explain", json!({"anchor":"src/cache.rs"}));
    let cache_has_why = cache_chain["causal_chain"]
        .as_array()
        .unwrap()
        .iter()
        .any(|step| {
            step["note"]
                .as_str()
                .map(|note| note.contains("TTL check"))
                .unwrap_or(false)
        });
    assert!(
        cache_has_why,
        "cache.rs must carry its own rationale: {cache_chain}"
    );

    // ...and must NOT have leaked onto auth.rs.
    let auth_chain = m.call("explain", json!({"anchor":"src/auth.rs"}));
    let auth_polluted = auth_chain["causal_chain"]
        .as_array()
        .unwrap()
        .iter()
        .any(|step| {
            step["note"]
                .as_str()
                .map(|note| note.contains("TTL check"))
                .unwrap_or(false)
        });
    assert!(
        !auth_polluted,
        "cache.rs rationale must not pollute auth.rs: {auth_chain}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&w.root);
}

/// Regression from live multi-hop chain testing: an agent edits a file and
/// `git add`s it (stages it) BEFORE calling `link` with no `effect`. A plain
/// `git diff` (unstaged) shows nothing for a staged file, so the old capture
/// returned empty and the rationale mis-bound to a stale prior diff — every hop
/// of a chain collapsed onto one event. Auto-capture must fold in staged changes
/// so the reason binds to the file actually changed.
#[test]
fn link_auto_capture_includes_staged_changes() {
    let w = world(
        "link-staged-capture",
        &[
            ("src/old.rs", "fn old() {}\n"),
            ("src/cache.rs", "fn get() {}\n"),
        ],
    );
    // A prior committed diff on old.rs is the stale event the bug would grab.
    std::fs::write(w.repo.join("src/old.rs"), "fn old() { 1 }\n").unwrap();
    w.capture_working();
    assert!(git(&w.repo, &["add", "-A"]).success());
    assert!(git(&w.repo, &["commit", "-q", "-m", "old"]).success());

    // Agent edits cache.rs and STAGES it before linking.
    std::fs::write(
        w.repo.join("src/cache.rs"),
        "fn get() -> Option<String> {\n    None\n}\n",
    )
    .unwrap();
    assert!(git(&w.repo, &["add", "src/cache.rs"]).success());

    let mut m = Mcp::spawn(&w.home, &w.repo);
    let linked = m.call(
        "link",
        json!({"relation":"rationale","note":"staged TTL fix in get()","source":"codex_app"}),
    );
    assert_eq!(linked["linked"], json!(true), "{linked}");
    assert_eq!(
        linked["captured_files"],
        json!(["src/cache.rs"]),
        "staged change must be captured, not missed: {linked}"
    );

    let chain = m.call("explain", json!({"anchor":"src/cache.rs"}));
    let has_why = chain["causal_chain"]
        .as_array()
        .unwrap()
        .iter()
        .any(|step| {
            step["note"]
                .as_str()
                .map(|note| note.contains("staged TTL fix"))
                .unwrap_or(false)
        });
    assert!(has_why, "staged rationale must bind to cache.rs: {chain}");

    drop(m);
    let _ = std::fs::remove_dir_all(&w.root);
}

/// Regression from live cross-scenario testing (planning → code bridge): a
/// coding agent implements a planned mission, then links its code change to that
/// mission by passing the `mission_…` id as `cause`. The edge MUST resolve to
/// the mission's event (not be dropped), so `explain mission_…` later traverses
/// from the work item down to the real code. The live failure was the agent
/// stuffing the mission id into `note` text instead of `cause`, leaving the
/// graph disconnected — this pins that `cause=mission` actually wires up.
#[test]
fn link_cause_mission_connects_planning_to_code() {
    let w = world(
        "link-mission-cause",
        &[("src/cache.rs", "fn get() {}\n")],
    );
    // Agent edits the file implementing the mission.
    std::fs::write(
        w.repo.join("src/cache.rs"),
        "fn get() -> Option<String> {\n    None\n}\n",
    )
    .unwrap();

    let mut m = Mcp::spawn(&w.home, &w.repo);
    let linked = m.call(
        "link",
        json!({
            "cause": w.mission,
            "relation": "derived_from",
            "note": "implemented cache TTL expiry for the planned mission",
            "source": "claude_code",
        }),
    );
    assert_eq!(linked["linked"], json!(true), "{linked}");
    assert!(
        !linked["cause_events"].as_array().unwrap().is_empty(),
        "cause=mission must resolve to a real event, not be dropped: {linked}"
    );
    assert_eq!(linked["relation"], json!("derived_from"), "{linked}");

    // explain the mission and confirm the forward edge reaches the code change.
    let chain = m.call("explain", json!({"anchor": w.mission}));
    let reaches_code = chain["forward"]
        .as_array()
        .map(|fs| {
            fs.iter().any(|f| {
                f["what"]
                    .as_str()
                    .map(|w| w.contains("cache.rs"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    assert!(
        reaches_code,
        "explain(mission) must reach the linked code change as a forward effect: {chain}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&w.root);
}

/// Regression from live Codex testing: an agent edits a file then calls `link`
/// with `effect: "src/cache.rs:1"` — a perfectly reasonable anchor on the line
/// it just changed. That line has no Brick event yet (the edit is uncommitted),
/// so the blame-based resolver finds nothing. `link` must NOT hard-error; it
/// should fall back to capturing the working diff, exactly like the no-effect
/// path. (Codex recovered by retrying without effect, but a lesser agent fails.)
#[test]
fn link_with_unresolvable_file_effect_falls_back_to_working_capture() {
    let w = world(
        "link-effect-fallback",
        &[("src/cache.rs", "fn get() {}\n")],
    );
    // Agent edits cache.rs with its own tools; no Brick event exists for it.
    std::fs::write(
        w.repo.join("src/cache.rs"),
        "fn get() -> Option<String> {\n    None\n}\n",
    )
    .unwrap();

    let mut m = Mcp::spawn(&w.home, &w.repo);
    let linked = m.call(
        "link",
        json!({"effect":"src/cache.rs:1","relation":"rationale","note":"get() now enforces TTL expiry","source":"codex_app"}),
    );
    assert_eq!(
        linked["linked"],
        json!(true),
        "a file:line effect with no event must fall back, not error: {linked}"
    );
    assert_eq!(
        linked["captured_files"],
        json!(["src/cache.rs"]),
        "fallback must capture the edited file: {linked}"
    );

    let chain = m.call("explain", json!({"anchor":"src/cache.rs"}));
    let has_why = chain["causal_chain"]
        .as_array()
        .unwrap()
        .iter()
        .any(|step| {
            step["note"]
                .as_str()
                .map(|note| note.contains("TTL expiry"))
                .unwrap_or(false)
        });
    assert!(has_why, "rationale must be recoverable: {chain}");

    drop(m);
    let _ = std::fs::remove_dir_all(&w.root);
}

#[test]
fn link_cross_event_edge_shows_as_forward_effect() {
    let w = world(
        "link-edge",
        &[
            ("src/auth.rs", "fn refresh() {\n    token();\n}\n"),
            ("src/test_auth.rs", "fn t() {}\n"),
        ],
    );
    // First change: auth.rs (the cause), captured + committed so it has a stable id.
    std::fs::write(
        w.repo.join("src/auth.rs"),
        "fn refresh() {\n    lock();\n    token();\n}\n",
    )
    .unwrap();
    w.capture_working();
    assert!(git(&w.repo, &["add", "-A"]).success());
    assert!(git(&w.repo, &["commit", "-qm", "fix auth"]).success());

    // Second change: the test, captured.
    std::fs::write(w.repo.join("src/test_auth.rs"), "fn t() {\n    refresh();\n}\n").unwrap();
    w.capture_working();

    let mut m = Mcp::spawn(&w.home, &w.repo);
    // The test (effect) is derived_from the auth change (cause).
    let linked = m.call(
        "link",
        json!({
            "effect":"src/test_auth.rs:2",
            "cause":"src/auth.rs:2",
            "relation":"derived_from",
            "note":"covers the race fix",
            "source":"codex_app"
        }),
    );
    assert_eq!(linked["linked"], json!(true), "{linked}");
    assert_eq!(linked["relation"], json!("derived_from"), "{linked}");

    // explain the auth change → the test shows up as a forward effect.
    let chain = m.call("explain", json!({"anchor":"src/auth.rs:2"}));
    let forward = chain["forward"].as_array().cloned().unwrap_or_default();
    assert!(
        forward
            .iter()
            .any(|effect| effect["relation_to_anchor"].as_str() == Some("derived_from")),
        "auth change should have a derived_from forward effect: {chain}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&w.root);
}

#[test]
fn link_rejects_information_free_edge() {
    let w = world("link-empty", &[("src/a.rs", "fn a() {}\n")]);
    std::fs::write(w.repo.join("src/a.rs"), "fn a() {\n    b();\n}\n").unwrap();
    w.capture_working();

    let mut m = Mcp::spawn(&w.home, &w.repo);
    // No cause and no note → must be rejected as a hard error.
    let resp = m.call("link", json!({"relation":"rationale","source":"codex_app"}));
    assert!(
        resp.get("_rpc_error").is_some(),
        "link with neither cause nor note must error: {resp}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&w.root);
}

// ---------------------------------------------------------------------------
// explain enrichment: live coordination + empty-record honesty.
// ---------------------------------------------------------------------------

#[test]
fn explain_surfaces_live_session_on_anchor_file() {
    let (root, home, repo, codex_dir, _claude_dir) = setup_world("explain-live");
    // A live Codex session is editing commands_git.rs right now.
    write_codex(&codex_dir, "codex-live-001", &repo, "src/commands_git.rs");

    let mut m = Mcp::spawn(&home, &repo);
    // Anchor on that file; even with no causal record, the live field fires.
    let chain = m.call("explain", json!({"anchor":"src/commands_git.rs:1"}));
    assert!(
        chain.get("live").is_some(),
        "explain must surface a live session editing the anchor file: {chain}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn explain_is_honest_when_no_record_exists() {
    let (root, home, repo, _codex_dir, _claude_dir) = setup_world("explain-empty");
    let mut m = Mcp::spawn(&home, &repo);
    // A file with no captured Brick history at all.
    let chain = m.call("explain", json!({"anchor":"src/commands_memory.rs:1"}));
    assert!(
        chain["anchor"]["resolved_events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "no record → no resolved events: {chain}"
    );
    assert!(
        chain
            .get("note")
            .and_then(Value::as_str)
            .map(|note| note.to_lowercase().contains("no brick record"))
            .unwrap_or(false),
        "explain must say so honestly when there is no record: {chain}"
    );
    // It does NOT fabricate a chain.
    assert!(
        chain["causal_chain"].as_array().map(|c| c.is_empty()).unwrap_or(true),
        "no record → empty chain, never guessed: {chain}"
    );

    drop(m);
    let _ = std::fs::remove_dir_all(&root);
}

/// The finished Claude transcript helper is retained for parity with the source
/// fixtures, exercised by the live test's negative space.
#[allow(dead_code)]
fn finished_claude(dir: &Path, sid: &str, repo: &Path, file: &str) {
    write_claude(dir, sid, repo, file);
}

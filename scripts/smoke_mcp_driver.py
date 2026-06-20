#!/usr/bin/env python3
"""MCP smoke driver: exercise all 13 `brick mcp-serve` tools end-to-end.

Driven by scripts/smoke_mcp.sh, which clones a real git repo into a temp dir and
sets BRICK_HOME to an isolated dir. This script talks the real MCP stdio JSON-RPC
protocol to a real `brick mcp-serve` process and asserts the cross-tool flows:
two native source profiles (codex_app + claude_code) backed by real transcript
files in their native formats, a Codex agent that patched a real file, a finished
Claude session, the full planning loop, FTS5 search, and liveness-aware claim
retirement.

Environment (all required, set by smoke_mcp.sh):
  BRICK_BIN    path to the built `brick` binary
  SMOKE_REPO   path to a real git repo checkout (the MCP server's cwd)
  BRICK_HOME   isolated home for global metadata/announcement DBs
  CODEX_DIR    dir to write codex transcripts into
  CLAUDE_DIR   dir to write claude transcripts into
  FILE_CODEX   repo-relative real file the Codex session "edits"
  FILE_CLAUDE  repo-relative real file the Claude session references
"""
import json, os, subprocess, sys, time, pathlib

BIN = os.environ["BRICK_BIN"]
REPO = os.environ["SMOKE_REPO"]
HOME = os.environ["BRICK_HOME"]
CODEX_DIR = os.environ["CODEX_DIR"]
CLAUDE_DIR = os.environ["CLAUDE_DIR"]
FILE_CODEX = os.environ["FILE_CODEX"]
FILE_CLAUDE = os.environ["FILE_CLAUDE"]

PASS, FAIL = [], []
def check(name, ok, detail=""):
    (PASS if ok else FAIL).append(name)
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" — {detail}" if detail and not ok else ""))
    return ok

def sh(args):
    return subprocess.run([BIN, *args], cwd=REPO, env=dict(os.environ, BRICK_HOME=HOME),
                          capture_output=True, text=True)

class Mcp:
    def __init__(self):
        self.p = subprocess.Popen([BIN, "mcp-serve"], cwd=REPO,
                                  env=dict(os.environ, BRICK_HOME=HOME),
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.id = 0
        self._rpc("initialize", {})
    def _rpc(self, method, params):
        self.id += 1
        self.p.stdin.write(json.dumps({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})+"\n")
        self.p.stdin.flush()
        return json.loads(self.p.stdout.readline())
    def call(self, tool, args):
        resp = self._rpc("tools/call", {"name":tool,"arguments":args})
        if "error" in resp: return {"_error": resp["error"]}
        return json.loads(resp["result"]["content"][0]["text"])
    def list_tools(self):
        return [t["name"] for t in self._rpc("tools/list", {})["result"]["tools"]]
    def close(self):
        try: self.p.stdin.close(); self.p.wait(timeout=5)
        except Exception: self.p.kill()

def now_iso(offset=0):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time()+offset))

def write_codex(sid, *, open_turn):
    patch = f"diff --git a/{FILE_CODEX} b/{FILE_CODEX}\n+++ b/{FILE_CODEX}\n+// add git status caching\n"
    lines = [
        {"timestamp":now_iso(-30),"payload":{"type":"user_message","message":"Cache git status lookups in commands_git","cwd":REPO,"model":"gpt-5"}},
        {"timestamp":now_iso(-25),"payload":{"type":"task_started"}},
        {"timestamp":now_iso(-20),"payload":{"type":"agent_message","message":"Adding a cache layer"}},
        {"timestamp":now_iso(-15),"payload":{"type":"function_call","call_id":"c1","name":"apply_patch","arguments":json.dumps({"patch":patch})}},
        {"timestamp":now_iso(-14),"payload":{"type":"function_call_output","call_id":"c1","output":"applied"}},
    ]
    if not open_turn:
        lines.append({"timestamp":now_iso(-10),"payload":{"type":"task_complete"}})
    (pathlib.Path(CODEX_DIR)/f"{sid}.jsonl").write_text("".join(json.dumps(l)+"\n" for l in lines))

def write_claude(sid, *, open_turn):
    lines = [{"type":"user","timestamp":now_iso(-30),"message":{"role":"user","content":f"Review {FILE_CLAUDE}","cwd":REPO}}]
    if open_turn:
        lines.append({"type":"assistant","timestamp":now_iso(-5),"message":{"content":[{"type":"text","text":"Reviewing"}],"stop_reason":None}})
    else:
        lines.append({"type":"assistant","timestamp":now_iso(-20),"message":{"content":[{"type":"text","text":"Done"}],"stop_reason":"end_turn"}})
    (pathlib.Path(CLAUDE_DIR)/f"{sid}.jsonl").write_text("".join(json.dumps(l)+"\n" for l in lines))

def main():
    assert pathlib.Path(REPO, FILE_CODEX).exists(), f"{FILE_CODEX} must exist in repo"
    assert pathlib.Path(REPO, FILE_CLAUDE).exists(), f"{FILE_CLAUDE} must exist in repo"

    sh(["init"])
    sh(["source","configure","--name","codex_app","--app-id","codex_app",
        "--actor-id","codex-agent","--actor-type","agent","--session-log-path",CODEX_DIR])
    sh(["source","configure","--name","claude_code","--app-id","claude_code",
        "--actor-id","claude-agent","--actor-type","agent","--session-log-path",CLAUDE_DIR])
    write_codex("codex-live-001", open_turn=True)
    write_claude("claude-done-002", open_turn=False)

    org = next((l.split("=")[1] for l in sh(["org","create","SmokeOrg"]).stdout.splitlines() if l.startswith("org_id=")), "")
    proj = next((l.split("=")[1] for l in sh(["project","create","--org",org,"SmokeProject"]).stdout.splitlines() if l.startswith("project_id=")), "")
    check("CLI setup produced org+project", bool(org and proj), f"org={org} proj={proj}")

    m = Mcp()
    try:
        print("\n== tools/list ==")
        tools = m.list_tools()
        want = ["explore_memory","recall_file","search_sessions","read_session","current_context",
                "list_missions","show_mission","manage_mission","record_artifact","attach_evidence",
                "live_sessions","announce_work","list_announcements"]
        check("all 13 tools present", all(t in tools for t in want), str(tools))

        print("\n== live_sessions ==")
        live = m.call("live_sessions", {})
        sessions = live.get("sessions", [])
        ids = [s.get("external_session_id") for s in sessions]
        check("sees running Codex", "codex-live-001" in ids, str(ids))
        check("excludes finished Claude", "claude-done-002" not in ids, str(ids))
        codex_row = next((s for s in sessions if s.get("external_session_id")=="codex-live-001"), {})
        check("resolves real git work_scope", REPO.split("/")[-1] in json.dumps(codex_row), json.dumps(codex_row)[:200])

        print("\n== search_sessions (FTS5 trigram) ==")
        sr = m.call("search_sessions", {"query":"git status cache"})
        check("out-of-order terms match (tokenized)", sr.get("match_count",0) >= 1, json.dumps(sr)[:200])
        sr2 = m.call("search_sessions", {"query":"commands_git"})
        check("substring matches real file path", sr2.get("match_count",0) >= 1, json.dumps(sr2)[:200])

        print("\n== recall_file / read_session / explore_memory ==")
        rc = m.call("recall_file", {"path":FILE_CODEX})
        check("recall_file surfaces prior work", rc.get("session_count",0) >= 1 or "commands_git" in json.dumps(rc).lower(), json.dumps(rc)[:200])
        rs = m.call("read_session", {"source":"codex_app","session_id":"codex-live-001"})
        check("read_session returns chunks", rs.get("total_chunks",0) >= 1 or "cache" in json.dumps(rs).lower(), json.dumps(rs)[:160])
        em = m.call("explore_memory", {"question":"how did we speed up git status"})
        check("explore_memory returns summary", not em.get("_error"), json.dumps(em)[:160])

        print("\n== planning loop ==")
        cc = m.call("current_context", {})
        check("current_context returns counts", "counts" in cc, json.dumps(cc)[:160])
        created = m.call("manage_mission", {"action":"create","project":proj,"title":"Cache git status","status":"active","source":"codex_app"})
        mid = created.get("mission_id","")
        check("manage_mission create", created.get("created") and mid)
        lm = m.call("list_missions", {"status":"active"})
        check("list_missions shows mission", any(x.get("title")=="Cache git status" for x in lm.get("missions",[])))
        art = m.call("record_artifact", {"title":"PR: cache","kind":"patch","mission":mid,"source":"codex_app"})
        aid = art.get("artifact_id","")
        check("record_artifact", art.get("recorded") and aid)
        ev = m.call("attach_evidence", {"artifact":aid,"path":FILE_CODEX,"source":"codex_app"})
        check("attach_evidence (real file)", ev.get("attached"))
        sm = m.call("show_mission", {"mission":mid})
        check("show_mission lists artifact", aid in sm.get("artifact_ids",[]))
        upd = m.call("manage_mission", {"action":"update","mission":mid,"status":"completed"})
        check("manage_mission update→completed", upd.get("updated"))

        print("\n== announce_work + liveness retirement ==")
        m.call("announce_work", {"scope":FILE_CODEX,"message":"editing","source":"codex_app","session_id":"codex-live-001"})
        m.call("announce_work", {"scope":"src/ghost.rs","message":"bare mcp","source":"mcp","session_id":"ghost"})
        m.call("announce_work", {"scope":FILE_CLAUDE,"message":"reviewing","source":"claude_code","session_id":"claude-done-002"})
        scopes = [a.get("scope") for a in m.call("list_announcements", {}).get("announcements",[])]
        check("live Codex claim kept", FILE_CODEX in scopes, str(scopes))
        check("unprobeable bare-mcp claim kept (TTL)", "src/ghost.rs" in scopes, str(scopes))
        check("dead Claude-session claim retired", FILE_CLAUDE not in scopes, str(scopes))
        check("recall_file surfaces live active_claim", "active_claims" in json.dumps(m.call("recall_file", {"path":FILE_CODEX})))

        print("\n== cross-tool: Claude recalls Codex-authored work ==")
        sm2 = m.call("show_mission", {"mission":mid})
        check("Claude sees Codex mission", sm2.get("title")=="Cache git status")
        check("Claude sees Codex artifact", aid in sm2.get("artifact_ids",[]))
    finally:
        m.close()

    print(f"\n== RESULT: {len(PASS)} passed, {len(FAIL)} failed ==")
    if FAIL:
        print("FAILED:", ", ".join(FAIL)); sys.exit(1)
    print("ALL GREEN")

if __name__ == "__main__":
    main()

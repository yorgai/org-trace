//! Agent Skill installation for `brick agent`.
//!
//! Claude Code, Codex, Cursor, Gemini CLI, Windsurf, and ORGII implement the
//! [Agent Skills] open standard: a `SKILL.md` file (YAML frontmatter + markdown
//! body) under a `skills/<name>/` directory. The frontmatter `description` is the
//! load-bearing discovery text, and the body is lazily loaded only when the agent
//! decides the skill applies.
//!
//! This is a strictly stronger nudge than the markdown memory block (CLAUDE.md /
//! AGENTS.md): that block is passive context the agent may skim past, whereas a
//! skill's description is decision-oriented routing metadata. We install the
//! Brick skill so that "investigating why code looks the way it does / what
//! caused a bug" reliably routes to `brick explain` instead of the agent
//! defaulting to grep + web search.
//!
//! Skills are always installed at per-user (global) locations so a single install
//! is visible across every project.
//!
//! [Agent Skills]: https://agentskills.io

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Bumped whenever `SKILL_BODY` or the frontmatter changes so `status` can report
/// an installed skill as stale and `install --force` can roll it forward.
const SKILL_VERSION: u32 = 4;

/// The skill directory name (becomes the `/brick` command in Claude Code) and the
/// frontmatter `name`. The directory name — not the frontmatter — drives skill
/// discovery in both clients.
const SKILL_NAME: &str = "brick";

/// A stable sentinel embedded as an HTML comment on the first body line so we can
/// detect a Brick-owned skill and its version without parsing YAML.
const SKILL_MARKER_PREFIX: &str = "<!-- brick:skill v=";

/// The full `SKILL.md` content. The `description` is the load-bearing field: both
/// clients surface it every turn, so it is written as "what + WHEN", front-loading
/// the trigger phrases (bug, issue, why, how did this happen) that should route to
/// `explain`.
///
/// IMPORTANT: `description` MUST be a single physical line. ORGII's skill scanner
/// parses frontmatter by hand (not a YAML library) — it reads everything after
/// `description:` on the same line and IGNORES continuation lines, so a YAML folded
/// scalar (`>-`) or block scalar would be parsed as the literal `">-"` and drop the
/// entire description. Keep it one line so both Claude Code (real YAML) and ORGII
/// (line parser) read it identically.
fn skill_md() -> String {
    format!(
        "---\n\
name: {SKILL_NAME}\n\
description: {DESCRIPTION}\n\
always: true\n\
---\n\
{SKILL_MARKER_PREFIX}{SKILL_VERSION} -->\n\
{}",
        SKILL_BODY.trim_start()
    )
}

/// Single-line skill description (see `skill_md` for why one line). Front-loads the
/// cause/history trigger phrases the agent routes on.
const DESCRIPTION: &str = "Recall WHY this codebase looks the way it does — the causal history across every AI tool that touched it. Use when investigating what caused a bug, explaining why some behavior or code exists, tracing what introduced a change, answering a \"how did this happen\" or \"why is this broken\" question, or reviewing code before you change it. Reach for this skill BEFORE grep, git log, reading files top-to-bottom, or web search — `brick explain` is the first move for any cause/history question.";

/// The skill body — the procedure the agent follows once it decides Brick applies.
/// Mirrors the `explain` workflow taught by the markdown block, written as
/// imperative steps.
const SKILL_BODY: &str = "\
# Brick — causal memory of this codebase

Brick answers WHY code looks the way it does, across every AI tool that has touched
this repo. It is the history layer git does not have: for any file it returns the
timeline of AI sessions that changed it, newest first, each with a transcript
pointer. Its agent surface is one MCP tool: `explain` (read WHY). It is read-only.

## When the task is about CAUSE or HISTORY

The moment a task is about why something is the way it is — what caused a bug, why a
behavior exists, what introduced a change, or understanding code before you touch it
— your FIRST move is `explain`, before grep, `git log`, reading files top-to-bottom,
or fetching the issue tracker:

```
brick explain <path>:<line>
brick explain <path>:<start>-<end>   # explain a whole block at once
```

(Or call the `explain` MCP tool.) The anchor can also be a whole file, an artifact,
a mission, or an event id. It returns a `causal_chain`: a newest-first timeline of
the sessions that touched the anchor — who changed it, WHEN, the `mission_title`
they did it under, and whether another session is editing the file right now. depth
0 is the most recent change; higher depth is older.

**What counts as a real Brick record.** A timeline step carrying an `actor_id`, a
`mission_title`, or `confidence: explicit` IS provenance — treat it as useful history
even when its `note` is null or shallow. If `causal_chain` is non-empty, Brick
succeeded: do NOT call it useless just because the top-level note says file-level
fallback or no exact line-level record. Only fall back to git/grep when `explain`
returns an empty `causal_chain` or an explicit \"No Brick record\" note.

**Follow `next_action` and go deeper than the `note`.** A step's `note` is only that
turn's CLOSING narration, which is often NOT the root cause. When `explain` returns
`next_action.kind = \"read_transcript\"`, run its bounded preview `command` before
concluding WHY; use `full_command` only when the preview is insufficient. If there
is no `next_action` but the note doesn't answer your question, run the step's
`transcript.read_session` command and read the original tool calls, errors, and
reasoning end-to-end. The real cause lives in that deep read.


## After you change code

Nothing to record: Brick recovers the WHY of your edits from the session transcript
automatically, so the next agent's `explain` will surface what you did and why.
";

/// Result of an install/uninstall/status operation on a skill file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillAction {
    Installed,
    Updated,
    Unchanged,
    Removed,
    Absent,
    Present,
    Stale,
}

impl SkillAction {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillAction::Installed => "installed",
            SkillAction::Updated => "updated",
            SkillAction::Unchanged => "unchanged",
            SkillAction::Removed => "removed",
            SkillAction::Absent => "absent",
            SkillAction::Present => "present",
            SkillAction::Stale => "stale",
        }
    }
}

/// A skill-capable client and where its per-user skills directory lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillClient {
    Claude,
    Codex,
    Cursor,
    Gemini,
    Orgii,
    Windsurf,
}

impl SkillClient {
    /// The reporting label, matching the `target_label` convention in `agent.rs`.
    pub fn label(self) -> &'static str {
        match self {
            SkillClient::Claude => "claude_skill",
            SkillClient::Codex => "codex_skill",
            SkillClient::Cursor => "cursor_skill",
            SkillClient::Gemini => "gemini_skill",
            SkillClient::Orgii => "orgii_skill",
            SkillClient::Windsurf => "windsurf_skill",
        }
    }

    fn skills_root(self, home: &Path) -> PathBuf {
        match self {
            SkillClient::Claude => home.join(".claude").join("skills"),
            SkillClient::Codex => home.join(".agents").join("skills"),
            SkillClient::Cursor => home.join(".cursor").join("skills"),
            SkillClient::Gemini => home.join(".gemini").join("skills"),
            SkillClient::Orgii => home.join(".orgii").join("skills"),
            SkillClient::Windsurf => home.join(".codeium").join("windsurf").join("skills"),
        }
    }

    /// The `SKILL.md` path: `<root>/brick/SKILL.md`.
    fn skill_path(self, home: &Path) -> PathBuf {
        self.skills_root(home).join(SKILL_NAME).join("SKILL.md")
    }
}

/// Installs (or rolls forward) the Brick skill for one client. Idempotent: an
/// up-to-date skill is left untouched unless `force` is set.
pub fn install(client: SkillClient, home: &Path, force: bool) -> Result<SkillAction> {
    let path = client.skill_path(home);
    let desired = skill_md();
    if let Some(existing) = read_existing(&path)? {
        let state = classify(&existing);
        if state == SkillAction::Present && !force {
            return Ok(SkillAction::Unchanged);
        }
        write_atomic(&path, &desired)?;
        return Ok(SkillAction::Updated);
    }
    write_atomic(&path, &desired)?;
    Ok(SkillAction::Installed)
}

/// Removes the Brick skill file for one client, leaving other skills untouched.
/// The `brick/` directory is removed too when it ends up empty.
pub fn uninstall(client: SkillClient, home: &Path) -> Result<SkillAction> {
    let path = client.skill_path(home);
    if read_existing(&path)?.is_none() {
        return Ok(SkillAction::Absent);
    }
    std::fs::remove_file(&path)
        .with_context(|| format!("failed to remove skill at {}", path.display()))?;
    if let Some(dir) = path.parent() {
        // Best-effort: drop the now-empty `brick/` dir; ignore if not empty.
        let _ = std::fs::remove_dir(dir);
    }
    Ok(SkillAction::Removed)
}

/// Reports whether the Brick skill is present, stale, or absent for one client.
pub fn status(client: SkillClient, home: &Path) -> Result<SkillAction> {
    let path = client.skill_path(home);
    match read_existing(&path)? {
        Some(content) => Ok(classify(&content)),
        None => Ok(SkillAction::Absent),
    }
}

/// The `SKILL.md` path for a client (exposed for reporting).
pub fn skill_path(client: SkillClient, home: &Path) -> PathBuf {
    client.skill_path(home)
}

/// Reads a skill file, returning `None` when it does not exist.
fn read_existing(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Classifies existing content as a present (current), stale (older version), or —
/// when no Brick marker is found — present-but-foreign skill we still treat as
/// present so we never clobber a user's own `brick` skill silently. Stale only when
/// the marker is ours AND the version differs.
fn classify(content: &str) -> SkillAction {
    match content.find(SKILL_MARKER_PREFIX) {
        Some(idx) => {
            let after = &content[idx + SKILL_MARKER_PREFIX.len()..];
            let version: u32 = after
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            if version == SKILL_VERSION {
                SkillAction::Present
            } else {
                SkillAction::Stale
            }
        }
        // No Brick marker — a foreign file at our path. Report present so we don't
        // overwrite it except under --force.
        None => SkillAction::Present,
    }
}

/// Writes `content` to `path` atomically, creating parent directories.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to install skill at {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("brick-skill-test-{tag}-{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn skill_md_has_required_frontmatter() {
        let md = skill_md();
        assert!(md.starts_with("---\n"), "must start with frontmatter");
        assert!(md.contains("name: brick"));
        assert!(md.contains("description:"));
        assert!(md.contains("always: true"));
        // The trigger phrasing the agent routes on must be present.
        assert!(md.contains("caused a bug"));
        assert!(md.contains("brick explain"));
        assert!(md.contains(SKILL_MARKER_PREFIX));
    }

    /// ORGII's scanner reads only the text on the SAME line as `description:` and
    /// drops continuation lines. A multi-line / folded-scalar description would be
    /// silently truncated to `>-`. This guards the single-line invariant by
    /// reproducing that line parser and asserting it recovers the full text.
    #[test]
    fn description_is_single_line_and_survives_orgii_line_parser() {
        let md = skill_md();
        // No YAML folded/block scalar indicators on the description line.
        assert!(
            !md.contains("description: >-") && !md.contains("description: |"),
            "description must be a plain single-line scalar"
        );
        // Reproduce ORGII's parse: text after `description:` on its own line.
        let line = md
            .lines()
            .find(|l| l.trim_start().starts_with("description:"))
            .expect("description line");
        let parsed = line
            .trim()
            .strip_prefix("description:")
            .unwrap()
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        assert!(
            parsed.contains("caused a bug") && parsed.contains("brick explain"),
            "ORGII line parser must recover the full description, got: {parsed}"
        );
    }

    #[test]
    fn install_writes_to_skill_client_roots() {
        let cases = [
            (SkillClient::Claude, ".claude/skills/brick/SKILL.md"),
            (SkillClient::Codex, ".agents/skills/brick/SKILL.md"),
            (SkillClient::Cursor, ".cursor/skills/brick/SKILL.md"),
            (SkillClient::Gemini, ".gemini/skills/brick/SKILL.md"),
            (SkillClient::Orgii, ".orgii/skills/brick/SKILL.md"),
            (
                SkillClient::Windsurf,
                ".codeium/windsurf/skills/brick/SKILL.md",
            ),
        ];

        for (client, suffix) in cases {
            let home = temp_home(client.label());
            let action = install(client, &home, false).unwrap();
            assert_eq!(action, SkillAction::Installed);
            let path = skill_path(client, &home);
            assert!(path.ends_with(suffix), "{}", path.display());
            assert!(path.exists());
        }
    }

    #[test]
    fn install_is_idempotent() {
        let home = temp_home("idem");
        assert_eq!(
            install(SkillClient::Orgii, &home, false).unwrap(),
            SkillAction::Installed
        );
        assert_eq!(
            install(SkillClient::Orgii, &home, false).unwrap(),
            SkillAction::Unchanged
        );
        assert_eq!(
            status(SkillClient::Orgii, &home).unwrap(),
            SkillAction::Present
        );
    }

    #[test]
    fn stale_version_is_detected_and_rolled_forward() {
        let home = temp_home("stale");
        let path = skill_path(SkillClient::Orgii, &home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let stale = format!("---\nname: brick\n---\n{SKILL_MARKER_PREFIX}0 -->\nold body\n");
        std::fs::write(&path, stale).unwrap();
        assert_eq!(
            status(SkillClient::Orgii, &home).unwrap(),
            SkillAction::Stale
        );
        // install without force rolls a stale block forward.
        assert_eq!(
            install(SkillClient::Orgii, &home, false).unwrap(),
            SkillAction::Updated
        );
        assert_eq!(
            status(SkillClient::Orgii, &home).unwrap(),
            SkillAction::Present
        );
    }

    #[test]
    fn uninstall_removes_file_and_reports_absent() {
        let home = temp_home("uninstall");
        install(SkillClient::Claude, &home, false).unwrap();
        assert_eq!(
            uninstall(SkillClient::Claude, &home).unwrap(),
            SkillAction::Removed
        );
        assert_eq!(
            status(SkillClient::Claude, &home).unwrap(),
            SkillAction::Absent
        );
        // uninstall when absent is reported, not an error.
        assert_eq!(
            uninstall(SkillClient::Claude, &home).unwrap(),
            SkillAction::Absent
        );
    }
}

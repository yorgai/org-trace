//! Conservative shell-command → touched-file inference.
//!
//! Most real coding-agent sessions edit files through `Bash`/`exec_command`
//! rather than a structured edit tool, so attributing those edits is essential
//! for file-session-blame to return useful results. This extractor recognizes
//! only a small set of high-confidence file-*writing* patterns and ignores
//! read-only commands, preferring a missed attribution over a wrong one.

use std::collections::BTreeSet;

/// Returns the set of file paths a shell command is highly likely to have
/// written, in stable (sorted) order. Read-only commands yield nothing.
///
/// Recognized write patterns:
/// - output redirects: `> file`, `>> file` (not `2>`, `2>&1`)
/// - `tee [-a] file...`
/// - in-place edits: `sed -i … file`, `perl -i … file`
/// - file creation/copy/move targets: `touch f`, `cp src dst`, `mv src dst`
///
/// Commands may be chained with `&&`, `||`, `;`, or `|`; each segment is scanned
/// independently.
pub(crate) fn shell_edit_targets(command: &str) -> Vec<String> {
    // A command may embed an apply_patch heredoc (e.g.
    // `bash -lc 'apply_patch << PATCH … *** Begin Patch …'`). In that case the
    // body is a Codex patch, not shell tokens, so parse it as a patch and do not
    // run the shell tokenizer over the patch lines (which would treat patch body
    // `+`/`-`/`@@` lines as redirects/args and emit garbage).
    if command.contains("*** Begin Patch") {
        return apply_patch_targets(command);
    }
    let mut files = BTreeSet::new();
    for segment in split_segments(command) {
        let tokens = tokenize(&segment);
        scan_redirects(&tokens, &mut files);
        scan_commands(&tokens, &mut files);
    }
    files.into_iter().collect()
}

/// Extracts target paths from a Codex apply_patch body embedded in a command,
/// recognizing `*** Add File:` / `*** Update File:` / `*** Delete File:` /
/// `*** Move to:` headers.
fn apply_patch_targets(command: &str) -> Vec<String> {
    let mut files = BTreeSet::new();
    for line in command.lines() {
        let trimmed = line.trim_start();
        for marker in [
            "*** Add File:",
            "*** Update File:",
            "*** Delete File:",
            "*** Move to:",
        ] {
            if let Some(path) = trimmed.strip_prefix(marker) {
                let path = path.trim().trim_matches(|c| c == '"' || c == '\'');
                if !path.is_empty() {
                    files.insert(path.to_string());
                }
            }
        }
    }
    files.into_iter().collect()
}

/// Splits a command line on shell separators that start a new simple command.
fn split_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                segments.push(std::mem::take(&mut current));
            }
            '|' if chars.peek() == Some(&'|') => {
                chars.next();
                segments.push(std::mem::take(&mut current));
            }
            ';' | '|' | '\n' => segments.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    segments.push(current);
    segments
}

/// Splits a segment into whitespace-separated tokens, stripping surrounding
/// quotes from each. Intentionally simple — good enough for path extraction.
fn tokenize(segment: &str) -> Vec<String> {
    segment
        .split_whitespace()
        .map(|token| token.trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|token| !token.is_empty())
        .collect()
}

/// Finds redirect targets: a token that is exactly `>`/`>>` (target is the next
/// token) or a `>file`/`>>file` glued form. Skips fd-qualified redirects like
/// `2>` and `2>&1`.
fn scan_redirects(tokens: &[String], files: &mut BTreeSet<String>) {
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == ">" || token == ">>" {
            if let Some(target) = tokens.get(index + 1) {
                push_path(target, files);
            }
            index += 2;
            continue;
        }
        // Glued form `>file` / `>>file`, but not `2>` / `&>` fd redirects.
        if let Some(rest) = token.strip_prefix(">>").or_else(|| token.strip_prefix('>')) {
            if !rest.is_empty() && !rest.contains('&') {
                push_path(rest, files);
            }
        }
        index += 1;
    }
}

/// Finds write targets for known file-mutating commands.
fn scan_commands(tokens: &[String], files: &mut BTreeSet<String>) {
    let Some(program) = tokens.first().map(|token| base_name(token)) else {
        return;
    };
    match program.as_str() {
        "tee" => {
            // tee [-a] FILE...
            for token in &tokens[1..] {
                if token.starts_with('-') {
                    continue;
                }
                push_path(token, files);
            }
        }
        "sed" | "perl" => {
            // Only in-place edits write a file; the file is the last argument.
            let in_place = tokens
                .iter()
                .any(|token| token == "-i" || token.starts_with("-i"));
            if in_place {
                if let Some(last) = tokens.last() {
                    if !last.starts_with('-') {
                        push_path(last, files);
                    }
                }
            }
        }
        "touch" => {
            for token in &tokens[1..] {
                if token.starts_with('-') {
                    continue;
                }
                push_path(token, files);
            }
        }
        "cp" | "mv" | "install" => {
            // Destination is the last non-flag argument.
            if let Some(dest) = tokens[1..]
                .iter()
                .rev()
                .find(|token| !token.starts_with('-'))
            {
                push_path(dest, files);
            }
        }
        _ => {}
    }
}

/// Adds a path-looking token, rejecting obvious non-paths (flags, redirector
/// fds, command substitutions).
fn push_path(token: &str, files: &mut BTreeSet<String>) {
    let trimmed = token.trim_matches(|c| c == '"' || c == '\'');
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.starts_with('$')
        || trimmed == "/dev/null"
        || trimmed.contains('*')
    {
        return;
    }
    // Reject tokens that don't look like a file path: pure numbers (fd numbers
    // like `1`/`2`) and tokens carrying shell metacharacters or patch noise
    // (`&`, `=`, braces, parens, `;`, backticks, redirects, pipes).
    if trimmed.chars().all(|c| c.is_ascii_digit())
        || trimmed.contains(['&', '=', '{', '}', '(', ')', ';', '`', '<', '>', '|', '\\'])
    {
        return;
    }
    // A real file path always contains at least one alphanumeric character;
    // reject bare punctuation tokens (`,`, `.`, `&&`-leftovers) that complex
    // multi-line commands can leave on a redirect boundary.
    if !trimmed.chars().any(|c| c.is_ascii_alphanumeric()) {
        return;
    }
    files.insert(trimmed.to_string());
}

fn base_name(token: &str) -> String {
    token.rsplit('/').next().unwrap_or(token).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_redirect_targets() {
        assert_eq!(shell_edit_targets("echo hi > out.txt"), vec!["out.txt"]);
        assert_eq!(
            shell_edit_targets("printf x >> logs/app.log"),
            vec!["logs/app.log"]
        );
        assert_eq!(shell_edit_targets("cat a.txt > b.txt"), vec!["b.txt"]);
    }

    #[test]
    fn extracts_glued_redirect() {
        assert_eq!(shell_edit_targets("echo hi >out.txt"), vec!["out.txt"]);
    }

    #[test]
    fn ignores_fd_redirects_and_devnull() {
        assert!(shell_edit_targets("make 2>&1").is_empty());
        assert!(shell_edit_targets("cmd > /dev/null").is_empty());
    }

    #[test]
    fn extracts_in_place_sed() {
        assert_eq!(
            shell_edit_targets("sed -i 's/a/b/' src/main.rs"),
            vec!["src/main.rs"]
        );
        // A non-in-place sed reads only.
        assert!(shell_edit_targets("sed 's/a/b/' src/main.rs").is_empty());
    }

    #[test]
    fn extracts_tee_touch_cp_mv() {
        assert_eq!(shell_edit_targets("echo x | tee note.md"), vec!["note.md"]);
        assert_eq!(shell_edit_targets("touch new.rs"), vec!["new.rs"]);
        assert_eq!(shell_edit_targets("cp a.txt b.txt"), vec!["b.txt"]);
        assert_eq!(shell_edit_targets("mv old.rs new.rs"), vec!["new.rs"]);
    }

    #[test]
    fn ignores_read_only_commands() {
        assert!(shell_edit_targets("cat README.md").is_empty());
        assert!(shell_edit_targets("ls -la src").is_empty());
        assert!(shell_edit_targets("grep -r foo .").is_empty());
        assert!(shell_edit_targets("rg pattern").is_empty());
    }

    #[test]
    fn handles_chained_commands() {
        let files = shell_edit_targets("mkdir -p d && echo x > d/f.txt && cat d/f.txt");
        assert_eq!(files, vec!["d/f.txt"]);
    }

    #[test]
    fn extracts_apply_patch_heredoc_targets() {
        let command = "bash -lc 'apply_patch << PATCH\n*** Begin Patch\n*** Add File: dna/problem4.py\n+x = 0\n@@\n-old\n+new\n*** Update File: src/lib.rs\n*** End Patch\nPATCH'";
        let files = shell_edit_targets(command);
        assert_eq!(files, vec!["dna/problem4.py", "src/lib.rs"]);
    }

    #[test]
    fn rejects_patch_body_noise_as_paths() {
        let command = "apply_patch\n*** Begin Patch\n*** Add File: a.py\n+result = {7}\n+x == 0:\n*** End Patch";
        assert_eq!(shell_edit_targets(command), vec!["a.py"]);
    }

    #[test]
    fn rejects_fd_and_operator_tokens_in_plain_commands() {
        assert!(shell_edit_targets("python run.py 2>&1 | tee").is_empty());
        assert!(shell_edit_targets("echo done > 1").is_empty());
    }

    #[test]
    fn rejects_bare_punctuation_redirect_tokens() {
        // Complex multi-line commands can leave a stray `,` on a redirect
        // boundary; a real path always has an alphanumeric char.
        assert!(shell_edit_targets("echo x > ,").is_empty());
        assert!(shell_edit_targets("printf y >> .").is_empty());
    }
}

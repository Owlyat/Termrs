//! Import shell commands from cheat.sh cheat sheets.
//!
//! `https://cheat.sh/<topic>?T` answers plain text: `#` comment lines,
//! blank-separated sections, one command per remaining line, plus
//! `#[source]` markers, `---` separators and `tags:` frontmatter. This
//! module fetches that text (blocking: always call off the UI thread, like
//! the AI worker) and parses it into saveable `(command, comment)` rows.
//!
//! Nothing here is ever executed: rows land in the command database with a
//! `cheat.sh:<topic>` tag for review in the picker, where running one still
//! takes an explicit Enter. Cheat-sheet `<placeholders>` are rewritten as
//! `{placeholders}` so they flow through the picker's value prompts.

use std::collections::HashSet;
use std::time::Duration;

/// At most this many rows per import (a sheet is a few dozen lines; this
/// caps pathological topics from flooding the database).
pub const MAX_ENTRIES: usize = 200;

/// Longer lines are prose that wrapped, not runnable commands.
pub const MAX_LINE: usize = 500;

/// Per-fetch network budget.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// One parsed cheat-sheet row, ready to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheatEntry {
    pub command: String,
    pub comment: String,
}

/// Download the plain-text sheet for `topic` (blocking; call off the UI
/// thread). `?T` asks for colorless output; any ANSI that slips through is
/// stripped anyway.
///
/// cheat.sh gates its text mode on the User-Agent (unknown agents get the
/// HTML frontend even with `?T`), so this identifies as curl. A body that
/// still looks like HTML is rejected instead of imported as garbage.
pub fn fetch_topic(topic: &str) -> Result<String, String> {
    let topic = topic.trim();
    if topic.is_empty() {
        return Err("empty topic (try e.g. tar)".into());
    }
    if topic.len() > 128 {
        return Err("topic too long".into());
    }
    let url = format!("https://cheat.sh/{}?T", encode_topic(topic));
    let text = ureq::get(&url)
        .set("User-Agent", "curl/8.0 (shellrs)")
        .set("Accept", "text/plain")
        .timeout(FETCH_TIMEOUT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => {
                format!("cheat.sh: HTTP {code} for {topic:?} (unknown topic?)")
            }
            ureq::Error::Transport(t) => format!("cheat.sh unreachable: {t}"),
        })
        .and_then(|r| {
            r.into_string()
                .map_err(|e| format!("cheat.sh: read failed: {e}"))
        })?;
    if text.trim().is_empty() {
        return Err(format!("cheat.sh: empty sheet for {topic:?}"));
    }
    if is_html_body(&text) {
        return Err(format!(
            "cheat.sh returned a web page instead of text for {topic:?} (try again later)"
        ));
    }
    Ok(crate::ansi::strip_ansi_markers(&text).0)
}

/// True when a response body looks like the cheat.sh HTML frontend rather
/// than a text sheet (doctype or root tag near the start).
fn is_html_body(text: &str) -> bool {
    text.chars()
        .take(2048)
        .collect::<String>()
        .to_ascii_lowercase()
        .contains("<html")
}

/// Parse `?T` sheet text into entries.
///
/// Rule: every `#` line above a command is its comment. Comment lines
/// accumulate until a command consumes them (stacked command lines share
/// one comment block via the remembered fallback); blank lines change
/// nothing. `#[markers]`, `---` separators, `tags:` frontmatter and
/// multi-source section headers (`cheat:fd`, `tldr:fd`) are skipped, as is
/// a leading `$ ` shell prompt some sheets use.
pub fn parse_sheet(text: &str) -> Vec<CheatEntry> {
    let mut out: Vec<CheatEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: Vec<String> = Vec::new();
    let mut last = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Source markers (`#[cheat.sheets:tar]`), separators, frontmatter,
        // multi-source headers (`cheat:fd`, `tldr:fd`).
        if line.starts_with("#[")
            || line == "---"
            || line.starts_with("tags:")
            || is_section_header(line)
        {
            continue;
        }
        if let Some(body) = line.strip_prefix('#') {
            let body = body.trim();
            if !body.is_empty() {
                pending.push(body.to_string());
            }
            continue;
        }
        // A command line: strip a `$ ` prompt prefix (`$HOME` is not one).
        let mut cmd = line;
        if let Some(rest) = cmd.strip_prefix('$') {
            if rest.is_empty() || rest.starts_with([' ', '\t']) {
                cmd = rest.trim_start();
            }
        }
        if cmd.is_empty() || cmd.len() > MAX_LINE {
            continue;
        }
        let comment = if pending.is_empty() {
            last.clone()
        } else {
            pending.join(" ")
        };
        last = comment.clone();
        pending.clear();
        let command = cheats_to_placeholders(cmd);
        if out.len() >= MAX_ENTRIES || !seen.insert(command.clone()) {
            continue;
        }
        out.push(CheatEntry { command, comment });
    }
    out
}

/// Multi-source section header (`cheat:fd`, `tldr:fd`, `cheat:git/log`):
/// a known source prefix plus a spaceless topic. Never a runnable command,
/// so it is skipped instead of imported.
fn is_section_header(line: &str) -> bool {
    if let Some((src, topic)) = line.split_once(':') {
        if matches!(src, "cheat" | "tldr")
            && !topic.is_empty()
            && !topic.contains([' ', '\t'])
        {
            return true;
        }
    }
    // Bare `name:` label, if a sheet ever uses one.
    if let Some(name) = line.strip_suffix(':') {
        return !name.is_empty()
            && !name.contains([' ', '\t', '/', '\\'])
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.'));
    }
    false
}

/// Rewrite cheat-sheet `<placeholders>` as `{placeholders}` so the picker
/// prompts for values before running.
///
/// Only `<` followed by a name character starts a placeholder (`<-`, `<<`
/// heredocs and bare `a<b` stay literal); the name keeps
/// `[A-Za-z0-9_-]`, spaces become `_`, anything else is dropped.
pub fn cheats_to_placeholders(cmd: &str) -> String {
    let mut out = String::new();
    let mut rest = cmd;
    while let Some(o) = rest.find('<') {
        let after = &rest[o + 1..];
        let valid_start = after
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let Some(c) = after.find('>') else {
            out.push_str(rest);
            return out;
        };
        if !valid_start {
            out.push_str(&rest[..=o]);
            rest = after;
            continue;
        }
        let name = sanitize_name(&after[..c]);
        if name.is_empty() {
            out.push_str(&rest[..=o]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..o]);
        out.push('{');
        out.push_str(&name);
        out.push('}');
        rest = &after[c + 1..];
    }
    out.push_str(rest);
    out
}

/// Placeholder inner text -> `{name}` charset: alphanumerics plus `_`/`-`,
/// separators (whitespace, `.`, `/`, …) fold to single `_`, the rest is
/// dropped.
fn sanitize_name(inner: &str) -> String {
    let mut name = String::new();
    let mut gap = true;
    for c in inner.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            name.push(c);
            gap = false;
        } else if matches!(c, ' ' | '\t' | '.' | '/' | ':' | '+' | '\\') {
            if !gap {
                name.push('_');
                gap = true;
            }
        }
    }
    name.trim_matches('_').to_string()
}

/// Drop entries whose command is already saved (exact match after trim).
pub fn dedupe_new(
    entries: Vec<CheatEntry>,
    existing: &HashSet<String>,
) -> Vec<CheatEntry> {
    entries
        .into_iter()
        .filter(|e| !existing.contains(e.command.trim()))
        .collect()
}

/// Percent-encode a cheat.sh topic, keeping the characters topics actually
/// use (`/` subtopics, `:` and `+` modifiers) readable.
fn encode_topic(topic: &str) -> String {
    let mut out = String::new();
    for b in topic.trim().bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b':' | b'+')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `cheat.sh/tar?T` shape (trimmed): markers, frontmatter,
    /// multi-line comments, stacked commands sharing one comment.
    const TAR_SAMPLE: &str = "#[cheat.sheets:tar]\n\
         # tar\n\
         # GNU version of the tar archiving utility\n\
         \n\
         # Delete file 'xdm' from the archive given to the `-f` flag.\n\
         tar --delete -f xdm_edited.tar.gz xdm\n\
         \n\
         # Extract the contents to the destination given to `-C`;\n\
         # falls back to the CWD when missing.\n\
         tar -C /mnt -xvf Tarball.tar\n\
         #[cheat:tar]\n\
         ---\n\
         tags: [ compression ]\n\
         ---\n\
         # To extract a .tgz or .tar.gz archive:\n\
         tar -xzvf /path/to/foo.tgz\n\
         tar -xzvf /path/to/foo.tar.gz\n\
         \n\
         # To create one:\n\
         $ tar -czvf /path/to/foo.tgz /path/to/foo/\n";

    #[test]
    fn parses_sections_comments_and_stacked_commands() {
        let got = parse_sheet(TAR_SAMPLE);
        assert_eq!(got.len(), 5, "entries: {got:?}");
        assert_eq!(got[0].command, "tar --delete -f xdm_edited.tar.gz xdm");
        assert!(got[0].comment.contains("Delete file"), "comment: {:?}", got[0].comment);
        // Multi-line comment block joins with a space.
        assert!(got[1].comment.contains("falls back"), "comment: {:?}", got[1].comment);
        // Markers, separators and frontmatter never become rows.
        assert!(!got.iter().any(|e| e.command.contains("cheat") || e.command == "---"));
        assert!(!got.iter().any(|e| e.comment.contains("cheat.sheets")));
        // Stacked commands share their comment block...
        assert_eq!(got[2].command, "tar -xzvf /path/to/foo.tgz");
        assert_eq!(got[3].command, "tar -xzvf /path/to/foo.tar.gz");
        assert_eq!(got[2].comment, got[3].comment);
        assert!(got[2].comment.contains("extract a .tgz"), "comment: {:?}", got[2].comment);
        // ...and a `$ ` prompt prefix is stripped.
        assert_eq!(got[4].command, "tar -czvf /path/to/foo.tgz /path/to/foo/");
    }

    /// Multi-source answer (`cheat:fd` + `tldr:fd` sections): headers are
    /// skipped, every other non-comment line is a command.
    #[test]
    fn parses_multisource_fd_sheet() {
        let sample = "cheat:fd\n\
             # Simple search:\n\
             fd <search query>\n\
             \n\
             # Specifying the root directory for the search:\n\
             fd <search query> <directory>\n\
             \n\
             # Searching for a particular file extension:\n\
             fd -e <file extension> <search query>\n\
             \n\
             tldr:fd\n\
             # fd\n\
             # An alternative to `find`.\n\
             # Aims to be faster and easier to use than `find`.\n\
             # More information: <https://github.com/sharkdp/fd>.\n\
             \n\
             # Recursively find files matching a specific pattern in the current directory:\n\
             fd \"string|regex\"\n\
             \n\
             # Execute a command on each search result returned:\n\
             fd \"string|regex\" --exec command\n";
        let got = parse_sheet(sample);
        assert_eq!(got.len(), 5, "entries: {got:?}");
        // Section headers never become rows...
        assert!(!got.iter().any(|e| e.command == "cheat:fd" || e.command == "tldr:fd"));
        // ...and every other line is picked up with its comment.
        assert_eq!(got[0].command, "fd {search_query}");
        assert!(got[0].comment.contains("Simple search"), "comment: {:?}", got[0].comment);
        assert_eq!(got[1].command, "fd {search_query} {directory}");
        assert_eq!(got[2].command, "fd -e {file_extension} {search_query}");
        assert_eq!(got[3].command, "fd \"string|regex\"");
        assert!(got[3].comment.contains("Recursively find"), "comment: {:?}", got[3].comment);
        assert_eq!(got[4].command, "fd \"string|regex\" --exec command");
    }

    #[test]
    fn section_headers_detected() {
        assert!(is_section_header("cheat:fd"));
        assert!(is_section_header("tldr:fd"));
        assert!(is_section_header("cheat:git/log"));
        assert!(is_section_header("section:"));
        assert!(!is_section_header("fd <search query>"));
        assert!(!is_section_header("C:\\path\\file:"));
        assert!(!is_section_header("http://host:8080"));
        assert!(!is_section_header("tags: [ x ]"));
        assert!(!is_section_header(":"));
        assert!(!is_section_header("not a header: with spaces"));
    }

    #[test]
    fn html_bodies_rejected() {
        assert!(is_html_body("<html>\n<head><title>cheat.sh/fd</title>"));
        assert!(is_html_body("<!DOCTYPE html><html lang=\"en\">"));
        assert!(is_html_body("  \n<HTML>"));
        assert!(!is_html_body("# tar\ntar -xvf foo.tar\n"));
        assert!(!is_html_body("cheat:fd\n# hi\nfd x\n"));
    }

    #[test]
    fn blanks_do_not_reset_comments() {
        // Every `#` line above a command is its comment, blanks or not.
        let got = parse_sheet("# what it does\n\ncmd --flag\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].comment, "what it does", "got: {got:?}");
        // ...including multi-line blocks split by blanks, and inheritance
        // past a blank after a consumed block.
        let got = parse_sheet("# line one\n\n# line two\ncmd-a\n\ncmd-b\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].comment, "line one line two", "got: {got:?}");
        assert_eq!(got[1].comment, "line one line two", "got: {got:?}");
    }

    #[test]
    fn placeholders_become_picker_values() {
        assert_eq!(cheats_to_placeholders("tar -xvf <archive>"), "tar -xvf {archive}");
        assert_eq!(
            cheats_to_placeholders("cp <source file> <dest>"),
            "cp {source_file} {dest}"
        );
        assert_eq!(
            cheats_to_placeholders("tar -czvf <target.tar> <dir>"),
            "tar -czvf {target_tar} {dir}"
        );
        // Shell syntax stays literal: redirect-from, heredoc, bare `<`.
        assert_eq!(cheats_to_placeholders("cmd <- file"), "cmd <- file");
        assert_eq!(cheats_to_placeholders("cat <<EOF"), "cat <<EOF");
        assert_eq!(cheats_to_placeholders("a<b"), "a<b");
        assert_eq!(cheats_to_placeholders("echo <>"), "echo <>");
        assert_eq!(cheats_to_placeholders("plain command"), "plain command");
    }

    #[test]
    fn topics_encode_safely() {
        assert_eq!(encode_topic("tar"), "tar");
        assert_eq!(encode_topic("git/log"), "git/log");
        assert_eq!(encode_topic("c++"), "c++");
        assert_eq!(encode_topic("a b"), "a%20b");
        assert_eq!(encode_topic("a?b"), "a%3Fb");
    }

    #[test]
    fn dedupe_skips_saved_commands() {
        let entries = vec![
            CheatEntry { command: "tar -xvf {f}".into(), comment: String::new() },
            CheatEntry { command: "tar -czvf a b".into(), comment: String::new() },
        ];
        let existing: HashSet<String> = ["tar -czvf a b".to_string()].into_iter().collect();
        let fresh = dedupe_new(entries, &existing);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].command, "tar -xvf {f}");
    }

    /// Live fetch (needs network): run explicitly, never in the suite.
    /// Proves text mode (HTML bodies are rejected) and multi-source parse.
    #[test]
    #[ignore = "needs network; run with --ignored"]
    fn live_fetch_tar_sheet() {
        let text = fetch_topic("tar").expect("cheat.sh reachable");
        let entries = parse_sheet(&text);
        assert!(!entries.is_empty(), "tar sheet parsed to nothing");
        assert!(
            entries.iter().any(|e| e.command.contains("tar")),
            "no tar command found"
        );
        // Multi-source topic: text (not HTML), only runnable rows.
        let fd = fetch_topic("fd").expect("cheat.sh fd reachable");
        let entries = parse_sheet(&fd);
        assert!(!entries.is_empty(), "fd sheet parsed to nothing");
        assert!(
            entries.iter().all(|e| e.command.contains("fd")),
            "non-fd rows imported: {entries:?}"
        );
        assert!(
            !entries.iter().any(|e| e.command.contains("cheat:") || e.command.contains("tldr:")),
            "section headers imported: {entries:?}"
        );
    }
}

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

// File order (§7-module-map §3): §1 `MAX_ENTRIES` → §4 `import` entry →
// §5 fetch → §7 sheet-parse helpers.

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
        .set("User-Agent", "curl/8.0 (termrs)")
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
        if let Some(rest) = cmd.strip_prefix('$')
            && (rest.is_empty() || rest.starts_with([' ', '\t'])) {
                cmd = rest.trim_start();
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
    if let Some((src, topic)) = line.split_once(':')
        && matches!(src, "cheat" | "tldr")
            && !topic.is_empty()
            && !topic.contains([' ', '\t'])
        {
            return true;
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
        } else if matches!(c, ' ' | '\t' | '.' | '/' | ':' | '+' | '\\')
            && !gap {
                name.push('_');
                gap = true;
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
#[path = "../test/cheatsheet_tests.rs"]
mod tests;

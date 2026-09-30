//! Placeholder substitution for template args.
//!
//! Batch style: `%1%`..`%N%`, `%*%` / `%!%` (all words), `%lineN%` / `%line_N%`
//! (case-insensitive), `%%` = literal `%`.
//!
//! Unix style: `$1`..`$N`, `${N}`, `$@` / `$*` / `$!` / `${@}` / `${*}` / `${!}`
//! (all words), `$lineN` / `${lineN}` (+ `_` + upper variants,
//! case-insensitive), `$$` = literal `$`.
//!
//! Unknown `%NAME%` / `$NAME` pass through untouched so child
//! shell still expands real env vars.

use crate::parse::Parsed;

/// Substitute placeholders in one template arg (no transform stdin).
#[allow(dead_code)]
pub fn substitute_arg(template: &str, data: &Parsed) -> String {
    substitute_arg_with_stdin(template, data, &[])
}

/// Substitute placeholders in one template arg, running per-occurrence
/// `$N=(cmd)` / `%N%=(cmd)` transforms with `stdin` as their stdin.
///
/// Only the occurrence carrying `=(...)` is replaced by the command
/// output; other occurrences of the same placeholder resolve normally.
/// Inside `cmd`, bare or double-quoted piper placeholders are expanded
/// from the same piped input (so `$1=(kalk $1*5)` computes from word 1),
/// while single-quoted spans stay verbatim for the child shell
/// (so `awk '{print $2}'` keeps its `$2`). One level only: the expanded
/// command is passed to the shell as-is, never re-scanned for transforms.
pub fn substitute_arg_with_stdin(template: &str, data: &Parsed, stdin: &[u8]) -> String {
    let all = data.words.join(" ");
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '%' {
            if let Some(repl) = eat_batch(template, i, data, &all) {
                let next = repl.1;
                // `%%` escape never carries a transform.
                if template[i..next] != *"%%"
                    && let Some((cmd, after)) = try_transform_suffix(template, next)
                {
                    let cmd = expand_inner(&cmd, data);
                    out.push_str(&crate::run::run_transform(&cmd, stdin));
                    i = after;
                    continue;
                }
                out.push_str(&repl.0);
                i = next;
                continue;
            }
            out.push('%');
            i += 1;
        } else if c == '$' {
            if let Some(repl) = eat_unix(template, i, data, &all) {
                let next = repl.1;
                // `$$` escape never carries a transform.
                if template[i..next] != *"$$"
                    && let Some((cmd, after)) = try_transform_suffix(template, next)
                {
                    let cmd = expand_inner(&cmd, data);
                    out.push_str(&crate::run::run_transform(&cmd, stdin));
                    i = after;
                    continue;
                }
                out.push_str(&repl.0);
                i = next;
                continue;
            }
            // eat_unix returns None only for lone/unknown `$`:
            // emit `$` + (unknown name kept by caller push below).
            // Re-scan: copy `$` literally and advance 1; name chars
            // get copied on following iterations.
            out.push('$');
            i += 1;
        } else {
            // Copy full UTF-8 char.
            let ch_len = template[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            out.push_str(&template[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

/// Substitute placeholders in every template arg.
#[allow(dead_code)]
pub fn substitute_args(template: &[String], data: &Parsed) -> Vec<String> {
    substitute_args_with_stdin(template, data, &[])
}

/// Same as [`substitute_args`] but per-occurrence `=(...)` transforms
/// receive `stdin` as their stdin.
pub fn substitute_args_with_stdin(
    template: &[String],
    data: &Parsed,
    stdin: &[u8],
) -> Vec<String> {
    template
        .iter()
        .map(|a| substitute_arg_with_stdin(a, data, stdin))
        .collect()
}

/// If `s[next..]` starts with `=(`, find the matching `)` (quote- and
/// depth-aware) and return (inner command, offset after `)`).
fn try_transform_suffix(s: &str, next: usize) -> Option<(String, usize)> {
    if next + 2 > s.len() || &s[next..next + 2] != "=(" {
        return None;
    }
    extract_paren(s, next + 1)
}

/// Extract `( ... )` starting at byte offset `open` (which must be `(`).
/// Parens inside single/double quotes are ignored, `(` increments depth.
/// Returns (inner text without outer parens, offset just after `)`).
fn extract_paren(s: &str, open: usize) -> Option<(String, usize)> {
    debug_assert_eq!(&s[open..open + 1], "(");
    let b = s.as_bytes();
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut j = open;
    while j < b.len() {
        let ch = b[j];
        if escaped {
            escaped = false;
            j += 1;
            continue;
        }
        if in_single {
            if ch == b'\'' {
                in_single = false;
            }
            j += 1;
            continue;
        }
        if in_double {
            if ch == b'"' {
                in_double = false;
            } else if ch == b'\\' {
                escaped = true;
            }
            j += 1;
            continue;
        }
        match ch {
            b'\\' => {
                escaped = true;
                j += 1;
            }
            b'\'' => {
                in_single = true;
                j += 1;
            }
            b'"' => {
                in_double = true;
                j += 1;
            }
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth -= 1;
                j += 1;
                if depth == 0 {
                    return Some((s[open + 1..j - 1].to_string(), j));
                }
            }
            _ => j += 1,
        }
    }
    None
}

/// Expand piper placeholders inside a transform's inner command.
///
/// Single-quoted spans stay verbatim (so `awk '{print $2}'` keeps its
/// `$2` for the child shell); bare or double-quoted `$n` / `%n%` /
/// `$*` / ... resolve from `data` like in the outer template. Unknown
/// `%NAME%` / `$NAME` pass through for the child shell. One level only:
/// the result is never re-scanned for further `=(...)` transforms.
fn expand_inner(cmd: &str, data: &Parsed) -> String {
    let all = data.words.join(" ");
    let b = cmd.as_bytes();
    let mut out = String::with_capacity(cmd.len());
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < b.len() {
        let c = b[i];
        // Non-ASCII bytes are never special: copy the whole UTF-8 char.
        if c >= 0x80 {
            let ch_len = cmd[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            out.push_str(&cmd[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        if in_single {
            out.push(c as char);
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                // A `'` inside double quotes is literal, otherwise it
                // opens a verbatim span.
                if !in_double {
                    in_single = true;
                }
                out.push('\'');
                i += 1;
            }
            b'"' => {
                in_double = !in_double;
                out.push('"');
                i += 1;
            }
            b'\\' => {
                // Escape: keep backslash + next char literally (mirrors
                // `extract_paren`), so `\$1` survives for the child shell.
                out.push('\\');
                if i + 1 < b.len() {
                    let n = b[i + 1];
                    if n >= 0x80 {
                        let ch_len =
                            cmd[i + 1..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                        out.push_str(&cmd[i + 1..i + 1 + ch_len]);
                        i += 1 + ch_len;
                    } else {
                        out.push(n as char);
                        i += 2;
                    }
                } else {
                    i += 1;
                }
            }
            b'$' => {
                if let Some((repl, next)) = eat_unix(cmd, i, data, &all) {
                    out.push_str(&repl);
                    i = next;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            b'%' => {
                if let Some((repl, next)) = eat_batch(cmd, i, data, &all) {
                    out.push_str(&repl);
                    i = next;
                } else {
                    out.push('%');
                    i += 1;
                }
            }
            _ => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// Try parse `%...%` at byte offset `i`. Returns (replacement, next offset).
fn eat_batch(s: &str, i: usize, data: &Parsed, all: &str) -> Option<(String, usize)> {
    let rest = &s[i..];
    let b = rest.as_bytes();
    if b.len() < 2 {
        return None; // lone `%`
    }
    if b[1] == b'%' {
        return Some(("%".to_string(), i + 2));
    }
    // Find closing `%`.
    let end = rest[1..].find('%')?;
    let inner = &rest[1..1 + end];
    if inner.is_empty() {
        return None;
    }
    if inner == "*" || inner == "!" {
        return Some((all.to_string(), i + 1 + end + 1));
    }
    if inner.chars().all(|c| c.is_ascii_digit()) {
        let n: usize = inner.parse().unwrap_or(0);
        let v = (n >= 1).then(|| data.words.get(n - 1).cloned().unwrap_or_default());
        return v.map(|v| (v, i + 1 + end + 1));
    }
    if let Some(n) = parse_line_key(inner) {
        let v = data.lines.get(n - 1).cloned().unwrap_or_default();
        return Some((v, i + 1 + end + 1));
    }
    None // unknown %NAME%: leave untouched
}

/// Try parse `$...` at byte offset `i`. Returns (replacement, next offset).
/// Unknown `$NAME` returns None so caller copies through literally.
fn eat_unix(s: &str, i: usize, data: &Parsed, all: &str) -> Option<(String, usize)> {
    let rest = &s[i..];
    let b = rest.as_bytes();
    if b.len() < 2 {
        return None;
    }
    match b[1] {
        b'$' => Some(("$".to_string(), i + 2)),
        b'*' | b'@' | b'!' => Some((all.to_string(), i + 2)),
        b'0'..=b'9' => {
            let mut j = 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let n: usize = rest[1..j].parse().unwrap_or(0);
            let v = if n >= 1 {
                data.words.get(n - 1).cloned().unwrap_or_default()
            } else {
                String::new()
            };
            Some((v, i + j))
        }
        b'{' => {
            let close = rest[2..].find('}')?;
            let inner = &rest[2..2 + close];
            if inner == "*" || inner == "@" || inner == "!" {
                return Some((all.to_string(), i + 2 + close + 1));
            }
            if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
                let n: usize = inner.parse().unwrap_or(0);
                let v = if n >= 1 {
                    data.words.get(n - 1).cloned().unwrap_or_default()
                } else {
                    String::new()
                };
                return Some((v, i + 2 + close + 1));
            }
            if let Some(n) = parse_line_key(inner) {
                let v = data.lines.get(n - 1).cloned().unwrap_or_default();
                return Some((v, i + 2 + close + 1));
            }
            None// unknown ${NAME}: pass through
        }
        c if (c as char).is_ascii_alphabetic() || c == b'_' => {
            let mut j = 1;
            while j < b.len() && ((b[j] as char).is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let name = &rest[1..j];
            if let Some(n) = parse_line_key(name) {
                let v = data.lines.get(n - 1).cloned().unwrap_or_default();
                return Some((v, i + j));
            }
            None// e.g. $HOME, $PATH: pass through for shell
        }
        _ => None,
    }
}

/// Parse `lineN` / `line_N` (case-insensitive) -> 1-based N.
fn parse_line_key(inner: &str) -> Option<usize> {
    let low = inner.to_ascii_lowercase();
    let digits = low.strip_prefix("line")?.strip_prefix('_').unwrap_or_else(|| &low[4..]);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Parsed {
        Parsed {
            lines: vec!["1 2 3".into(), "hello world ___".into()],
            words: vec!["1", "2", "3", "hello", "world", "___"]
                .into_iter()
                .map(String::from)
                .collect(),
        }
    }

    #[test]
    fn batch_numbered_and_all() {
        let d = data();
        assert_eq!(substitute_arg("%1% %2% %6%", &d), "1 2 ___");
        assert_eq!(substitute_arg("[%*%]", &d), "[1 2 3 hello world ___]");
        assert_eq!(substitute_arg("%%%1%%%", &d), "%1%");
    }

    #[test]
    fn batch_lines_case_insensitive() {
        let d = data();
        assert_eq!(substitute_arg("%line2%", &d), "hello world ___");
        assert_eq!(substitute_arg("%LINE_1%", &d), "1 2 3");
    }

    #[test]
    fn unix_numbered_and_all() {
        let d = data();
        assert_eq!(substitute_arg("$1-$2-$6", &d), "1-2-___");
        assert_eq!(substitute_arg("${1} ${@} ${*}", &d), "1 1 2 3 hello world ___ 1 2 3 hello world ___");
        assert_eq!(substitute_arg("$@ and $*", &d), "1 2 3 hello world ___ and 1 2 3 hello world ___");
    }

    #[test]
    fn unix_lines_and_passthrough() {
        let d = data();
        assert_eq!(substitute_arg("$line2 ${LINE_1}", &d), "hello world ___ 1 2 3");
        assert_eq!(substitute_arg("$HOME stays", &d), "$HOME stays");
        assert_eq!(substitute_arg("%PATH% stays", &d), "%PATH% stays");
        assert_eq!(substitute_arg("$$1", &d), "$1");
    }

    #[test]
    fn missing_index_is_empty() {
        let d = data();
        assert_eq!(substitute_arg("[%9%][$9]", &d), "[][]");
    }

    #[test]
    fn bang_is_all_words_alias() {
        let d = data();
        assert_eq!(substitute_arg("$!", &d), "1 2 3 hello world ___");
        assert_eq!(substitute_arg("${!}", &d), "1 2 3 hello world ___");
        assert_eq!(substitute_arg("%!%", &d), "1 2 3 hello world ___");
        assert_eq!(substitute_arg("echo $!", &d), "echo 1 2 3 hello world ___");
    }

    #[test]
    fn paren_extraction_ignores_quotes_and_nesting() {
        let s = "=(awk '{print $2}')";
        assert_eq!(extract_paren(s, 1), Some(("awk '{print $2}'".into(), s.len())));
        let s = "=(sed 's/(a)/(b)/')";
        assert_eq!(
            extract_paren(s, 1),
            Some(("sed 's/(a)/(b)/'".into(), s.len()))
        );
        let s = "=(echo (hi))";
        assert_eq!(extract_paren(s, 1), Some(("echo (hi)".into(), s.len())));
        assert_eq!(extract_paren("=(unclosed", 1), None);
        assert_eq!(
            try_transform_suffix("$1=(cat)", 2),
            Some(("cat".into(), "$1=(cat)".len()))
        );
        assert_eq!(try_transform_suffix("$1", 2), None);
        // `$$` / `%%` escapes never carry a transform.
        assert_eq!(
            substitute_arg_with_stdin("$$=(cat)", &data(), b"in"),
            "$=(cat)"
        );
    }

    #[test]
    fn transform_replaces_only_its_own_occurrence() {
        // Portable stdin-echo: `cat` (unix) / `more` (windows).
        #[cfg(target_os = "windows")]
        const ECHO_STDIN: &str = "more";
        #[cfg(not(target_os = "windows"))]
        const ECHO_STDIN: &str = "cat";
        let d = Parsed {
            lines: vec!["hello hello".into()],
            words: vec!["hello".into(), "hello".into()],
        };
        let tpl = format!("$1=({ECHO_STDIN})");
        // First occurrence transformed (full stdin), second kept as word.
        assert_eq!(
            substitute_arg_with_stdin(&format!("{tpl} $1"), &d, b"hello hello\n"),
            "hello hello hello"
        );
        // Inner `$2` must not be expanded by piper.
        assert_eq!(
            substitute_arg_with_stdin("$1=(echo hi)", &d, b"hello hello\n"),
            "hi"
        );
        // Batch style too.
        let tpl3 = format!("%1%=({ECHO_STDIN})");
        assert_eq!(
            substitute_arg_with_stdin(&format!("{tpl3} %1%"), &d, b"hello hello\n"),
            "hello hello hello"
        );
        // Empty `()` -> empty, other occurrence untouched.
        assert_eq!(
            substitute_arg_with_stdin("$1=() $1", &d, b"hello hello\n"),
            " hello"
        );
        // Unbalanced keeps placeholder value + literal rest.
        assert_eq!(
            substitute_arg_with_stdin("$1=(cat", &d, b"hello hello\n"),
            "hello=(cat"
        );
    }

    #[test]
    fn inner_expands_bare_but_protects_single_quotes() {
        let d = data(); // words: 1 2 3 hello world ___
        // Bare placeholders resolve from the same piped input.
        assert_eq!(expand_inner("kalk $1*5", &d), "kalk 1*5");
        assert_eq!(expand_inner("echo $2 %3% $@", &d), "echo 2 3 1 2 3 hello world ___");
        // Double-quoted placeholders expand too.
        assert_eq!(expand_inner("echo \"$1\"", &d), "echo \"1\"");
        // Single-quoted spans stay verbatim for the child shell.
        assert_eq!(expand_inner("awk '{print $2}'", &d), "awk '{print $2}'");
        assert_eq!(expand_inner("echo '$1' $1", &d), "echo '$1' 1");
        // Unknown names pass through; escapes survive.
        assert_eq!(expand_inner("echo $HOME %PATH%", &d), "echo $HOME %PATH%");
        assert_eq!(expand_inner("echo \\$1", &d), "echo \\$1");
    }

    #[test]
    fn transform_uses_expanded_inner() {
        let d = Parsed {
            lines: vec!["2".into(), "world".into()],
            words: vec!["2".into(), "world".into()],
        };
        // Portable `echo`: inner `$1` is piper's word 1, not the shell's.
        assert_eq!(
            substitute_arg_with_stdin("$1=(echo VAL-$1)", &d, b"2\nworld\n"),
            "VAL-2"
        );
        // The asking use case: sed script with a computed replacement.
        assert_eq!(
            substitute_arg_with_stdin("s/$1/\"$1=(echo VAL-$1)\"/g", &d, b"2\nworld\n"),
            "s/2/\"VAL-2\"/g"
        );
        // Quoted inner placeholders are left for the child shell:
        // `awk '{print $2}'` still sees awk's field 2.
        #[cfg(not(target_os = "windows"))]
        {
            let a = Parsed {
                lines: vec!["hello hello".into()],
                words: vec!["hello".into(), "hello".into()],
            };
            assert_eq!(
                substitute_arg_with_stdin("$1=(awk '{print $2}')", &a, b"hello hello\n"),
                "hello"
            );
        }
    }
}

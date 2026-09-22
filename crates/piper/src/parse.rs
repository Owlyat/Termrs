//! Split piped stdin into lines + flat words.
//!
//! Words split on ASCII whitespace, honouring double quotes
//! so `"a b"` stays one word (quotes kept, matching legacy split_args).

/// Parsed stdin: raw lines + flat word list across all lines.
pub struct Parsed {
    /// Raw lines (no line-break chars).
    pub lines: Vec<String>,
    /// Flat words, in order, across all lines.
    pub words: Vec<String>,
}

/// Parse raw stdin text into [`Parsed`].
pub fn parse_input(text: &str) -> Parsed {
    // Tools like `grep -z/--null-data` terminate output lines with NUL
    // instead of newline (e.g. `grep -Poz ...` emits `match\0`).
    // `str::lines()` does not split on NUL and `char::is_whitespace`
    // is false for NUL, so without this the NUL sticks to the last
    // word and later `Command::arg` fails with
    // "nul byte found in provided data". Treat NUL as a line break.
    let normalized = text.replace('\0', "\n");
    let mut lines = Vec::new();
    let mut words = Vec::new();
    for line in normalized.lines() {
        // Strip trailing \r (Windows pipe) but keep rest raw.
        let line = line.strip_suffix('\r').unwrap_or(line);
        lines.push(line.to_string());
        words.extend(split_args(line));
    }
    Parsed { lines, words }
}

/// Split one line on spaces, keeping `"quoted spans"` together.
pub fn split_args(input: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            c if (c.is_whitespace() || c == '\0') && !in_quotes => {
                if !current.trim().is_empty() {
                    result.push(current.trim().to_string());
                }
                current = String::new();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        result.push(current.trim().to_string());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_flat_words() {
        let p = parse_input("1 2 3\nhello world ___ ___\n");
        assert_eq!(p.lines.len(), 2);
        assert_eq!(p.words, vec!["1", "2", "3", "hello", "world", "___", "___"]);
    }

    #[test]
    fn keeps_quoted_span() {
        assert_eq!(split_args(r#"a "b c" d"#), vec!["a", r#""b c""#, "d"]);
    }

    #[test]
    fn nul_is_line_break_not_part_of_word() {
        // `grep -z/--null-data` emits `match\0`; the NUL must not
        // stick to the word (it would break process spawning).
        let p = parse_input("https://example.com/one-piece\0");
        assert_eq!(p.words, vec!["https://example.com/one-piece"]);
        assert_eq!(p.lines, vec!["https://example.com/one-piece"]);
        let p = parse_input("a\0b\0");
        assert_eq!(p.words, vec!["a", "b"]);
    }
}

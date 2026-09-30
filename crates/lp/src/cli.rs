//! Parsing of an `lp` invocation into a loop variable, a source and a block.
//!
//! An invocation looks like `for <var> --in <source> { <command> }` or
//! `for <var> --in_cmd ( <command> ) { <command> }`. The command block is kept
//! as raw text so quotes and pipes survive intact.

/// Where the iteration values come from.
#[derive(Debug, Clone)]
pub enum Source {
    /// A literal source: integer, range, file or directory path.
    Value(String),
    /// A shell command whose stdout lines are the iteration values.
    Command(String),
}

/// A per-value condition with optional negation (`--if not (...)`).
#[derive(Debug, Clone)]
pub struct Condition {
    pub command: String,
    pub negate: bool,
}

/// A parsed loop invocation.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub var: String,
    pub source: Source,
    pub block: String,
    /// Optional variable receiving the 1-based iteration index.
    pub enumerate: Option<String>,
    /// Optional per-value filter: run the block only when this condition passes.
    pub filter: Option<Condition>,
    /// Optional per-value break condition: stop the loop when this condition fails.
    pub while_cond: Option<Condition>,
}

/// Parse a full invocation string (with or without the leading program name).
pub fn parse(command: &str) -> Result<Invocation, String> {
    let mut rest = command.trim();

    if let Some((token, after)) = peek_token(rest) {
        if is_lp_name(&token) {
            rest = after;
        }
    }

    let (keyword, after) = next_token(rest)?;
    if keyword != "for" && keyword != "--for" {
        return Err("expected 'for <var>'".to_string());
    }

    let (var, after) = next_token(after)?;
    let (source_kw, after) = next_token(after)?;

    let (source, after) = match source_kw.as_str() {
        "in" | "--in" => {
            let (value, after) = next_token(after)?;
            (Source::Value(value), after)
        }
        "in_cmd" | "--in_cmd" => {
            let after = after.trim_start();
            if !after.starts_with('(') {
                return Err("expected '(' after '--in_cmd'".to_string());
            }
            let (cmd, after) = extract_paren(after)?;
            (Source::Command(cmd.trim().to_string()), after)
        }
        other => {
            return Err(format!("expected '--in' or '--in_cmd', found '{other}'"));
        }
    };

    let (enumerate, filter, while_cond, after) = parse_clauses(after)?;

    let after = after.trim_start();
    if !after.starts_with('{') {
        return Err("missing '{ ... }' command block".to_string());
    }
    let (block, _) = extract_block(after)?;

    Ok(Invocation {
        var,
        source,
        block: block.trim().to_string(),
        enumerate,
        filter,
        while_cond,
    })
}

/// Parse the optional `--enumerate <var>`, `--if [not] (cmd)` and
/// `--while [not] (cmd)` clauses after the source, in any order.
fn parse_clauses(input: &str) -> Result<(Option<String>, Option<Condition>, Option<Condition>, &str), String> {
    let mut rest = input;
    let mut enumerate = None;
    let mut filter = None;
    let mut while_cond = None;

    loop {
        let Some((token, after_token)) = peek_token(rest) else {
            break;
        };
        match token.as_str() {
            "--enumerate" => {
                if enumerate.is_some() {
                    return Err("duplicate '--enumerate'".to_string());
                }
                let (name, rest2) = next_token(after_token)?;
                enumerate = Some(name);
                rest = rest2;
            }
            "if" | "--if" => {
                if filter.is_some() {
                    return Err("duplicate '--if'".to_string());
                }
                let (cmd, rest2) = parse_cond_command(after_token, "--if")?;
                filter = Some(cmd);
                rest = rest2;
            }
            "while" | "--while" => {
                if while_cond.is_some() {
                    return Err("duplicate '--while'".to_string());
                }
                let (cmd, rest2) = parse_cond_command(after_token, "--while")?;
                while_cond = Some(cmd);
                rest = rest2;
            }
            _ => break,
        }
    }

    Ok((enumerate, filter, while_cond, rest))
}

/// Parse a `[not] ( <command> )` condition after `--if` / `--while`.
/// A leading `not` (or `!`) negates the exit-status test.
fn parse_cond_command<'a>(input: &'a str, flag: &str) -> Result<(Condition, &'a str), String> {
    let mut rest = input.trim_start();
    let mut negate = false;
    if let Some((token, after_token)) = peek_token(rest) {
        if token.eq_ignore_ascii_case("not") || token == "!" {
            // Only treat it as negation when a '(' follows; otherwise let the
            // paren check below report the missing '(' error.
            if after_token.trim_start().starts_with('(') {
                negate = true;
                rest = after_token;
            }
        }
    }
    let trimmed = rest.trim_start();
    if !trimmed.starts_with('(') {
        return Err(format!("expected '( ... )' after '{flag}'"));
    }
    let (cmd, rest) = extract_paren(trimmed)?;
    let cmd = cmd.trim().to_string();
    if cmd.is_empty() {
        return Err(format!("empty condition after '{flag}'"));
    }
    Ok((Condition { command: cmd, negate }, rest))
}

/// Whether a token names this program (`lp`, `lp.exe`, or a path to it).
fn is_lp_name(token: &str) -> bool {
    std::path::Path::new(token)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("lp"))
}

/// Read the next whitespace-delimited or quoted token.
fn peek_token(input: &str) -> Option<(String, &str)> {
    next_token(input).ok()
}

/// Read the next token, honouring single and double quotes.
fn next_token(input: &str) -> Result<(String, &str), String> {
    let input = input.trim_start();
    let Some(first) = input.chars().next() else {
        return Err("unexpected end of command".to_string());
    };

    if first == '"' || first == '\'' {
        let rest = &input[first.len_utf8()..];
        let Some(pos) = rest.find(first) else {
            return Err("unterminated quote".to_string());
        };
        let token = rest[..pos].to_string();
        return Ok((token, &rest[pos + first.len_utf8()..]));
    }

    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    Ok((input[..end].to_string(), &input[end..]))
}

/// Extract the contents of a brace-delimited block.
///
/// `input` must start at the opening `{`. Braces inside quotes are ignored and
/// nested braces are balanced. Returns the inner text and bytes consumed.
fn extract_block(input: &str) -> Result<(String, usize), String> {
    extract_delimited(input, '{', '}')
}

/// Extract the contents of a parenthesis-delimited command.
///
/// `input` must start at the opening `(`. Parentheses inside quotes are ignored
/// and nesting is balanced. Returns the inner text and the remaining input.
fn extract_paren(input: &str) -> Result<(String, &str), String> {
    let mut depth = 0usize;
    let mut start = None;
    let mut in_double = false;
    let mut in_single = false;

    for (idx, ch) in input.char_indices() {
        match ch {
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '(' if !in_double && !in_single => {
                if depth == 0 {
                    start = Some(idx + ch.len_utf8());
                }
                depth += 1;
            }
            ')' if !in_double && !in_single => {
                if depth == 0 {
                    return Err("unbalanced ')'".to_string());
                }
                depth -= 1;
                if depth == 0 {
                    let begin = start.expect("opening paren recorded");
                    return Ok((input[begin..idx].to_string(), &input[idx + ch.len_utf8()..]));
                }
            }
            _ => {}
        }
    }

    Err("unbalanced '('".to_string())
}

/// Extract the contents between matching delimiters, skipping quoted text.
fn extract_delimited(input: &str, open: char, close: char) -> Result<(String, usize), String> {
    let mut depth = 0usize;
    let mut start = None;
    let mut in_double = false;
    let mut in_single = false;

    for (idx, ch) in input.char_indices() {
        match ch {
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            c if c == open && !in_double && !in_single => {
                if depth == 0 {
                    start = Some(idx + ch.len_utf8());
                }
                depth += 1;
            }
            c if c == close && !in_double && !in_single => {
                if depth == 0 {
                    return Err(format!("unbalanced '{close}'"));
                }
                depth -= 1;
                if depth == 0 {
                    let begin = start.expect("opening delimiter recorded");
                    return Ok((input[begin..idx].to_string(), idx + ch.len_utf8()));
                }
            }
            _ => {}
        }
    }

    Err(format!("unbalanced '{open}'"))
}

//! kalk: tiny stdin + args calculator with shell-style booleans.
//!
//! Usage:
//! ```text
//! kalk 3-2            # => 1
//! kalk "1 + 1 * 2"    # => 3
//! echo 1+1 | kalk     # => 2
//! kalk                # REPL when stdin is a terminal
//! kalk "1 > 2"        # => 1 (false; exit 1)
//! kalk "2 == 2"       # => 0 (true; exit 0)
//! kalk "5 % 2 == 0"   # => 1 (false; 5 is odd)
//! ```
//!
//! Expression syntax: arithmetic comes from [`meval`]: `+ - * / % ^`,
//! parens, functions (`sqrt`, `sin`, `cos`, ...), constants (`pi`, `e`).
//! Prefix `!` is logical NOT handled by kalk (not factorial).
//! Comparisons and logic are handled by kalk itself, shell-style:
//! `0` is true, non-zero is false. `== = != < <= > >=`, `&&`, `||`, prefix `!`.
//! A boolean prints `0` (true) or `1` (false) and sets the exit code
//! (`0` true, `1` false, `2` error). Plain arithmetic always exits `0`.
//! Quote `< > | &` so the shell does not eat them: `kalk "1 > 2"`.

use std::io::{self, BufRead, IsTerminal};

/// Shell-style truth: 0 (true) vs non-zero (false).
/// `Bool(true)` prints as `0`, `Bool(false)` as `1`.
enum EvalVal {
    Number(f64),
    Bool(bool),
}

impl EvalVal {
    fn is_true(&self) -> bool {
        match self {
            EvalVal::Number(n) => *n == 0.0,
            EvalVal::Bool(b) => *b,
        }
    }
    fn as_number(&self) -> f64 {
        match self {
            EvalVal::Number(n) => *n,
            EvalVal::Bool(true) => 0.0,
            EvalVal::Bool(false) => 1.0,
        }
    }
}

/// Format float: integers print without `.0`, rest via Display.
fn format_result(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    // Exact integer within i64 range -> print as int.
    if v.fract() == 0.0 && v >= i64::MIN as f64 && v <= i64::MAX as f64 {
        return format!("{}", v as i64);
    }
    format!("{v}")
}

/// If `s` is fully wrapped in one outer paren pair, return the inner slice.
fn strip_outer_parens(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if b.len() < 2 || b[0] != b'(' || b[b.len() - 1] != b')' {
        return None;
    }
    let mut depth = 0usize;
    for (i, &c) in b.iter().enumerate() {
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            // Closed the outer pair before the end -> not fully wrapped.
            if depth == 0 && i != b.len() - 1 {
                return None;
            }
        }
    }
    if depth != 0 {
        return None;
    }
    Some(&s[1..s.len() - 1])
}

/// Find first top-level (depth 0) occurrence of `pat`. Returns byte index.
fn find_top_level(s: &str, pat: &str) -> Option<usize> {
    let sb = s.as_bytes();
    let pb = pat.as_bytes();
    if pb.is_empty() || sb.len() < pb.len() {
        return None;
    }
    let mut depth = 0usize;
    let mut i = 0usize;
    while i + pb.len() <= sb.len() {
        match sb[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && &sb[i..i + pb.len()] == pb {
            // For `&&`/`||` require same-char run awareness is unnecessary:
            // meval never uses `&` or `|`, so any occurrence is logic.
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Collect top-level comparison ops: Vec<(byte_start, byte_len)>.
fn find_comparisons(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut ops = Vec::new();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 0 => {
                let rest = &b[i..];
                if rest.len() >= 2
                    && (rest[0..2] == [b'=', b'='][..]
                        || rest[0..2] == [b'!', b'='][..]
                        || rest[0..2] == [b'<', b'='][..]
                        || rest[0..2] == [b'>', b'='][..])
                {
                    ops.push((i, 2));
                    i += 2;
                } else if b[i] == b'<' || b[i] == b'>' || b[i] == b'=' {
                    ops.push((i, 1));
                    i += 1;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    ops
}

fn apply_cmp(op: &str, a: f64, b: f64) -> bool {
    match op {
        "==" | "=" => a == b,
        "!=" => a != b,
        "<" => a < b,
        "<=" => a <= b,
        ">" => a > b,
        ">=" => a >= b,
        _ => false,
    }
}

/// Recursive eval: logic/comparison here, pure arithmetic via meval.
fn eval_inner(expr: &str) -> Result<EvalVal, String> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err("empty expression".to_string());
    }

    // `||` (lowest precedence).
    if let Some(idx) = find_top_level(expr, "||") {
        let l = eval_inner(&expr[..idx])?;
        let r = eval_inner(&expr[idx + 2..])?;
        return Ok(EvalVal::Bool(l.is_true() || r.is_true()));
    }
    // `&&`.
    if let Some(idx) = find_top_level(expr, "&&") {
        let l = eval_inner(&expr[..idx])?;
        let r = eval_inner(&expr[idx + 2..])?;
        return Ok(EvalVal::Bool(l.is_true() && r.is_true()));
    }
    // Prefix `!` (logical NOT). Postfix `!` (factorial like `5!`)
    // does not start with `!`, so it still reaches meval untouched.
    if expr.starts_with('!') {
        let inner = eval_inner(&expr[1..])?;
        return Ok(EvalVal::Bool(!inner.is_true()));
    }

    // Comparisons (left-associative, e.g. `a == b`).
    let ops = find_comparisons(expr);
    if !ops.is_empty() {
        let mut nums = Vec::with_capacity(ops.len() + 1);
        let mut op_strs = Vec::with_capacity(ops.len());
        let mut prev = 0usize;
        for (start, len) in &ops {
            op_strs.push(&expr[*start..*start + *len]);
            let part = eval_inner(&expr[prev..*start])?;
            nums.push(part.as_number());
            prev = *start + *len;
        }
        let last = eval_inner(&expr[prev..])?;
        nums.push(last.as_number());
        let mut acc = nums[0];
        for (k, op) in op_strs.iter().enumerate() {
            let res = apply_cmp(op, acc, nums[k + 1]);
            acc = if res { 0.0 } else { 1.0 };
        }
        return Ok(EvalVal::Bool(acc == 0.0));
    }

    // Parenthesized boolean, e.g. `(1 > 2) || (3 == 3)`.
    // Pure arithmetic parens fall through to meval if not fully wrapped,
    // and even if wrapped we just recurse (harmless extra step).
    if let Some(inner) = strip_outer_parens(expr) {
        if inner.trim().is_empty() {
            return Err("empty expression".to_string());
        }
        // Only recurse when meval cannot handle it directly, so that
        // arithmetic like `(1+2)` keeps meval's exact behavior first.
        // If meval fails we retry as boolean below; here we check whether
        // the inside needs boolean handling by attempting meval on the whole.
        // Fast path: if the whole expr parses as arithmetic, use it.
        match meval::eval_str(expr) {
            Ok(v) => return Ok(EvalVal::Number(v)),
            Err(_) => return eval_inner(inner),
        }
    }

    match meval::eval_str(expr) {
        Ok(v) => Ok(EvalVal::Number(v)),
        Err(e) => Err(format!("{e}")),
    }
}

/// Eval single expression, trimmed. Empty => None (skip silently).
/// Returns (output, exit_code): bool true => ("0", 0), false => ("1", 1),
/// number => (formatted, 0).
fn eval_expr(expr: &str) -> Option<Result<(String, i32), String>> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }
    match eval_inner(expr) {
        Ok(EvalVal::Bool(true)) => Some(Ok(("0".to_string(), 0))),
        Ok(EvalVal::Bool(false)) => Some(Ok(("1".to_string(), 1))),
        Ok(EvalVal::Number(v)) => Some(Ok((format_result(v), 0))),
        Err(e) => Some(Err(e)),
    }
}

/// Eval + print one expression. Returns exit code (0 true/number, 1 false, 2 err).
fn run_one(expr: &str) -> i32 {
    match eval_expr(expr) {
        None => 0,
        Some(Ok((out, code))) => {
            println!("{out}");
            code
        }
        Some(Err(e)) => {
            eprintln!("kalk: error: {e}");
            2
        }
    }
}

/// Eval each non-empty stdin line. Exit 2 if any error, else 1 if any false.
fn run_piped(stdin: io::StdinLock<'_>) -> i32 {
    let mut saw_false = false;
    let mut saw_error = false;
    for line in stdin.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("kalk: stdin error: {e}");
                saw_error = true;
                continue;
            }
        };
        match eval_expr(&line) {
            None => {}
            Some(Ok((out, code))) => {
                println!("{out}");
                if code == 1 {
                    saw_false = true;
                }
            }
            Some(Err(e)) => {
                eprintln!("kalk: error in `{}`: {e}", line.trim());
                saw_error = true;
            }
        }
    }
    if saw_error {
        2
    } else {
        i32::from(saw_false)
    }
}

/// Interactive REPL for terminal stdin with no args.
fn run_repl(stdin: io::StdinLock<'_>) -> i32 {
    for line in stdin.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("kalk: stdin error: {e}");
                return 1;
            }
        };
        let expr = line.trim();
        if expr.is_empty() {
            continue;
        }
        if expr == "exit" || expr == "quit" {
            break;
        }
        match eval_expr(expr) {
            None => {}
            Some(Ok((out, _))) => println!("{out}"),
            Some(Err(e)) => eprintln!("kalk: error: {e}"),
        }
    }
    0
}

fn print_usage() -> i32 {
    eprintln!("usage: kalk <expr> | echo <expr> | kalk");
    eprintln!(" Crate pick: `meval` (MIT, zero-dep, tiny) over `evalexpr` (AGPL) / `fasteval` (older API).");
    2
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        std::process::exit(print_usage());
    }

    // `kalk 3-2`, `kalk 3 - 2`, `kalk "1 + 1"` all work: join with space.
    if !args.is_empty() {
        std::process::exit(run_one(&args.join(" ")));
    }

    let stdin = io::stdin();
    if stdin.is_terminal() {
        // No args + terminal -> REPL (Ctrl+C / "exit" to quit).
        std::process::exit(run_repl(stdin.lock()));
    } else {
        std::process::exit(run_piped(stdin.lock()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_arithmetic() {
        // (output, exit_code); plain numbers always exit 0.
        assert_eq!(eval_expr("1+1").unwrap().unwrap(), ("2".to_string(), 0));
        assert_eq!(eval_expr("3-2").unwrap().unwrap(), ("1".to_string(), 0));
        assert_eq!(eval_expr("2*3").unwrap().unwrap(), ("6".to_string(), 0));
        assert_eq!(eval_expr("7/2").unwrap().unwrap(), ("3.5".to_string(), 0));
        assert_eq!(
            eval_expr("2^10").unwrap().unwrap(),
            ("1024".to_string(), 0)
        );
        assert_eq!(
            eval_expr("(1+2)*3").unwrap().unwrap(),
            ("9".to_string(), 0)
        );
        // Modulo is arithmetic.
        assert_eq!(eval_expr("5 % 2").unwrap().unwrap(), ("1".to_string(), 0));
    }

    #[test]
    fn comparisons_shell_style() {
        // 0 = true, 1 = false.
        assert_eq!(eval_expr("1>2").unwrap().unwrap(), ("1".to_string(), 1));
        assert_eq!(eval_expr("2==2").unwrap().unwrap(), ("0".to_string(), 0));
        assert_eq!(eval_expr("2 != 2").unwrap().unwrap(), ("1".to_string(), 1));
        assert_eq!(eval_expr("1 < 2").unwrap().unwrap(), ("0".to_string(), 0));
        assert_eq!(eval_expr("2 >= 2").unwrap().unwrap(), ("0".to_string(), 0));
        assert_eq!(eval_expr("3 <= 2").unwrap().unwrap(), ("1".to_string(), 1));
        // Single `=` is an alias for `==`.
        assert_eq!(eval_expr("2=2").unwrap().unwrap(), ("0".to_string(), 0));
        // Modulo predicate for even/odd filtering.
        assert_eq!(
            eval_expr("4 % 2 == 0").unwrap().unwrap(),
            ("0".to_string(), 0)
        );
        assert_eq!(
            eval_expr("5 % 2 == 0").unwrap().unwrap(),
            ("1".to_string(), 1)
        );
    }

    #[test]
    fn logic_ops() {
        // 0 true, non-zero false: `1>2` is false(1), `2==2` is true(0).
        assert_eq!(
            eval_expr("(1>2) || (2==2)").unwrap().unwrap(),
            ("0".to_string(), 0)
        );
        assert_eq!(
            eval_expr("(1>2) && (2==2)").unwrap().unwrap(),
            ("1".to_string(), 1)
        );
        assert_eq!(eval_expr("!(1>2)").unwrap().unwrap(), ("0".to_string(), 0));
        assert_eq!(eval_expr("!0").unwrap().unwrap(), ("1".to_string(), 1));
    }

    #[test]
    fn functions_and_consts() {
        assert_eq!(
            eval_expr("sqrt(16)").unwrap().unwrap(),
            ("4".to_string(), 0)
        );
        assert_eq!(eval_expr("sin(0)").unwrap().unwrap(), ("0".to_string(), 0));
    }

    #[test]
    fn empty_is_skipped() {
        assert!(eval_expr("").is_none());
        assert!(eval_expr("   ").is_none());
    }

    #[test]
    fn invalid_is_error() {
        assert!(eval_expr("1+").unwrap().is_err());
    }
}

//! piper: pipe stdin words into a command template.
//!
//! # Quick Start
//!
//! ```text
//! cat test.txt | awk "{print $3}" | tail -n 1 | piper echo %1%
//! echo hello world | piper echo %1% %2% "[%*%]"
//! echo hello world | piper echo '$1' '$@'   # quote $ placeholders in sh/bash!
//! echo hello world | piper echo '$!'        # $! = all words, like $@
//! ... | piper --dry-run echo %1%   # show command without running
//! ... | piper                      # legacy: write temp script, print path
//! ```
//!
//! Every piped invocation overwrites the recall slot `%TEMP%\piper`
//! (`$TMPDIR/piper` on unix) with the raw stdin bytes. Later, anywhere:
//! ```text
//! piper $!        # print last piped output (no pipe needed)
//! piper echo $!   # echo it via a command (quote '$!' in sh/bash)
//! ```

mod parse;
mod run;
mod substitute;

use std::io::{IsTerminal, Read, Write};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let template: Vec<String> = std::env::args().skip(1).collect();

    if template.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }
    if template.iter().any(|a| a == "--version" || a == "-V") {
        println!("piper {VERSION}");
        return;
    }

    let (dry_run, template) = match template.first().map(String::as_str) {
        Some("--dry-run") => (true, template[1..].to_vec()),
        _ => (false, template),
    };

    // No pipe attached (interactive console): don't block waiting for
    // EOF — a bare `piper $!` must recall instead of hanging.
    // (Git Bash/mintty: stdin is a held-open pipe there, so with no
    // pipe use `piper $! </dev/null`.)
    let mut buf = Vec::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin()
            .lock()
            .read_to_end(&mut buf)
            .expect("piper: failed to read stdin");
    }

    // Every piped input overwrites the recall slot %TEMP%/piper.
    if !buf.is_empty() {
        run::save_last_output(&buf);
    }
    let effective = resolve_input(buf, run::load_last_output());
    let text = String::from_utf8_lossy(&effective);
    let data = parse::parse_input(&text);

    if template.is_empty() {
        // Legacy fallback: no template -> temp script path on stdout.
        let path = run::write_fallback_script(&data);
        println!("{}", path.to_string_lossy());
        return;
    }

    if is_bare_bang(&template) {
        // Recall: print the raw bytes (exact last output), not words.
        if effective.is_empty() {
            eprintln!(
                "piper: no piped input and nothing saved in {} yet — pipe something into piper first",
                run::cache_path().to_string_lossy()
            );
            std::process::exit(1);
        }
        std::io::stdout()
            .write_all(&effective)
            .expect("piper: failed to write stdout");
        return;
    }

    let argv = substitute::substitute_args_with_stdin(&template, &data, &effective);

    if dry_run {
        println!("{}", argv.join(" "));
        return;
    }

    run::exec_shell(&argv);
}

/// Pick what placeholders resolve from: the current pipe when there is
/// one, otherwise the saved recall slot.
fn resolve_input(piped: Vec<u8>, cached: Option<Vec<u8>>) -> Vec<u8> {
    if !piped.is_empty() {
        piped
    } else {
        cached.unwrap_or_default()
    }
}

/// True when the template is just the recall placeholder: `piper $!`.
fn is_bare_bang(template: &[String]) -> bool {
    matches!(template, [a] if a == "$!" || a == "${!}" || a == "%!%")
}

/// Print help to stdout.
fn print_help() {
    println!(
        "piper {VERSION} - pipe stdin words into a command template\n\
         \n\
         USAGE:\n\
         \x20 <pipe> | piper <command> [args with placeholders...]\n\
         \x20 <pipe> | piper --dry-run <command> [...]\n\
         \x20 <pipe> | piper   # legacy: write temp script, print path\n\
         \x20 piper $!         # print last piped output (no pipe needed)\n\
         \n\
         PLACEHOLDERS (substituted from piped stdin):\n\
          \x20 %1% %2% ...   word N (1-based, flat across lines)\n\
          \x20 %*% %!%       all words joined with spaces\n\
          \x20 %line1%       raw line N (also %LINE_1%, case-insensitive)\n\
          \x20 $1 $2 ... ${{1}}  same words, unix style (quote: '$1')\n\
          \x20 $@ $* $! ${{@}} ${{*}} ${{!}}  all words (pipe output)\n\
          \x20 $line1 ${{line1}}    raw line N\n\
          \x20 %% $$(literal)  escape to literal % / $\n\
          \x20 Unknown %NAME% / $NAME pass through for child shell.\n\
          \n\
           PER-OCCURRENCE TRANSFORM:\n\
            \x20 $1=(cmd) / %1%=(cmd) / $@=(cmd) / $line1=(cmd) ...\n\
            \x20 run cmd with the piped stdin, replace ONLY that occurrence\n\
            \x20 with cmd's output (trailing newlines stripped). Others\n\
            \x20 stay normal: echo hi hi | piper echo '$1=(awk ''{{print $2}}'')' '$1'\n\
            \x20 inside cmd, bare/double-quoted piper placeholders expand\n\
            \x20 from the same input ($1=(kalk $1*5) computes from word 1)\n\
            \x20 while single-quoted spans stay verbatim for the child\n\
            \x20 shell (awk ''{{print $2}}'' keeps its $2). Example:\n\
          \x20 \x20 echo hello hello | piper echo '$1=(awk ''{{print $2}}'' | sed ''s/hello/world/g'')' '$1'\n\
          \x20 \x20 -> world hello\n\
         \n\
         RECALL SLOT:\n\
         \x20 every piped invocation overwrites %TEMP%\\piper ($TMPDIR/piper)\n\
         \x20 with the raw stdin bytes.\n\
         \x20 piper $! / piper %!%   print last saved output (or current\n\
         \x20 \x20 pipe's output for ... | piper $!)\n\
         \x20 piper echo $!          echo it via a command (quote '$!' in sh)\n\
         \x20 with no pipe, $1 $@ $! ... all resolve from the saved output.\n\
         \n\
         QUOTING:\n\
         \x20 sh/bash expands $1 $@ $! BEFORE piper sees them, so quote:\n\
         \x20 ... | piper echo '$1'  ('$@' / '$!' for all words)\n\
         \x20 cmd expands %1% %*% before piper, so prefer $1 / $! on Windows.\n\
         \x20 Use --dry-run to check what piper received.\n\
         \x20 NUL bytes (e.g. from grep -z/--null-data) are treated as\n\
         \x20 line breaks so they cannot break the child command.\n\
         \n\
         EXAMPLES:\n\
         \x20 cat test.txt | piper echo %1%\n\
         \x20 echo hello world | piper echo Got:%1%,%2% all=[%*%]\n\
         \x20 echo hello world | piper echo '$1' '$@'\n\
         \x20 echo hello world | piper echo '$!'   # echo all piped words\n\
         \x20 echo hello world | piper '$!'        # print the pipe output\n\
         \x20 piper '$!'                           # recall it later, anywhere\n\
         \n\
         NOTE: child cannot set parent %1% after &&, and cmd expands\n\
         %vars% before piper runs. So pass the command TO piper;\n\
         piper substitutes then runs it via cmd /C (win) or sh -c."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_bang_detection() {
        assert!(is_bare_bang(&args(&["$!"])));
        assert!(is_bare_bang(&args(&["${!}"])));
        assert!(is_bare_bang(&args(&["%!%"])));
        assert!(!is_bare_bang(&args(&["echo", "$!"])));
        assert!(!is_bare_bang(&args(&["$@", "$!"])));
        assert!(!is_bare_bang(&args(&["$1"])));
    }

    #[test]
    fn resolve_prefers_pipe_over_cache() {
        assert_eq!(
            resolve_input(b"new".to_vec(), Some(b"old".to_vec())),
            b"new".to_vec()
        );
        assert_eq!(
            resolve_input(vec![], Some(b"old".to_vec())),
            b"old".to_vec()
        );
        assert!(resolve_input(vec![], None).is_empty());
    }
}

//! `lp` - a tiny loop engine for Windows.
//!
//! Iterates over a source (integer range, file lines, or directory files) and
//! runs a command block once per value, substituting the loop variable.
//!
//! ```text
//! lp --for x --in 5 {echo $x}
//! lp --for line --in file.txt {echo $line | grep "a"}
//! lp --for filepath --in F:/Script/ {lp --for line --in $filepath {echo $line}}
//! ```

mod cli;
mod exec;
mod source;
mod substitute;

use std::process::ExitCode;

const USAGE: &str = "\
usage: lp for <var> --in <source> [--enumerate <index>] [--if ( <command> )] [--while ( <command> )] { <command> }
        lp for <var> --in_cmd ( <command> ) [--enumerate <index>] [--if ( <command> )] [--while ( <command> )] { <command> }

  <source> may be:
    an integer N           iterate 0..=N
    a range A..B / A..=B   iterate the range
    a file path            iterate its lines
    a directory path       iterate its files (full paths)

  'in' and 'in_cmd' are accepted without the leading '--'.

  --in_cmd runs a command and iterates its stdout lines.

  --enumerate binds a second variable to the 1-based iteration index.

  --if runs <command> per value (with $var substituted) and runs the block
    only when it exits 0. Output of the condition is discarded.

  --while runs <command> per value (with $var substituted) and stops the loop
    at the first value where it exits non-0. Forces sequential execution.

  <var> and <index> are available inside the block and the conditions
  as $name, ${name} or %name%.
";

fn main() -> ExitCode {
    let command = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let command = command.trim();

    if command.is_empty() || command == "--help" || command == "-h" {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    match exec::run_invocation(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lp: {err}");
            ExitCode::FAILURE
        }
    }
}

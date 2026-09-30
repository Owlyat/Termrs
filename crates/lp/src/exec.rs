//! Execution of a parsed loop invocation.
//!
//! Each value is substituted into the block and the resulting command is run.
//! Commands run in parallel, but each command's output is captured and replayed
//! in source order, so parallel execution never interleaves or reorders output.
//! `--if (cond)` skips values whose condition exits non-zero, while
//! `--while (cond)` stops the loop at the first value whose condition exits
//! non-zero. `--while` forces sequential execution so break order is exact.
//! Blocks that are themselves `lp` invocations are handled in-process so that
//! nested loops do not depend on the outer shell understanding braces.

use std::io::Write;
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
use std::fs::File;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use rayon::prelude::*;

use crate::cli;
use crate::source;
use crate::substitute;

/// Captured stdout and stderr of one command, kept for ordered replay.
struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run one full `lp` invocation, iterating its block over its source.
pub fn run_invocation(command: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut stdout = raw_stdio(false)?;
        let mut stderr = raw_stdio(true)?;
        run_into(command, &mut stdout, &mut stderr)
    }
    #[cfg(not(windows))]
    {
        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();
        run_into(command, &mut stdout, &mut stderr)
    }
}

/// Duplicate a standard stream handle as a `File` so captured bytes are written
/// verbatim, bypassing Rust's UTF-8 console conversion on Windows.
#[cfg(windows)]
fn raw_stdio(stderr: bool) -> Result<File, String> {
    let handle = if stderr {
        std::io::stderr().as_raw_handle()
    } else {
        std::io::stdout().as_raw_handle()
    };
    let owned = unsafe { BorrowedHandle::borrow_raw(handle) }
        .try_clone_to_owned()
        .map_err(|e| format!("failed to duplicate stdio handle: {e}"))?;
    Ok(File::from(owned))
}

/// Run an invocation, replaying each command's captured output to `out`/`err`.
fn run_into<O: Write, E: Write>(command: &str, out: &mut O, err: &mut E) -> Result<(), String> {
    let invocation = cli::parse(command)?;
    let values = source::values(&invocation.source)?;

    if invocation.while_cond.is_some() {
        return run_sequential(&invocation, values, out, err);
    }

    let jobs = render_jobs(&invocation, values);

    let results: Vec<Result<Captured, String>> = jobs
        .par_iter()
        .map(|job| {
            if let Some(filter) = &job.filter
                && !eval_filter(&filter.command, filter.negate)? {
                    return Ok(Captured {
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
            capture_command(&job.block)
        })
        .collect();

    for result in results {
        let captured = result?;
        out.write_all(&captured.stdout)
            .map_err(|e| format!("failed to write output: {e}"))?;
        err.write_all(&captured.stderr)
            .map_err(|e| format!("failed to write output: {e}"))?;
    }

    Ok(())
}

/// Sequential loop honouring `--while` (break) and `--if` (skip), in source order.
fn run_sequential<O: Write, E: Write>(
    invocation: &cli::Invocation,
    values: Vec<String>,
    out: &mut O,
    err: &mut E,
) -> Result<(), String> {
    for (index, value) in values.into_iter().enumerate() {
        if let Some(cond) = &invocation.while_cond {
            let rendered = substitute_template(&cond.command, invocation, index, &value);
            if !eval_filter(&rendered, cond.negate)? {
                break;
            }
        }
        if let Some(cond) = &invocation.filter {
            let rendered = substitute_template(&cond.command, invocation, index, &value);
            if !eval_filter(&rendered, cond.negate)? {
                continue;
            }
        }
        let block = substitute_template(&invocation.block, invocation, index, &value);
        let block = block.trim().to_string();
        if block.is_empty() {
            continue;
        }
        let captured = capture_command(&block)?;
        out.write_all(&captured.stdout)
            .map_err(|e| format!("failed to write output: {e}"))?;
        err.write_all(&captured.stderr)
            .map_err(|e| format!("failed to write output: {e}"))?;
    }
    Ok(())
}

/// One rendered job for the parallel (`--if`-only) path.
struct Job {
    block: String,
    filter: Option<JobFilter>,
}

/// A substituted `--if` condition with its negation flag preserved.
struct JobFilter {
    command: String,
    negate: bool,
}

/// Substitute the loop variable and optional index into each value, dropping
/// empty blocks. The index variable is filled first so a literal `$index`
/// occurring inside an iterated value is never rewritten.
fn render_jobs(invocation: &cli::Invocation, values: Vec<String>) -> Vec<Job> {
    values
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| {
            let block = substitute_template(&invocation.block, invocation, index, &value);
            let block = block.trim().to_string();
            if block.is_empty() {
                return None;
            }
            let filter = invocation.filter.as_ref().map(|cond| JobFilter {
                command: substitute_template(&cond.command, invocation, index, &value),
                negate: cond.negate,
            });
            Some(Job { block, filter })
        })
        .collect()
}

/// Apply `--enumerate` then the loop variable to a template string.
fn substitute_template(
    template: &str,
    invocation: &cli::Invocation,
    index: usize,
    value: &str,
) -> String {
    let staged = match &invocation.enumerate {
        Some(name) => substitute::apply(template, name, &(index + 1).to_string()),
        None => template.to_string(),
    };
    substitute::apply(&staged, &invocation.var, value)
}

/// Run a single (already substituted) command and capture its output.
fn capture_command(command: &str) -> Result<Captured, String> {
    if is_lp_invocation(command) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_into(command, &mut stdout, &mut stderr)?;
        return Ok(Captured { stdout, stderr });
    }

    let mut child = Command::new("cmd");
    #[cfg(windows)]
    child.raw_arg("/C").raw_arg(command);
    #[cfg(not(windows))]
    child.arg("/C").arg(command);

    let output = child
        .output()
        .map_err(|e| format!("failed to run '{command}': {e}"))?;

    Ok(Captured {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Evaluate a rendered condition, applying `not` negation.
///
/// An empty render counts as false before negation, so `--if not` runs the
/// block while plain `--if` skips it.
fn eval_filter(rendered: &str, negate: bool) -> Result<bool, String> {
    if rendered.trim().is_empty() {
        return Ok(negate);
    }
    let passed = run_condition(rendered)?;
    Ok(if negate { !passed } else { passed })
}

/// Run a condition command (already substituted) and report whether it exited 0.
///
/// Exit 0 means pass. A clean non-zero exit (no stderr) means filter-false.
/// A non-zero exit with stderr means the condition itself errored, so return
/// `Err` with the stderr text instead of silently skipping every value.
fn run_condition(condition: &str) -> Result<bool, String> {
    if is_lp_invocation(condition) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_into(condition, &mut stdout, &mut stderr)?;
        return Ok(true);
    }

    let mut child = Command::new("cmd");
    #[cfg(windows)]
    child.raw_arg("/C").raw_arg(condition);
    #[cfg(not(windows))]
    child.arg("/C").arg(condition);

    let output = child
        .output()
        .map_err(|e| format!("failed to run '{condition}': {e}"))?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return Err(format!("condition '{condition}' failed: {stderr}"));
    }
    Ok(false)
}

/// Whether a command is another `lp` invocation handled in-process.
fn is_lp_invocation(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or("");
    let stem = Path::new(first)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(first);
    stem.eq_ignore_ascii_case("lp")
}

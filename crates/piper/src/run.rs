//! Run substituted template via shell, or fall back to temp script file.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use crate::parse::Parsed;

/// Execute substituted argv via system shell. Never returns on success path:
/// exits process with child status code.
pub fn exec_shell(argv: &[String]) -> ! {
    // Defensive: `Command::arg` rejects interior NUL bytes outright
    // ("nul byte found in provided data"). stdin is already NUL-cleaned
    // in parse, but substituted env values could still carry one, so
    // strip any leftovers instead of crashing the spawn.
    let cmdline = argv.join(" ").replace('\0', "");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&cmdline);
        c
    };
    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&cmdline);
        c
    };
    let status = cmd.status().unwrap_or_else(|e| {
        eprintln!("piper: failed to spawn shell: {e}");
        std::process::exit(127);
    });
    std::process::exit(status.code().unwrap_or(1));
}

/// Execute a per-occurrence transform `cmd` with `input` as its stdin,
/// capturing stdout. Trailing `\r`/`\n` are stripped like `$(...)`.
/// Never panics: spawn/write failures yield empty string.
pub fn run_transform(cmd: &str, input: &[u8]) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if cmd.trim().is_empty() {
        return String::new();
    }
    #[cfg(target_os = "windows")]
    let mut child = match Command::new("cmd")
        .arg("/C")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    #[cfg(not(target_os = "windows"))]
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
        // stdin closed on drop.
    }
    let out = child.wait_with_output().map(|o| o.stdout).unwrap_or_default();
    // Strip NUL (would break later spawn) and trailing newlines like $(...).
    let s = String::from_utf8_lossy(&out).replace('\0', "");
    s.trim_end_matches(['\r', '\n']).to_string()
}
pub fn cache_path() -> PathBuf {
    std::env::temp_dir().join("piper")
}

/// Best-effort overwrite of the recall slot with raw piped stdin.
/// Never fails the caller: cache write errors are silently ignored.
pub fn save_last_output(buf: &[u8]) {
    save_last_output_to(&cache_path(), buf);
}

/// Load the recall slot saved by a previous invocation, if any.
pub fn load_last_output() -> Option<Vec<u8>> {
    load_last_output_from(&cache_path())
}

fn save_last_output_to(path: &std::path::Path, buf: &[u8]) {
    let _ = std::fs::write(path, buf);
}

fn load_last_output_from(path: &std::path::Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}
/// Legacy fallback (no template args): write temp script exporting
/// `PIPER_*` vars, print its path. Safe names work with
/// `call` (cmd) / `source` (sh); numeric `%1%` can't be set from child.
pub fn write_fallback_script(data: &Parsed) -> PathBuf {
    let mut path = PathBuf::from(std::env::temp_dir());
    if cfg!(target_os = "windows") {
        path.push("piper.cmd");
    } else {
        path.push("piper.sh");
    }
    let mut file = File::create(&path).expect("piper: cannot create temp script");

    if !cfg!(target_os = "windows") {
        writeln!(file, "#!/bin/sh").unwrap();
        writeln!(file, "# source this file: . \"{}\"", path.to_string_lossy()).unwrap();
        for (idx, line) in data.lines.iter().enumerate() {
            writeln!(file, "PIPER_LINE_{}={}", idx + 1, escape_sh(line)).unwrap();
        }
        for (idx, word) in data.words.iter().enumerate() {
            writeln!(file, "PIPER_{}={}", idx + 1, escape_sh(word)).unwrap();
        }
        writeln!(file, "PIPER_ALL={}", escape_sh(&data.words.join(" "))).unwrap();
        writeln!(
            file,
            "PIPER_COUNT={} PIPER_LINE_COUNT={}",
            data.words.len(),
            data.lines.len()
        )
        .unwrap();
    } else {
        writeln!(file, "@echo off").unwrap();
        writeln!(file, "rem call this file: call \"{}\"", path.to_string_lossy()).unwrap();
        for (idx, line) in data.lines.iter().enumerate() {
            writeln!(file, "set \"PIPER_LINE_{}={}\"", idx + 1, escape_batch(line)).unwrap();
        }
        for (idx, word) in data.words.iter().enumerate() {
            writeln!(file, "set \"PIPER_{}={}\"", idx + 1, escape_batch(word)).unwrap();
        }
        writeln!(file, "set \"PIPER_ALL={}\"", escape_batch(&data.words.join(" "))).unwrap();
        writeln!(file, "set \"PIPER_COUNT={}\"", data.words.len()).unwrap();
        writeln!(file, "set \"PIPER_LINE_COUNT={}\"", data.lines.len()).unwrap();
    }
    path
}

/// Batch-safe value: double `%`, drop `\r`/`\n`, double inner quotes.
fn escape_batch(s: &str) -> String {
    s.replace('%', "%%")
        .replace('\r', "")
        .replace('\n', "")
        .replace('"', "\"\"")
}

/// Sh-safe value: double-quoted with `\`, `"`, `$`, backtick escaped.
fn escape_sh(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("piper-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn recall_slot_roundtrip_and_overwrite() {
        let p = scratch_path("recall");
        // Missing file -> None.
        assert!(load_last_output_from(&p).is_none());
        save_last_output_to(&p, b"https://example.com/one-piece\0");
        // Raw bytes preserved exactly (NUL kept in the slot).
        assert_eq!(
            load_last_output_from(&p).as_deref(),
            Some(b"https://example.com/one-piece\0".as_slice())
        );
        // Overwrite replaces previous content.
        save_last_output_to(&p, b"second");
        assert_eq!(
            load_last_output_from(&p).as_deref(),
            Some(b"second".as_slice())
        );
        std::fs::remove_file(&p).ok();
    }
}

//! AI assistant bridge: run a configured CLI with a prompt on stdin and
//! extract a shell command from its answer.
//!
//! Any CLI that reads a prompt on stdin and prints the answer on stdout works
//! (`ollama run llama3`, `llm`, `aichat`, `sgpt`, ...). The reply is
//! normalized: a fenced code block is preferred, otherwise the last
//! non-empty line, with a leading `$ ` stripped.
//!
//! The assistant works towards a goal: create a single shell command that
//! satisfies the user's need. The first prompt states that goal plus the
//! user's request — it does NOT dump the saved commands. Instead the model
//! queries for context with ONE JSON object per turn, and termrs runs the
//! query locally and feeds the result back:
//!
//! - `{"action":"db","query":"<keywords>","limit":10}` → saved commands match
//! - `{"action":"which","tools":["<name>"]}` → TOOL REPORT (FOUND/MISSING)
//! - `{"action":"help","tool":"<name>"}` → usage text (`/?` on Windows,
//!   `--help`/`man` elsewhere; the tool itself is NEVER executed)
//! - `{"action":"answer","command":"<shell command>"}` → final answer
//!
//! Running bare commands is disabled: the ONLY command that ever runs is the
//! final answer, typed into the pane for the user to confirm. A fenced block
//! tagged `answer` (```answer) is also accepted as final.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default limit on how long the AI CLI may run per turn before it is
/// killed. Overridable per run (and via `[ai] timeout_s`).
#[cfg(test)]
pub const DEFAULT_AI_TIMEOUT: Duration = Duration::from_secs(60);

/// Hard limit for a single AI-driven probe (`exec`) command.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum AI turns per question (1 initial + follow-ups after each probe).
const MAX_TURNS: usize = 6;

/// How much probe output is fed back to the model per turn.
const PROBE_CHARS: usize = 4000;

/// Nudge fed back when the model repeats an already-answered query
/// verbatim (small models echo the example every turn). Points it at the
/// results above instead of burning another turn re-running.
const REPEAT_NUDGE: &str = "\nTOOL RESULT: you already asked exactly that -- the answer is in the results above. Do NOT repeat queries; use what you have and reply with the next NEW query or the final answer.\n";

/// True when `sig` was already answered (records it otherwise), so exact
/// repeats are nudged instead of re-executed.
fn already_asked(seen: &mut Vec<String>, sig: String) -> bool {
    if seen.iter().any(|s| s == &sig) {
        true
    } else {
        seen.push(sig);
        false
    }
}

/// Goal-first instructions for the model: create a command for the target
/// shell satisfying the user's need, querying the saved-command database
/// and the machine's CLI tools on demand with ONE JSON object per turn
/// instead of receiving every saved command up front. The model may never
/// run commands: only `help` queries execute (as `<tool> /?` / `--help`),
/// and the final answer is merely typed into the pane for confirmation.
pub const PROBE_PROTOCOL: &str = "Goal: create a single shell command for the target shell (see the Target shell line) satisfying the user's need. You may first gather context with exactly ONE JSON object per turn: {\"action\":\"db\",\"query\":\"<keywords>\",\"limit\":10} searches the saved command database, or {\"action\":\"which\",\"tools\":[\"<name>\"]} to check whether a CLI tool is installed, or {\"action\":\"help\",\"tool\":\"<name>\"} to read a tool's usage (termrs runs `<name> /?` on Windows or `<name> --help` elsewhere; the tool itself is NEVER executed, so interactive programs are safe). Each reply returns the query result; then continue or finish with {\"action\":\"answer\",\"command\":\"<single shell command>\"}. Alternatively give the final command as ```answer <command> ```. Rules: you cannot run commands, and termrs will not run anything for you except help queries -- the ONLY command that ever runs is your final answer, typed into the pane for the user to confirm. Final answer must be one runnable shell line for the target shell satisfying the initial need; prefer a saved command verbatim when one fits.";

 /// Ask the configured CLI, letting the model query the saved `saved`
/// snapshot with `{"action":"db",...}` instead of receiving every command in
/// the first prompt. The snapshot is only searched — rows are fed back solely
/// for queries the model actually makes.
#[cfg(test)]
pub fn run_with_db(
    ai_command: &str,
    lead_in: &str,
    question: &str,
    saved: &[crate::commands_db::SavedCommand],
) -> Result<String, String> {
    run_full(ai_command, lead_in, question, saved, DEFAULT_AI_TIMEOUT)
}

/// Convenience wrapper for tests: run with no saved-command snapshot.
#[cfg(test)]
pub fn run(ai_command: &str, lead_in: &str, question: &str) -> Result<String, String> {
    run_with_db(ai_command, lead_in, question, &[])
}

/// Full agentic loop with an explicit per-turn `timeout` for the AI CLI.
/// Each of the [`MAX_TURNS`] turns gets its own `timeout` budget.
pub fn run_full(
    ai_command: &str,
    lead_in: &str,
    question: &str,
    saved: &[crate::commands_db::SavedCommand],
    timeout: Duration,
) -> Result<String, String> {
    if ai_command.trim().is_empty() {
        return Err("no [ai] command configured".into());
    }
    let system = build_system(lead_in, saved.len());
    let mut conversation = format!("{system}\n\nUser need: {question}\n");
    let mut last_legacy = String::new();
    // Signatures of queries already answered ("db:git:10"). An exact repeat
    // gets a nudge instead of a re-run: small models echo the example query
    // every turn, and re-running only grows the prompt.
    let mut seen: Vec<String> = Vec::new();

    for turn in 0..MAX_TURNS {
        let raw = run_raw(ai_command, &conversation, question, timeout)?;
        log::info!("ai turn {turn}: {} chars", raw.len());
        match parse_probe(&raw) {
            Probe::Answer(cmd) => {
                if cmd.is_empty() {
                    return Err("AI returned an empty answer".into());
                }
                return Ok(cmd);
            }
            Probe::Db { query, limit } => {
                if already_asked(&mut seen, format!("db:{query}:{limit}")) {
                    conversation.push_str(REPEAT_NUDGE);
                    continue;
                }
                let hits = search_saved(saved, &query, limit);
                let total = count_saved_matches(saved, &query);
                let shown = format_saved(&hits);
                log::info!("ai db query: {query:?} limit={limit} -> {total} match(es)");
                conversation.push_str(&format!(
                    "\nAssistant query: saved commands for {query:?}\nDATABASE RESULT: {total} match(es), showing {} (query={query:?}, limit={limit}):\n{shown}\nContinue towards the initial need: reply with the next JSON query or the final answer.\n",
                    hits.len()
                ));
            }
            Probe::Which(tools) => {
                if tools.is_empty() {
                    conversation.push_str(
                        "\nAssistant probe: which (no tools listed).\nTOOL RESULT: no tools listed; reply with a new probe or the final answer.\n",
                    );
                    continue;
                }
                let mut sorted = tools.clone();
                sorted.sort();
                if already_asked(&mut seen, format!("which:{}", sorted.join(","))) {
                    conversation.push_str(REPEAT_NUDGE);
                    continue;
                }
                let report = which_report(&tools);
                log::info!("ai which probe: {tools:?} -> {} chars", report.len());
                conversation.push_str(&format!(
                    "\nAssistant probe: which {tools:?}\nTOOL RESULT:\n{report}\nContinue towards the initial need: reply with the next JSON query or the final answer.\n"
                ));
            }
            Probe::Help { tool } => {
                if tool.trim().is_empty() {
                    conversation.push_str(
                        "\nAssistant probe: help (no tool named).\nTOOL RESULT: no tool named; reply with a new probe or the final answer.\n",
                    );
                    continue;
                }
                if already_asked(&mut seen, format!("help:{}", tool.trim())) {
                    conversation.push_str(REPEAT_NUDGE);
                    continue;
                }
                let out = help_probe(&tool);
                log::info!("ai help probe: {tool:?} -> {} chars", out.len());
                conversation.push_str(&format!(
                    "\nAssistant probe: help {tool}\nTOOL RESULT (usage text only; the tool was NOT executed):\n{out}\nContinue towards the initial need: reply with the next JSON query or the final answer.\n"
                ));
            }
            Probe::Denied => {
                log::info!("ai exec denied (bare command, never run)");
                conversation.push_str(
                    "\nTOOL RESULT: REFUSED -- termrs never runs commands for you. Only help queries execute (as `<tool> /?` on Windows or `<tool> --help` elsewhere). To learn a tool's usage use {\"action\":\"help\",\"tool\":\"<name>\"}. Answer with the final command; it will only be typed into the pane for the user to confirm.\n",
                );
            }
            Probe::Legacy => {
                let cmd = extract_command(&raw);
                if cmd.is_empty() {
                    // No usable command and no probe: keep the raw text so a
                    // later turn (or the caller) can see what happened.
                    last_legacy = raw.trim().to_string();
                    if turn + 1 >= MAX_TURNS {
                        break;
                    }
                    conversation.push_str(
                        "\nTOOL RESULT: no command or probe found in your last reply. Reply with a JSON probe or the final answer.\n",
                    );
                    continue;
                }
                return Ok(cmd);
            }
        }
        if conversation.len() > 16_000 {
            // Keep the system head and the recent tail; the middle is the
            // oldest probe traffic, least relevant for the next turn.
            let tail = conversation[conversation.len() - 12_000..].to_string();
            conversation = format!("{system}\n\n[... truncated ...]\n{tail}");
        }
    }
    if last_legacy.is_empty() {
        Err("AI kept probing without giving a final command".into())
    } else {
        Err(format!("AI returned no command: {}", truncate(&last_legacy, 300)))
    }
}

/// Build the first prompt: the goal plus OS/ARCH context plus
/// [`PROBE_PROTOCOL`], unless the configured lead-in already teaches the
/// JSON query protocol (then only the environment line is added, so
/// small-context models are not fed the same instructions twice).
/// `saved_count` tells the model a database exists without listing it.
fn build_system(lead_in: &str, saved_count: usize) -> String {
    let lower = lead_in.to_ascii_lowercase();
    let probe_aware = lower.contains("which") && lower.contains("action");
    let env = format!(
        "Environment: OS={} ARCH={}.",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let db_hint = if saved_count > 0 {
        format!(" {saved_count} saved command(s) are stored and searchable via the db query.")
    } else {
        String::new()
    };
    if probe_aware {
        format!("{lead_in}\n\n{env}{db_hint}")
    } else {
        format!("{lead_in}\n\n{PROBE_PROTOCOL}\n\n{env}{db_hint}")
    }
}

/// One AI invocation: substitute `{prompt}`/`{question}` (or stdin) with the
/// full `conversation`, run the CLI, strip ANSI, and return its raw reply.
/// The CLI is killed after `timeout`.
fn run_raw(
    ai_command: &str,
    conversation: &str,
    question: &str,
    timeout: Duration,
) -> Result<String, String> {
    run_raw_inner(ai_command, conversation, question, timeout, false)
}

/// Inner single invocation; `folded` retries once with an ASCII-folded
/// prompt when the CLI crashes at startup with no output (some llama.cpp
/// builds die with 0xC0000409 on non-ASCII argv). Never recurses twice.
fn run_raw_inner(
    ai_command: &str,
    conversation: &str,
    question: &str,
    timeout: Duration,
    folded: bool,
) -> Result<String, String> {
    let prompt = format!("{conversation}\n");
    let (substituted, has_placeholder) = apply_placeholders(ai_command, &prompt, question);
    let line = fix_program_path(&substituted);
    log::info!("ai run: {line} (placeholder={has_placeholder})");

    let mut cmd = shell(&line);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if !has_placeholder {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("run {line:?}: {e}"))?;
    if !has_placeholder
        && let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes());
            // Drop closes stdin so CLIs that read to EOF proceed.
        }
    let (stdout, stderr, status, timed_out) = collect_with(child, timeout)?;
    if timed_out {
        return Err(format!(
            "AI command timed out after {}s [{line}]",
            timeout.as_secs()
        ));
    }
    if !status.map(|s| s.success()).unwrap_or(false) {
        // Startup crash with no output (e.g. llama.cpp 0xC0000409 on
        // non-ASCII argv): retry once with an ASCII-folded prompt.
        if !folded && looks_like_startup_crash(&status, &stdout, &stderr) {
            let folded_conversation = fold_to_ascii(conversation);
            let folded_question = fold_to_ascii(question);
            if folded_conversation != conversation || folded_question != question {
                log::warn!(
                    "ai run crashed with no output (exit {}); retrying with ASCII-folded prompt",
                    status
                        .map(|s| s
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".into()))
                        .unwrap_or_else(|| "unknown".into())
                );
                return run_raw_inner(
                    ai_command,
                    &folded_conversation,
                    &folded_question,
                    timeout,
                    true,
                );
            }
        }
        let code = status
            .map(|s| {
                s.code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into())
            })
            .unwrap_or_else(|| "unknown".into());
        let out = String::from_utf8_lossy(&stdout);
        let err = String::from_utf8_lossy(&stderr);
        // Strip ANSI (llama-cli prints a coloured banner) before reporting.
        let plain_out = crate::ansi::strip_ansi_markers(&out).0;
        // Log the full response: with small local models the CLI often exits
        // non-zero while the reason sits on stdout, not stderr.
        log::warn!(
            "ai run failed: exit {code} [{line}]\n--- stdout ---\n{}\n--- stderr ---\n{}",
            truncate(plain_out.trim(), 2000),
            truncate(err.trim(), 2000)
        );
        return Err(format!(
            "AI command failed (exit {code}) [{line}]\n--- response (stdout) ---\n{}\n--- error (stderr) ---\n{}",
            truncate(plain_out.trim(), 800),
            truncate(err.trim(), 800)
        ));
    }
    let answer = String::from_utf8_lossy(&stdout);
    // Strip ANSI (llama-cli prints a coloured banner) before extracting.
    let plain = crate::ansi::strip_ansi_markers(&answer).0;
    log::debug!("ai reply ({} chars): {}", plain.len(), truncate(plain.trim(), 1000));
    Ok(plain)
}

/// True when the CLI died from an OS exception (negative exit code on
/// Windows, e.g. 0xC0000409 stack overrun) before printing anything.
/// That signature means "crashed at startup", never "model said no".
fn looks_like_startup_crash(
    status: &Option<std::process::ExitStatus>,
    stdout: &[u8],
    stderr: &[u8],
) -> bool {
    let exception = status
        .map(|s| s.code().map(|c| c < 0).unwrap_or(false))
        .unwrap_or(false);
    exception
        && stdout.iter().all(|b| b.is_ascii_whitespace())
        && stderr.iter().all(|b| b.is_ascii_whitespace())
}

/// Transliterate to ASCII (`ü` → `u`, `—` → `--`, `…` → `...`).
/// Fallback for CLIs whose argv handling crashes on non-ASCII bytes;
/// only used for the single folded retry, never the first attempt.
fn fold_to_ascii(s: &str) -> String {
    deunicode::deunicode(s)
}

/// `(stdout, stderr, exit status, timed_out)` from a finished CLI child.
type CollectResult = (Vec<u8>, Vec<u8>, Option<std::process::ExitStatus>, bool);

/// Read the child's stdout/stderr on background threads while polling for
/// exit, killing it if it exceeds `limit`. Prevents a hung CLI from
/// wedging the AI worker.
fn collect_with(
    mut child: std::process::Child,
    limit: Duration,
) -> Result<CollectResult, String> {
    let out_handle = child.stdout.take().map(spawn_reader);
    let err_handle = child.stderr.take().map(spawn_reader);

    let start = Instant::now();
    let mut timed_out = false;
    let mut status = None;
    loop {
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(s) => {
                status = Some(s);
                break;
            }
            None => {
                if start.elapsed() > limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    let stdout = out_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = err_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    Ok((stdout, stderr, status, timed_out))
}

/// Spawn a thread draining a pipe fully.
fn spawn_reader(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// One parsed AI turn: a context query or the final answer.
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    /// `{"action":"db","query":"...","limit":10}` — search saved commands.
    Db { query: String, limit: usize },
    /// `{"action":"which","tools":[...]}` — check PATH for tools.
    Which(Vec<String>),
    /// `{"action":"help","tool":"..."}` — read usage (`/?` / `--help`);
    /// the tool itself is never executed.
    Help { tool: String },
    /// A bare `exec`/`run` of a real command: refused, never executed.
    Denied,
    /// `{"action":"answer","command":"..."}` or an ```answer fence.
    Answer(String),
    /// No JSON probe: fall back to legacy command extraction.
    Legacy,
}

/// Which command language the answer must be written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFamily {
    /// Windows Command Prompt (`cmd.exe`).
    Cmd,
    /// Windows PowerShell (`powershell` / `pwsh`).
    Powershell,
    /// POSIX shell (`sh` / `bash` / ...).
    Posix,
}

/// The shell panes actually run: the configured one, or the platform
/// default (`cmd.exe` on Windows, `$SHELL`/`sh` elsewhere). This is what
/// the AI answer must be written for — never "the default shell".
pub fn effective_shell(configured: &str) -> String {
    let c = configured.trim();
    if !c.is_empty() {
        return c.to_string();
    }
    if cfg!(windows) {
        "cmd.exe".to_string()
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
    }
}

/// Classify a shell name into its command language.
pub fn shell_family(shell: &str) -> ShellFamily {
    let l = shell.to_ascii_lowercase();
    if l.contains("powershell") || l.contains("pwsh") {
        ShellFamily::Powershell
    } else if l.contains("cmd") || l.contains("command prompt") || (l.trim().is_empty() && cfg!(windows))
    {
        ShellFamily::Cmd
    } else {
        ShellFamily::Posix
    }
}

/// One-line directive naming the target shell and outlawing the classic
/// mix-up (PowerShell syntax typed into `cmd.exe` and vice versa).
pub fn shell_directive(shell: &str) -> String {
    match shell_family(shell) {
        ShellFamily::Cmd => format!(
            "Target shell: {shell} (Windows Command Prompt). The final command MUST be valid cmd.exe syntax (builtins like dir/copy/del, chaining with & and &&). NEVER answer with PowerShell syntax: no `powershell -Command`, no Get-ChildItem/Get-Content/Select-String, no ${{...}}."
        ),
        ShellFamily::Powershell => format!(
            "Target shell: {shell} (Windows PowerShell). The final command MUST be valid PowerShell syntax (cmdlets like Get-ChildItem are fine). NEVER answer with cmd.exe syntax."
        ),
        ShellFamily::Posix => format!(
            "Target shell: {shell} (POSIX shell). The final command MUST be valid sh syntax. NEVER answer with PowerShell or cmd.exe syntax."
        ),
    }
}

/// Classify one raw AI reply. JSON inside a fence is accepted; the `answer`
/// fence wins over `answer` JSON; anything without a probe is [`Probe::Legacy`].
fn parse_probe(raw: &str) -> Probe {
    if let Some(cmd) = answer_fence(raw) {
        return Probe::Answer(cmd);
    }
    let Some(obj) = extract_json_object(raw) else {
        return Probe::Legacy;
    };
    // `"action"` wins; bare keys (`{"which":...}`) are accepted too.
    let action = obj
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match action.as_str() {
        "db" | "database" | "commands" | "saved" | "search" | "query" | "find" | "lookup"
        | "history" => Probe::Db {
            query: db_query_from(&obj),
            limit: json_limit(&obj, DEFAULT_DB_LIMIT),
        },
        "which" | "where" | "tools" => Probe::Which(string_list(obj.get("tools").or_else(|| obj.get("names")))),
        "help" | "usage" | "man" | "manual" => Probe::Help {
            tool: help_tool_from(&obj),
        },
        // Legacy `exec`/`run`: only a help-style invocation is honoured
        // (as a `help` query); running bare commands is denied.
        "exec" | "run" | "probe" | "shell" => match exec_as_help(&obj) {
            Some(tool) => Probe::Help { tool },
            None => Probe::Denied,
        },
        "answer" | "command" | "final" | "done" => {
            let cmd = obj
                .get("command")
                .or_else(|| obj.get("cmd"))
                .or_else(|| obj.get("answer"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Probe::Answer(clean(&cmd))
        }
        _ => {
            // Shorthand without "action".
            if obj.get("which").is_some() || obj.get("tools").is_some() {
                let tools = string_list(obj.get("which").or_else(|| obj.get("tools")));
                Probe::Which(tools)
            } else if obj.get("db").is_some()
                || obj.get("database").is_some()
                || obj.get("search").is_some()
                || obj.get("query").is_some()
            {
                Probe::Db {
                    query: db_query_from(&obj),
                    limit: json_limit(&obj, DEFAULT_DB_LIMIT),
                }
            } else if obj.get("help").is_some()
                || obj.get("usage").is_some()
                || obj.get("man").is_some()
                || obj.get("manual").is_some()
            {
                Probe::Help {
                    tool: help_tool_from(&obj),
                }
            } else if let Some(cmd) = obj
                .get("exec")
                .or_else(|| obj.get("run"))
                .or_else(|| obj.get("probe"))
                .and_then(|v| v.as_str())
            {
                // Bare shorthand without "action": help-style only.
                match as_help_tool(cmd) {
                    Some(tool) => Probe::Help { tool },
                    None => Probe::Denied,
                }
            } else if obj.get("answer").is_some() || obj.get("command").is_some() {
                let cmd = obj
                    .get("answer")
                    .or_else(|| obj.get("command"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Probe::Answer(clean(cmd))
            } else {
                Probe::Legacy
            }
        }
    }
}

/// Tool name for a `help` query: prefers `tool`, else the `command`/`help`
/// value's first token, else the shorthand key itself (`{"help":"diskpart"}`).
fn help_tool_from(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    if let Some(t) = obj.get("tool").and_then(|v| v.as_str()) {
        let t = first_token(t.trim());
        if !t.is_empty() {
            return t;
        }
    }
    for key in ["command", "cmd", "help", "usage", "man", "manual", "for", "about"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            let t = first_token(s.trim());
            if !t.is_empty() {
                return t;
            }
        }
    }
    String::new()
}

/// Legacy `exec`/`run` objects: honour only help-style invocations by
/// extracting their tool (`{"action":"exec","command":"fd --help"}` →
/// `Some("fd")`); anything that would RUN something returns `None`
/// (→ denied, never executed).
fn exec_as_help(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if let Some(cmd) = obj
        .get("command")
        .or_else(|| obj.get("cmd"))
        .and_then(|v| v.as_str())
    {
        return as_help_tool(cmd);
    }
    let tool = obj.get("tool").and_then(|v| v.as_str()).unwrap_or("");
    if tool.trim().is_empty() {
        return None;
    }
    // `tool` + help-ish `args` counts; anything else is denied.
    let args = string_list(obj.get("args")).join(" ").to_ascii_lowercase();
    if args.is_empty() || is_help_flag(&args) || args.contains("help") || args.contains("man") {
        let tool = first_token(tool.trim());
        if tool.is_empty() {
            None
        } else {
            Some(tool)
        }
    } else {
        None
    }
}

/// First whitespace-separated token (quote-aware) of a command fragment.
fn first_token(s: &str) -> String {
    split_quoted(s).into_iter().next().unwrap_or_default()
}

/// `Some(tool)` when `cmd` is a help invocation (`help X`, `man X`,
/// `X --help`, `X /?`, ...); `None` for anything that would run something.
fn as_help_tool(cmd: &str) -> Option<String> {
    let t = cmd.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    // `help <tool>` / `man <tool>` form.
    for prefix in ["help ", "man "] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let tool = first_token(rest.trim());
            if !tool.is_empty() {
                return Some(tool);
            }
        }
    }
    // `<tool> <help-flag>` form: first token is the tool when the tail
    // carries a help flag. A bare tool name alone is NOT help (denied).
    let tool = first_token(t);
    if tool.is_empty() {
        return None;
    }
    let tail = lower
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");
    if tail.is_empty() {
        return None;
    }
    if is_help_flag(&tail) || tail.contains("help") {
        Some(tool)
    } else {
        None
    }
}

/// True when `args` is just a help flag (`--help`, `-h`, `-?`, `/?`, ...).
fn is_help_flag(args: &str) -> bool {
    let a = args.trim().to_ascii_lowercase();
    matches!(
        a.as_str(),
        "--help" | "-h" | "-?" | "/?" | "-help" | "/help" | "help" | "--usage" | "-u"
    ) || a.split_whitespace().any(|w| matches!(
        w,
        "--help" | "-h" | "-?" | "/?" | "-help" | "/help" | "help" | "--usage"
    ))
}

/// Default rows returned for a `db` query without an explicit `limit`.
const DEFAULT_DB_LIMIT: usize = 10;

/// Hard cap for a `db` `limit` (the model may ask for e.g. `"limit": 100`).
const MAX_DB_LIMIT: usize = 100;

/// Extract the search text for a `db` query: prefers `query`/`q`, else the
/// shorthand key itself (`{"db":"git"}`), else empty (lists most-used).
fn db_query_from(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    for key in ["query", "q", "keywords", "search", "text", "for", "db", "database"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                // `search`/`db` keys only count when they hold the query text;
                // `query`-family keys win because they are checked first.
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Numeric `limit` (also `n`/`count`/`max`) from a query object, clamped to
/// `1..=MAX_DB_LIMIT`, or `default` when absent/unparseable.
fn json_limit(obj: &serde_json::Map<String, serde_json::Value>, default: usize) -> usize {
    obj.get("limit")
        .or_else(|| obj.get("n"))
        .or_else(|| obj.get("count"))
        .or_else(|| obj.get("max"))
        .and_then(|v| {
            v.as_u64()
                .map(|n| n as usize)
                .or_else(|| v.as_str()?.trim().parse::<usize>().ok())
        })
        .map(|n| n.clamp(1, MAX_DB_LIMIT))
        .unwrap_or(default)
}

/// Search a saved-command snapshot: every whitespace-separated word of
/// `query` must appear (case-insensitive) in command, comment or tags.
/// Empty query lists everything. Most-used first, then newest (`id` DESC).
pub fn search_saved<'a>(
    saved: &'a [crate::commands_db::SavedCommand],
    query: &str,
    limit: usize,
) -> Vec<&'a crate::commands_db::SavedCommand> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect();
    let mut hits: Vec<&crate::commands_db::SavedCommand> = saved
        .iter()
        .filter(|c| {
            if words.is_empty() {
                return true;
            }
            let hay = format!("{} {} {}", c.command, c.comment, c.tags).to_ascii_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .collect();
    hits.sort_by(|a, b| b.uses.cmp(&a.uses).then(b.id.cmp(&a.id)));
    hits.truncate(limit.clamp(1, MAX_DB_LIMIT));
    hits
}

/// Total matches for a query (before `limit`), for the RESULT header.
fn count_saved_matches(saved: &[crate::commands_db::SavedCommand], query: &str) -> usize {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect();
    if words.is_empty() {
        return saved.len();
    }
    saved
        .iter()
        .filter(|c| {
            let hay = format!("{} {} {}", c.command, c.comment, c.tags).to_ascii_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .count()
}

/// Compact one-line-per-command rendering for the model. Empty when no hits.
fn format_saved(hits: &[&crate::commands_db::SavedCommand]) -> String {
    if hits.is_empty() {
        return "(no saved commands match)".to_string();
    }
    hits.iter()
        .map(|c| {
            let comment = c.comment.trim();
            if comment.is_empty() {
                format!("#{} [uses {}] {}", c.id, c.uses, c.command)
            } else {
                format!("#{} [uses {}] {} -- {}", c.id, c.uses, c.command, comment)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// String array from a JSON value (a single string becomes one item).
fn string_list(v: Option<&serde_json::Value>) -> Vec<String> {
    match v {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .take(20)
            .collect(),
        Some(serde_json::Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s.to_string()]
            }
        }
        _ => Vec::new(),
    }
}

/// First JSON object in `raw` (inside a fence or bare). Scans from each `{`
/// to each later `}` (longest first) and returns the first value that parses
/// as an object, so nested braces in strings still work.
///
/// Strict double-quoted JSON is tried first; as a fallback, single-quoted
/// JSON is accepted too. The fallback exists because the `{prompt}`
/// substitution rewrites `"` to `'` (shell quoting), so the model sees
/// `{'action':'which',...}` examples and mimics them back.
fn extract_json_object(raw: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let bytes = raw.as_bytes();
    let opens: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'{')
        .map(|(i, _)| i)
        .collect();
    // Pass 1: strict JSON.
    for &start in &opens {
        for end in (0..bytes.len()).rev().filter(|&e| bytes[e] == b'}' && e > start) {
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(&raw[start..=end])
            {
                return Some(map);
            }
        }
    }
    // Pass 2: single-quoted JSON (`'` -> `"` outside `"..."` strings).
    for &start in &opens {
        for end in (0..bytes.len()).rev().filter(|&e| bytes[e] == b'}' && e > start) {
            let converted = json_single_to_double(&raw[start..=end]);
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(&converted)
            {
                return Some(map);
            }
        }
    }
    None
}

/// Convert single-quoted JSON to double-quoted: every `'` outside an
/// existing `"..."` string becomes `"`. `'` and `"` are both one byte, so
/// byte indices into the original stay valid.
fn json_single_to_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_double = false;
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if c == '"' && prev != Some('\\') {
            in_double = !in_double;
            out.push(c);
        } else if c == '\'' && !in_double {
            out.push('"');
        } else {
            out.push(c);
        }
        prev = Some(c);
    }
    out
}

/// Contents of a fenced block whose opening tag names `answer`
/// (```` ```answer ````, case-insensitive), cleaned to one line.
fn answer_fence(s: &str) -> Option<String> {
    let mut inside = false;
    let mut buf = String::new();
    for line in s.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if inside {
                let cmd = clean(&buf);
                return if cmd.is_empty() { None } else { Some(cmd) };
            }
            if rest.trim().to_ascii_lowercase().starts_with("answer") {
                inside = true;
            }
            continue;
        }
        if inside {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    None
}

/// `which`-style report for `tools`: one `name: FOUND at <path>` /
/// `name: MISSING` line each (PATH lookup, no shell needed).
fn which_report(tools: &[String]) -> String {
    tools
        .iter()
        .take(20)
        .map(|t| {
            let name = t.trim();
            match which_tool(name) {
                Some(path) => format!("{}: FOUND at {}", name, path.display()),
                None => format!("{name}: MISSING"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// PATH lookup for one tool name (`which`/`where` semantics). Only the first
/// whitespace-separated token is looked up; directories from `PATH` are
/// searched (with `PATHEXT` on Windows); an explicit path is checked
/// directly. Returns the full path when executable-found.
pub fn which_tool(name: &str) -> Option<std::path::PathBuf> {
    let first = split_quoted(name).into_iter().next().unwrap_or_default();
    let first = first.trim();
    if first.is_empty() {
        return None;
    }
    let candidate = std::path::Path::new(first);
    if candidate.components().count() > 1 {
        // Explicit (relative or absolute) path: accept existing files.
        if candidate.is_file() {
            return Some(candidate.to_path_buf());
        }
        #[cfg(windows)]
        for ext in pathext() {
            let p = candidate.with_extension(ext.trim_start_matches('.'));
            // `with_extension` replaces; instead append when missing.
            let appended = std::path::PathBuf::from(format!("{first}{ext}"));
            if appended.is_file() {
                return Some(appended);
            }
            let _ = p;
        }
        return None;
    }
    let paths = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&paths) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let direct = dir.join(first);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(windows)]
        for ext in pathext() {
            let p = dir.join(format!("{first}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
        #[cfg(not(windows))]
        let _ = ();
    }
    None
}

#[cfg(windows)]
fn pathext() -> Vec<String> {
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
        .split(';')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Read a tool's usage WITHOUT executing it: `<tool> /?` on Windows (the
/// `man` equivalent — safe even for interactive programs like `diskpart`),
/// `<tool> --help` then `man <tool>` elsewhere. The tool name is validated
/// and resolved first; anything else is refused. Never panics; timeouts and
/// failures are reported as text so the loop can continue.
fn help_probe(tool: &str) -> String {
    let tool = first_token(tool.trim());
    if !valid_tool_name(&tool) {
        return format!(
            "REFUSED: {tool:?} is not a plain tool name (single token, letters/digits/._-+ and path separators only)"
        );
    }
    if which_tool(&tool).is_none() {
        return format!("{tool}: MISSING (not on PATH, no help to read)");
    }
    #[cfg(windows)]
    let flags: &[&str] = &["/?", "--help"];
    #[cfg(not(windows))]
    let flags: &[&str] = &["--help", "man"];
    let mut attempts = Vec::new();
    for flag in flags {
        // `man` is `man <tool>`; every other flag is `<tool> <flag>`.
        let line = if *flag == "man" {
            format!("man {tool}")
        } else {
            format!("{tool} {flag}")
        };
        let out = capture_help(&line);
        attempts.push(format!("[{line}]\n{out}"));
        // First useful output wins: exit 0, or real help text on stdout
        // with silent stderr (`dir /?` prints help yet exits 1).
        if out.contains("EXIT 0") && help_body_len(&out) > 40 {
            break;
        }
        if help_body_len(&out) > 200 && !out.contains("STDERR:") {
            break;
        }
    }
    truncate(&attempts.join("\n---\n"), PROBE_CHARS)
}

/// Length of the STDOUT body inside a [`capture_help`] report.
fn help_body_len(out: &str) -> usize {
    out.split_once("STDOUT:\n")
        .map(|(_, body)| body.chars().count())
        .unwrap_or(0)
}

/// A tool name is safe for a help probe: one token, no shell metacharacters.
fn valid_tool_name(tool: &str) -> bool {
    !tool.is_empty()
        && tool.len() <= 64
        && tool.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/' | '\\' | ':')
        })
}

/// Run one help command (built by [`help_probe`], never model-verbatim) and
/// return truncated `EXIT + STDOUT + STDERR` text. Stdin is closed, so an
/// interactive tool that ignores the help flag gets EOF instead of a hang.
fn capture_help(line: &str) -> String {
    let line = fix_program_path(line);
    let mut cmd = shell(&line);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("SPAWN FAILED [{line}]: {e}"),
    };
    let (stdout, stderr, status, timed_out) = match collect_with(child, PROBE_TIMEOUT) {
        Ok(r) => r,
        Err(e) => return format!("PROBE ERROR [{line}]: {e}"),
    };
    if timed_out {
        return format!(
            "TIMEOUT after {}s [{line}]: showing partial output (if any)",
            PROBE_TIMEOUT.as_secs()
        );
    }
    let code = status
        .map(|s| {
            s.code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        })
        .unwrap_or_else(|| "unknown".into());
    let mut out = format!("EXIT {code} [{line}]\nSTDOUT:\n{}", truncate(&String::from_utf8_lossy(&stdout), PROBE_CHARS / 2));
    let err = truncate(&String::from_utf8_lossy(&stderr), PROBE_CHARS / 2);
    if !err.trim().is_empty() {
        out.push_str(&format!("\nSTDERR:\n{err}"));
    }
    truncate(&out, PROBE_CHARS)
}

/// Truncate to `max` chars (with marker), keeping output model-sized.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\n... [truncated]")
}

/// Collapse all whitespace (including newlines) into single spaces, so a
/// value can be embedded safely in a single shell argument.
fn flatten(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Substitute `{prompt}` / `{question}` in the raw line. Values are flattened
/// to one line and, when they contain spaces and the placeholder is not
/// already inside quotes, the value is quoted so it stays one argument.
/// Returns the new line and whether any placeholder was present.
fn apply_placeholders(line: &str, prompt: &str, question: &str) -> (String, bool) {
    let (line, a) = substitute_placeholder(line, "{prompt}", prompt);
    let (line, b) = substitute_placeholder(&line, "{question}", question);
    (line, a || b)
}

/// Replace every `placeholder` in `line`, quoting the value when needed.
/// Shared with the saved-command placeholder filling.
pub(crate) fn substitute_placeholder(line: &str, placeholder: &str, value: &str) -> (String, bool) {
    if !line.contains(placeholder) {
        return (line.to_string(), false);
    }
    // Quotes inside the value would break the surrounding quoting.
    let value = flatten(value).replace('"', "'");
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find(placeholder) {
        out.push_str(&rest[..i]);
        let before = out.chars().last();
        let after = rest[i + placeholder.len()..].chars().next();
        let already_quoted = matches!(
            (before, after),
            (Some('"'), Some('"')) | (Some('\''), Some('\''))
        );
        if already_quoted || !value.contains(' ') {
            out.push_str(&value);
        } else {
            out.push('"');
            out.push_str(&value);
            out.push('"');
        }
        rest = &rest[i + placeholder.len()..];
    }
    out.push_str(rest);
    (out, true)
}

/// Quote an **unquoted** program path that contains spaces, so the shell does
/// not split it (`C:\Users\PC MASTER RACE\tool.exe …`). Lines that already
/// start with a quote, or whose first token is an existing file, are returned
/// unchanged so shell syntax and quoting are preserved verbatim.
fn fix_program_path(line: &str) -> String {
    let trimmed = line.trim_start();
    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        return line.to_string();
    }
    let tokens = split_quoted(line);
    if tokens.len() < 2 || std::path::Path::new(&tokens[0]).is_file() {
        return line.to_string();
    }
    // Find the shortest leading run of tokens that names an existing file.
    for k in 2..=tokens.len() {
        let joined = tokens[..k].join(" ");
        if std::path::Path::new(&joined).is_file() {
            let mut out = format!("\"{joined}\"");
            if k < tokens.len() {
                out.push(' ');
                out.push_str(&tokens[k..].join(" "));
            }
            log::debug!("ai: quoted program path {joined:?}");
            return out;
        }
    }
    line.to_string()
}

/// Rebuild a command line, quoting any token that contains whitespace.
/// Spawn `command` through the platform shell (needed for `.cmd` wrappers).
///
/// On Windows the line is passed with `raw_arg`: letting Rust quote the whole
/// argument makes `cmd` misparse inner quotes, splitting `-p "a b c"` into
/// separate arguments (the "invalid argument: are" failure). The child also
/// gets its own process group and no console, so its Ctrl+C can't kill termrs.
fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let mut c = Command::new("cmd");
        c.raw_arg("/C");
        c.raw_arg(command);
        c.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

/// Split a command line into argv, honouring single/double quotes. When the
/// first token is not an existing file, progressively rejoin tokens with
/// spaces to recover a program path that itself contains spaces.
/// Quote-aware whitespace split (no shell expansion).
fn split_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match c {
            '"' | '\'' => {
                if quote == Some(c) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(c);
                } else {
                    cur.push(c);
                }
                has = true;
            }
            c if c.is_whitespace() && quote.is_none() => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// Pull a shell command out of an AI answer: an ```answer fence first, then
/// the first fenced block, else the last non-empty line; `$ `/`> ` prompts
/// are stripped. Always a single line (newlines become spaces) so typing it
/// cannot fire stray Enters.
pub fn extract_command(answer: &str) -> String {
    if let Some(cmd) = answer_fence(answer) {
        return cmd;
    }
    // `{"action":"answer","command":"..."}` is also a final answer.
    if let Probe::Answer(cmd) = parse_probe(answer)
        && !cmd.is_empty() {
            return cmd;
        }
    let raw = match first_fence(answer) {
        Some(block) => block,
        None => answer
            .lines()
            .rev()
            .map(str::to_string)
            .find(|l| {
                let c = clean(l);
                // Skip prompt echoes (`> …`) and the chat CLI's chatter.
                !c.is_empty()
                    && !l.trim_start().starts_with('>')
                    && !c.eq_ignore_ascii_case("Exiting...")
            })
            .unwrap_or_default(),
    };
    clean(&raw)
}

/// Contents of the first fenced code block, if any.
fn first_fence(s: &str) -> Option<String> {
    let mut inside = false;
    let mut buf = String::new();
    for line in s.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if inside {
                return Some(buf);
            }
            inside = true;
            continue;
        }
        if inside {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    None
}

/// Trim whitespace and a leading shell prompt marker / list bullet, then
/// collapse all whitespace (including newlines) so the result is one line.
fn clean(line: &str) -> String {
    let t = line.trim();
    let t = t
        .strip_prefix("$ ")
        .or_else(|| t.strip_prefix("> "))
        .or_else(|| t.strip_prefix("PS> "))
        .or_else(|| t.strip_prefix("- "))
        .or_else(|| t.strip_prefix("* "))
        .unwrap_or(t);
    let t = t.trim().trim_matches('`').trim();
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_fenced_block() {
        let a = "Sure!\n```sh\nls -la\n```\nThat lists files.";
        assert_eq!(extract_command(a), "ls -la");
    }

    #[test]
    fn fenced_block_becomes_one_line() {
        let a = "```sh\necho a\necho b\n```";
        assert_eq!(extract_command(a), "echo a echo b");
    }

    /// A real llama-cli chat answer: banner + prose + fenced command.
    #[test]
    fn extracts_from_chat_output() {
        let a = "> a command to show hidden folders\n\
                 To show hidden folders use:\n\n\
                 ```bash\nls -la\n```\n\n\
                 This lists everything.\nExiting...";
        assert_eq!(extract_command(a), "ls -la");
    }

    /// Without a fence, the prompt echo and chatter are skipped.
    #[test]
    fn skips_prompt_echo_and_chatter() {
        let a = "> show hidden files\n    ls -la\nExiting...";
        assert_eq!(extract_command(a), "ls -la");
    }

    #[test]
    fn falls_back_to_last_line() {
        assert_eq!(extract_command("Here you go:\n$ df -h"), "df -h");
        assert_eq!(extract_command("git status\n"), "git status");
    }

    #[test]
    fn strips_bullets_and_backticks() {
        assert_eq!(extract_command("- `echo hi`"), "echo hi");
    }

    #[test]
    fn empty_when_nothing() {
        assert_eq!(extract_command("   \n\n"), "");
    }

    /// End-to-end: the runner spawns through the shell, reads, and extracts.
    #[test]
    fn runs_command_through_shell() {
        let out = run("echo termrs_ai_probe", "", "ignored").unwrap();
        assert_eq!(out, "termrs_ai_probe");
    }

    /// A quoted argument with spaces must reach the program as ONE argument
    /// (regression for "invalid argument: are" when `-p "You are …"` split).
    /// cmd's `for` exposes argument boundaries: one item -> `["a b"]`,
    /// a split would yield `["a]` / `[b"]`.
    #[test]
    fn quoted_argument_with_spaces_stays_one() {
        let out = run(r#"for %i in (c "a b") do @echo [%i]"#, "", "ignored").unwrap();
        assert_eq!(out, r#"["a b"]"#, "argument was split");
    }

    /// End-to-end regression: an unquoted `{question}` with spaces must reach
    /// the program as one argument (the "invalid argument: are" case).
    #[test]
    fn placeholder_value_stays_one_argument() {
        let out = run(r#"for %i in (-p {question}) do @echo [%i]"#, "", "a b").unwrap();
        assert_eq!(out, r#"["a b"]"#, "placeholder value was split");
    }

    /// A failing command reports its stderr and the exact line.
    #[test]
    fn reports_failure_with_command() {
        let err = run("this_command_does_not_exist_xyz", "", "q").unwrap_err();
        assert!(err.contains("this_command_does_not_exist_xyz"), "err: {err}");
    }

    /// Realistic llama.cpp one-shot line: an unquoted placeholder that
    /// contains spaces is quoted; an already-quoted one is left alone.
    #[test]
    fn substitutes_prompt_and_question() {
        let line = r#"llama cli -m "C:\m.gguf" -p {question} -st"#;
        let (out, had) = apply_placeholders(line, "SYS\n\nQ", "list files");
        assert!(had);
        assert_eq!(
            out,
            r#"llama cli -m "C:\m.gguf" -p "list files" -st"#
        );

        // Already quoted in the config: substituted without extra quotes.
        let quoted = r#"llama cli -m "C:\m.gguf" -p "{question}" -st"#;
        let (out, _) = apply_placeholders(quoted, "SYS\n\nQ", "list files");
        assert_eq!(out, r#"llama cli -m "C:\m.gguf" -p "list files" -st"#);

        // Single-word value needs no quotes.
        let (out, _) = apply_placeholders(line, "SYS", "hidden");
        assert_eq!(out, r#"llama cli -m "C:\m.gguf" -p hidden -st"#);

        // No placeholder -> line returned verbatim.
        let plain = r#"llama cli -m "C:\m.gguf" -st"#;
        let (same, had) = apply_placeholders(plain, "x", "y");
        assert!(!had);
        assert_eq!(same, plain);
    }

    /// Placeholder values are flattened so they cannot break the command line.
    #[test]
    fn placeholder_is_single_line() {
        let multi = "You are a shell assistant.\n\nlist hidden files\n";
        assert_eq!(
            flatten(multi),
            "You are a shell assistant. list hidden files"
        );
    }

    /// A two-word program (`llama cli`) is left alone.
    #[test]
    fn keeps_two_word_program() {
        let line =
            r#"llama cli -m "C:\Users\me\AppData\Local\llmfit\models\x.gguf" -p hi -st"#;
        assert_eq!(fix_program_path(line), line);
    }

    #[test]
    fn splits_quoted_arguments() {
        assert_eq!(split_quoted("llm -m gpt"), vec!["llm", "-m", "gpt"]);
        assert_eq!(
            split_quoted(r#""C:\a b\tool.exe" --flag"#),
            vec![r#"C:\a b\tool.exe"#, "--flag"]
        );
        assert_eq!(split_quoted("  spaced   out  "), vec!["spaced", "out"]);
    }

    /// An unquoted program path containing spaces is quoted when the file
    /// exists (Windows home paths like `PC MASTER RACE`).
    #[test]
    fn quotes_unquoted_program_path_with_spaces() {
        let dir = std::env::temp_dir().join(format!("termrs ai {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("tool.exe");
        std::fs::write(&exe, b"x").unwrap();
        let line = format!("{} run llama3", exe.display());
        let fixed = fix_program_path(&line);
        assert_eq!(fixed, format!("\"{}\" run llama3", exe.display()));
        // Already-quoted lines are returned verbatim.
        let quoted = format!("\"{}\" run", exe.display());
        assert_eq!(fix_program_path(&quoted), quoted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Shell syntax is preserved verbatim (regression: split+requote mangled
    /// `(c "a b")` into `(c "a b)"`).
    #[test]
    fn preserves_shell_syntax() {
        let line = r#"for %i in (c "a b") do @echo [%i]"#;
        assert_eq!(fix_program_path(line), line);
    }

    /// The `answer` fence is the final command, even with prose around it.
    #[test]
    fn answer_fence_is_final() {
        let a = "Let me check.\n```answer\ngit status\n```\nRun it.";
        assert_eq!(answer_fence(a), Some("git status".into()));
        assert_eq!(extract_command(a), "git status");
        assert!(matches!(
            parse_probe(a),
            Probe::Answer(ref c) if c == "git status"
        ));
    }

    /// `which` / `help` / `answer` JSON probes classify correctly, including
    /// inside fences; bare `exec` of a real command is denied, help-style
    /// `exec` is honoured as `help`.
    #[test]
    fn parses_json_probes() {
        assert!(matches!(
            parse_probe(r#"{"action":"which","tools":["fd","nope-xyz"]}"#),
            Probe::Which(ref t) if t == &vec!["fd".to_string(), "nope-xyz".to_string()]
        ));
        assert!(matches!(
            parse_probe("```json\n{\"action\":\"help\",\"tool\":\"diskpart\"}\n```"),
            Probe::Help { ref tool } if tool == "diskpart"
        ));
        assert!(matches!(
            parse_probe(r#"{"action":"exec","command":"fd --help"}"#),
            Probe::Help { ref tool } if tool == "fd"
        ));
        assert!(matches!(
            parse_probe(r#"{"action":"exec","command":"dir /b /s"}"#),
            Probe::Denied
        ));
        assert!(matches!(
            parse_probe(r#"{"action":"run","command":"diskpart"}"#),
            Probe::Denied
        ));
        assert!(matches!(
            parse_probe(r#"{"action":"answer","command":"git status"}"#),
            Probe::Answer(ref c) if c == "git status"
        ));
        // Bare keys without "action" also work.
        assert!(matches!(
            parse_probe(r#"{"which":["rg"]}"#),
            Probe::Which(_)
        ));
        assert!(matches!(
            parse_probe(r#"{"help":"diskpart"}"#),
            Probe::Help { ref tool } if tool == "diskpart"
        ));
        // Plain prose stays legacy.
        assert_eq!(parse_probe("just run ls -la"), Probe::Legacy);
    }

    /// PATH lookup finds the running shell and misses nonsense names.
    #[test]
    fn which_finds_shell_and_misses_unknown() {
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(
            which_tool(shell).is_some(),
            "expected {shell} on PATH"
        );
        assert_eq!(which_tool("definitely-not-a-tool-xyz"), None);
        assert_eq!(which_tool(""), None);
        let report = which_report(&[shell.into(), "definitely-not-a-tool-xyz".into()]);
        assert!(report.contains("FOUND"), "report: {report}");
        assert!(report.contains("MISSING"), "report: {report}");
    }

    /// Help probes read usage text without executing the tool; bad names
    /// and missing tools are refused reported as text, never run.
    #[test]
    fn help_probe_reads_usage_without_executing() {
        // `cmd /?` documents the shell itself: must contain EXIT + help text.
        let out = help_probe("cmd");
        assert!(out.contains("EXIT"), "out: {out}");
        // Missing tools are reported, not run.
        let missing = help_probe("definitely-not-a-tool-xyz");
        assert!(missing.contains("MISSING"), "out: {missing}");
        // Shell metacharacters never reach the shell: only the first token
        // is ever used, so `& del ...` is dropped (help for `dir` instead).
        let chopped = help_probe("dir & del C:\\");
        assert!(chopped.contains("[dir /?]"), "out: {chopped}");
        // A metacharacter INSIDE the single token is refused outright.
        let evil = help_probe("di&r");
        assert!(evil.contains("REFUSED"), "out: {evil}");
        let empty = help_probe("   ");
        assert!(empty.contains("REFUSED"), "out: {empty}");
        assert!(truncate("abcdef", 3).contains("truncated"));
        assert_eq!(truncate("ab", 10), "ab");
    }

    /// Target-shell detection: empty config on Windows means cmd.exe, and
    /// the directive outlaws the PowerShell/cmd mix-up both ways.
    #[test]
    fn shell_identity_names_cmd_and_bans_mixups() {
        assert_eq!(effective_shell(""), if cfg!(windows) { "cmd.exe".to_string() } else { std::env::var("SHELL").unwrap_or_else(|_| "sh".into()) });
        assert_eq!(effective_shell("pwsh.exe"), "pwsh.exe");
        assert_eq!(shell_family("cmd.exe"), ShellFamily::Cmd);
        assert_eq!(shell_family("C:\\Windows\\System32\\cmd.exe"), ShellFamily::Cmd);
        assert_eq!(shell_family("powershell.exe"), ShellFamily::Powershell);
        assert_eq!(shell_family("pwsh"), ShellFamily::Powershell);
        assert_eq!(shell_family("/bin/bash"), ShellFamily::Posix);
        let cmd = shell_directive("cmd.exe");
        assert!(cmd.contains("cmd.exe") && cmd.contains("PowerShell"), "cmd: {cmd}");
        assert!(cmd.contains("NEVER"), "cmd: {cmd}");
        let ps = shell_directive("powershell");
        assert!(ps.contains("PowerShell") && ps.contains("NEVER"), "ps: {ps}");
    }

    /// Exact query repeats are detected so the loop nudges instead of
    /// re-running (small models echo the example query every turn).
    #[test]
    fn repeat_queries_are_detected() {
        let mut seen = Vec::new();
        assert!(!already_asked(&mut seen, "db:keywords:10".into()));
        assert!(already_asked(&mut seen, "db:keywords:10".into()));
        assert!(!already_asked(&mut seen, "db:other:10".into()));
        assert!(REPEAT_NUDGE.contains("already asked"));
    }

    /// End-to-end: a bare `exec` is denied (never run) and the loop continues
    /// to the final answer. The fake answers differently depending on whether
    /// turn 1 came back REFUSED (denied) or EXIT (executed).
    #[test]
    fn denied_exec_never_runs() {
        #[cfg(windows)]
        let fake = "findstr \"REFUSED\" >nul && (echo ```answer & echo final-after-deny & echo ```) || (findstr \"RESULT\" >nul && (echo ```answer & echo should-not-happen & echo ```) || (echo {\"action\":\"exec\",\"command\":\"hostname\"}))";
        #[cfg(not(windows))]
        let fake = "if grep -q 'REFUSED'; then { echo '```answer'; echo 'final-after-deny'; echo '```'; } elif grep -q 'RESULT'; then { echo '```answer'; echo 'should-not-happen'; echo '```'; } else echo '{\"action\":\"exec\",\"command\":\"hostname\"}'; fi";
        let out = run(fake, "Goal: make a command.", "q").unwrap();
        assert_eq!(out, "final-after-deny");
    }

    /// End-to-end agentic loop: a fake AI CLI that first probes `which`,
    /// then answers. The probe result is fed back and the final command wins.
    ///
    /// The fake reads the conversation on stdin: turn 1 has no TOOL RESULT,
    /// so it prints a probe; turn 2 sees the fed-back result and answers.
    #[test]
    fn agentic_loop_feeds_probe_back_to_answer() {
        #[cfg(windows)]
        let fake = "findstr \"TOOL RESULT\" >nul && (echo ```answer & echo echo hello-agentic & echo ```) || (echo {\"action\":\"which\",\"tools\":[\"cmd\"]})";
        #[cfg(not(windows))]
        let fake = "grep -q 'TOOL RESULT' && { echo '```answer'; echo 'echo hello-agentic'; echo '```'; } || echo '{\"action\":\"which\",\"tools\":[\"sh\"]}'";
        let out = run(fake, "sys", "do the thing").unwrap();
        assert_eq!(out, "echo hello-agentic");
    }

    /// The `{prompt}` substitution rewrites `"` to `'` (shell quoting), so
    /// the model sees single-quoted examples and mimics them back. Those
    /// must still parse as probes.
    #[test]
    fn parses_single_quoted_probes() {
        assert!(matches!(
            parse_probe("{'action':'which','tools':['fd']}"),
            Probe::Which(ref t) if t == &vec!["fd".to_string()]
        ));
        assert!(matches!(
            parse_probe("{'action':'exec','command':'fd --help'}"),
            Probe::Help { ref tool } if tool == "fd"
        ));
        assert!(matches!(
            parse_probe("{'action':'exec','command':'dir /b /s'}"),
            Probe::Denied
        ));
        assert!(matches!(
            parse_probe("{'action':'answer','command':'git status'}"),
            Probe::Answer(ref c) if c == "git status"
        ));
    }

    /// A failing AI CLI reports its stdout (where small local models put the
    /// reason), not just stderr, plus the exit code.
    #[test]
    fn failure_error_includes_stdout_and_exit_code() {
        let err = run("echo boom-stdout-xyz && exit 1", "", "q").unwrap_err();
        assert!(err.contains("boom-stdout-xyz"), "err: {err}");
        assert!(err.contains("exit 1"), "err: {err}");
    }

    /// A lead-in that already teaches the query protocol is not doubled;
    /// a plain one gets the goal-first protocol appended. The first prompt
    /// states the goal and the DB size, never the commands themselves.
    #[test]
    fn system_prompt_skips_duplicate_protocol() {
        let aware = r#"Goal: make a command. Use {"action":"which"} and {"action":"db"} queries."#;
        let sys = build_system(aware, 3);
        assert!(
            !sys.contains("ONE JSON object"),
            "protocol duplicated: {sys}"
        );
        assert!(sys.contains("Environment:"), "sys: {sys}");
        assert!(sys.contains("3 saved command(s)"), "sys: {sys}");
        // Full command text is never in the first prompt.
        assert!(!sys.contains("wsl w3m"), "sys: {sys}");
        let sys2 = build_system("You are a shell assistant.", 0);
        assert!(sys2.contains("ONE JSON object"), "sys: {sys2}");
        assert!(!sys2.contains("saved command(s)"), "sys: {sys2}");
    }

    /// `db` queries classify correctly: action names, aliases, `limit`
    /// extraction (including `"limit": 100`), bare keys, single quotes.
    #[test]
    fn parses_db_probes() {
        assert!(matches!(
            parse_probe(r#"{"action":"db","query":"git log","limit":5}"#),
            Probe::Db { query, limit } if query == "git log" && limit == 5
        ));
        // Aliases for the action and the query/limit keys.
        assert!(matches!(
            parse_probe(r#"{"action":"search","q":"w3m","limit":100}"#),
            Probe::Db { query, limit } if query == "w3m" && limit == 100
        ));
        // Missing limit defaults; huge limits clamp to MAX_DB_LIMIT.
        assert!(matches!(
            parse_probe(r#"{"action":"db","query":"x"}"#),
            Probe::Db { limit, .. } if limit == DEFAULT_DB_LIMIT
        ));
        assert!(matches!(
            parse_probe(r#"{"action":"db","query":"x","limit":9999}"#),
            Probe::Db { limit, .. } if limit == MAX_DB_LIMIT
        ));
        // Bare keys without "action".
        assert!(matches!(
            parse_probe(r#"{"db":"git status"}"#),
            Probe::Db { query, .. } if query == "git status"
        ));
        // Single-quoted (the `{prompt}` substitution mangles quotes).
        assert!(matches!(
            parse_probe("{'action':'db','query':'cargo','limit':3}"),
            Probe::Db { query, limit } if query == "cargo" && limit == 3
        ));
    }

    /// DB search: all query words must match, most-used first, honours limit.
    #[test]
    fn db_search_filters_orders_and_limits() {
        use crate::commands_db::SavedCommand;
        let saved = vec![
            SavedCommand { id: 1, command: "git status".into(), comment: "tree".into(), tags: "git".into(), uses: 1 },
            SavedCommand { id: 2, command: "git log --oneline".into(), comment: "history".into(), tags: "git".into(), uses: 9 },
            SavedCommand { id: 3, command: "cargo run -q".into(), comment: "run silently".into(), tags: "rust".into(), uses: 5 },
        ];
        // Both words must match somewhere across command/comment/tags.
        let hits = search_saved(&saved, "git history", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 2);
        // Empty query lists most-used first.
        let hits = search_saved(&saved, "", 10);
        assert_eq!(hits.iter().map(|c| c.id).collect::<Vec<_>>(), vec![2, 3, 1]);
        // Limit truncates; counts ignore it.
        let hits = search_saved(&saved, "git", 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 2);
        assert_eq!(count_saved_matches(&saved, "git"), 2);
        assert_eq!(count_saved_matches(&saved, ""), 3);
        // Rendered rows carry id, uses, command and comment.
        let text = format_saved(&search_saved(&saved, "cargo", 10));
        assert!(text.contains("cargo run -q") && text.contains("run silently"), "text: {text}");
        assert!(format_saved(&search_saved(&saved, "nope-xyz", 10)).contains("no saved commands match"));
    }

    /// The per-turn timeout kills a hung AI CLI instead of wedging the
    /// worker; the default budget is 60s.
    #[test]
    fn ai_timeout_kills_hung_cli() {
        assert_eq!(DEFAULT_AI_TIMEOUT, std::time::Duration::from_secs(60));
        // Sleeps ~3-5s; the 1s budget must kill it first.
        #[cfg(windows)]
        let hang = "ping -n 4 127.0.0.1 >nul";
        #[cfg(not(windows))]
        let hang = "sleep 5";
        let err = run_full(hang, "", "q", &[], std::time::Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("timed out after 1s"), "err: {err}");
    }

    /// End-to-end: a fake AI CLI queries the db first, then answers from the
    /// fed-back rows. Only matching rows cross into the conversation.
    #[test]
    fn agentic_loop_queries_db_then_answers() {
        use crate::commands_db::SavedCommand;
        let saved = vec![
            SavedCommand { id: 7, command: "wsl w3m {url}".into(), comment: "web in terminal".into(), tags: "web".into(), uses: 4 },
        ];
        #[cfg(windows)]
        let fake = "findstr \"RESULT\" >nul && (echo ```answer & echo wsl w3m https://example.com & echo ```) || (echo {\"action\":\"db\",\"query\":\"w3m\"})";
        #[cfg(not(windows))]
        let fake = "grep -q 'RESULT' && { echo '```answer'; echo 'wsl w3m https://example.com'; echo '```'; } || echo '{\"action\":\"db\",\"query\":\"w3m\"}'";
        let out = run_with_db(fake, "Goal: make a command.", "browse the web", &saved).unwrap();
        assert_eq!(out, "wsl w3m https://example.com");
    }

    /// ASCII folding transliterates punctuation and letters; plain ASCII
    /// passes through untouched.
    #[test]
    fn fold_to_ascii_transliterates() {
        assert_eq!(fold_to_ascii("a — b"), "a -- b");
        assert_eq!(fold_to_ascii("Grüße"), "Grusse");
        assert_eq!(fold_to_ascii("plain ascii 123"), "plain ascii 123");
        assert_ne!(fold_to_ascii("lead with — dash"), "lead with — dash");
    }

    /// Crash signature: OS exception + zero output. Normal failures
    /// (exit 1, partial output) never trigger the folded retry.
    #[test]
    fn startup_crash_detected() {
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            let crashed = std::process::ExitStatus::from_raw(0xC0000409);
            assert!(looks_like_startup_crash(&Some(crashed), b"", b""));
            assert!(!looks_like_startup_crash(&Some(crashed), b"partial", b""));
            assert!(!looks_like_startup_crash(&Some(crashed), b"", b"nope"));
            let failed = std::process::ExitStatus::from_raw(1);
            assert!(!looks_like_startup_crash(&Some(failed), b"", b""));
            assert!(!looks_like_startup_crash(&None, b"", b""));
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::process::ExitStatusExt;
            let ok = std::process::ExitStatus::from_raw(0);
            assert!(!looks_like_startup_crash(&Some(ok), b"", b""));
            let failed = std::process::ExitStatus::from_raw(1 << 8); // exit(1)
            assert!(!looks_like_startup_crash(&Some(failed), b"", b""));
        }
    }

    /// End-to-end: a startup crash (exception, no output) retries once with
    /// an ASCII-folded prompt instead of failing. First turn crashes, the
    /// folded retry answers.
    #[test]
    fn startup_crash_retries_with_folded_prompt() {
        let flag = std::env::temp_dir().join(format!(
            "termrs-fold-retry-{}-{}.flag",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&flag);
        let flag_s = flag.display().to_string();
        #[cfg(windows)]
        let fake = format!(
            "if exist \"{flag_s}\" (echo ```answer & echo recovered-after-fold & echo ```) else (echo x > \"{flag_s}\" & exit 3221226505)"
        );
        #[cfg(not(windows))]
        let fake = format!(
            "if [ -f '{flag_s}' ]; then echo '```answer'; echo 'recovered-after-fold'; echo '```'; else touch '{flag_s}'; kill -11 $$; fi"
        );
        // Non-ASCII lead so the folded retry actually differs (else no retry).
        let out = run_full(
            &fake,
            "lead with — dash",
            "q",
            &[],
            std::time::Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(out, "recovered-after-fold");
        let _ = std::fs::remove_file(&flag);
    }
}

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

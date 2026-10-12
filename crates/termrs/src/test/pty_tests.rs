    use super::*;

    /// Shell for tests that assert exact prompt/echo text. On Windows
    /// `cmd.exe /d` skips AutoRun, so a developer's Clink/oh-my-posh injection
    /// can't rewrite the prompt (and hide the OSC 133 marker); elsewhere the
    /// default `$SHELL`/`sh` is already deterministic.
    fn deterministic_shell() -> &'static str {
        if cfg!(windows) { "cmd.exe /d" } else { "" }
    }

    /// Probe raw PTY reads on a background thread (bounded wait), logging
    /// each result plus spawn diagnostics.
    #[test]
    #[ignore = "diagnostic probe; run with --ignored"]
    fn raw_read_probe() {
        println!("COMSPEC={:?}", std::env::var("COMSPEC"));
        println!("default_shell={:?}", default_shell());
        let system = native_pty_system();
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = system.openpty(size).expect("openpty");
        let mut cmd = CommandBuilder::new(default_shell());
        cmd.arg("/c");
        cmd.arg("echo hello_from_pty");
        cmd.cwd(std::env::current_dir().unwrap());
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        println!("spawned, pid={:?}", child.process_id());
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let mut writer = pair.master.take_writer().expect("writer");
        let _ = writer.write_all(b"echo probe123\r");
        let _ = writer.flush();

        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send("Ok(0)".into());
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(n) => {
                        if tx
                            .send(format!("Ok({n}) {:?}", String::from_utf8_lossy(&buf[..n])))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(format!("Err({e}) kind={:?}", e.kind()));
                        break;
                    }
                }
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = 0usize;
        while std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.recv_timeout(Duration::from_millis(200)) {
                got += 1;
                println!("recv: {msg}");
            }
        }
        println!("wait_status={:?}", child.try_wait());
        println!("messages={got}");
    }

    /// Probe: does writing to the PTY reach the shell and come back?
    #[test]
    fn shell_echoes_input() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn pane");
        // Give the shell a moment to print its banner.
        for _ in 0..40 {
            pane.poll();
            std::thread::sleep(Duration::from_millis(25));
        }
        pane.write(b"echo termrs_probe\r");
        let mut seen = false;
        for _ in 0..200 {
            pane.poll();
            if pane.screen().contents().contains("termrs_probe") {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(seen, "shell never echoed input; screen:\n{}", pane.screen().contents());
        pane.kill();
        rt.shutdown_background();
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termrs-pty-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `spawn_in` starts the shell in the given directory (tracked cwd
    /// included); a missing directory falls back to the process directory
    /// instead of failing the spawn.
    #[test]
    fn spawn_in_starts_shell_in_given_dir() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let dir = temp_dir("spawn-in");
        let mut pane = Pane::spawn_in(0, "", 200, rt.handle(), &wake, &dir).expect("spawn in dir");
        assert_eq!(pane.cwd(), dir.as_path());
        pane.kill();

        let missing = dir.join("gone-missing");
        let mut fallback =
            Pane::spawn_in(1, "", 200, rt.handle(), &wake, &missing).expect("spawn falls back");
        assert_eq!(
            fallback.cwd(),
            std::env::current_dir().unwrap().as_path()
        );
        fallback.kill();
        let _ = std::fs::remove_dir_all(&dir);
        rt.shutdown_background();
    }

    /// With the OSC 133 prompt marker, yank returns only the command output.
    #[test]
    fn yank_excludes_prompt_and_echo() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, deterministic_shell(), 200, rt.handle(), &wake).expect("spawn");
        // Let the first prompt (with its marker) arrive.
        for _ in 0..60 {
            pane.poll();
            std::thread::sleep(Duration::from_millis(25));
            if !pane.markers.is_empty() {
                break;
            }
        }
        pane.write(b"echo hello world\r");
        let mut text = String::new();
        for _ in 0..200 {
            pane.poll();
            text = pane.last_output();
            if text.trim() == "hello world" {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        pane.kill();
        let got = text.trim();
        assert_eq!(got, "hello world", "prompt/echo stripped (capture={:?})", pane.capture);
        rt.shutdown_background();
    }

    /// The cwd query parses the shell's answer (and ignores its own echo).
    #[test]
    fn cwd_query_parses_shell_reply() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn");
        pane.parse_cwd("echo TERMRS_CWD:%CD%\r\n");
        assert_eq!(pane.cwd(), std::env::current_dir().unwrap().as_path());
        pane.parse_cwd("TERMRS_CWD:C:\\Users\\me\\project\r\n");
        assert_eq!(pane.cwd(), std::path::Path::new("C:\\Users\\me\\project"));
        pane.kill();
        rt.shutdown_background();
    }

    /// Prompts move the tracked cwd (what `cd`, `pushd` and the yazi
    /// `y.cmd` wrapper print), while lookalikes and gone dirs do not.
    #[test]
    fn prompt_lines_move_tracked_cwd() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn");
        let base = std::env::temp_dir().join(format!(
            "termrs-prompt-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir_a = base.join("a");
        let dir_b = base.join("my dir");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let seq0 = pane.cwd_seq();

        // Plain cmd prompt, then a `cd` to another dir (incl. a space).
        pane.feed_text_for_test(&format!("{}\r\n{}>", dir_a.display(), dir_a.display()), &[]);
        assert_eq!(pane.cwd(), dir_a.as_path());
        assert!(pane.cwd_seq() != seq0);
        pane.feed_text_for_test(
            &format!("cd /d \"{}\"\r\n{}>", dir_b.display(), dir_b.display()),
            &[],
        );
        assert_eq!(pane.cwd(), dir_b.as_path());
        // PowerShell shape and glued output both resolve.
        pane.feed_text_for_test(&format!("PS {}>", dir_a.display()), &[]);
        assert_eq!(pane.cwd(), dir_a.as_path());
        pane.feed_text_for_test(&format!("done{}>", dir_b.display()), &[]);
        assert_eq!(pane.cwd(), dir_b.as_path());
        // Clink-style bracketed prompt (`┖[dir]`) moves it as well.
        pane.feed_text_for_test(&format!("┖[{}]", dir_a.display()), &[]);
        assert_eq!(pane.cwd(), dir_a.as_path());
        // OSC 7 reports apply too.
        pane.feed_text_for_test("", std::slice::from_ref(&dir_a));
        assert_eq!(pane.cwd(), dir_a.as_path());

        // Gone dirs, bare markers and ordinary output are ignored.
        let seq = pane.cwd_seq();
        pane.feed_text_for_test("Z:\\definitely-missing-termrs>\r\n", &[]);
        pane.feed_text_for_test(">\r\n$\r\nsee you later>\r\n", &[]);
        pane.feed_text_for_test("", &[std::path::PathBuf::from("Z:\\missing-too")]);
        assert_eq!(pane.cwd(), dir_a.as_path());
        assert_eq!(pane.cwd_seq(), seq);
        pane.kill();
        let _ = std::fs::remove_dir_all(&base);
        rt.shutdown_background();
    }

    #[test]
    fn paste_wraps_in_bracketed_markers_when_enabled() {
        // Raw text when bracketed mode is off.
        assert_eq!(paste_bytes("hi", false), b"hi".to_vec());
        // Wrapped when the program enabled bracketed paste.
        assert_eq!(
            paste_bytes("hi", true),
            b"\x1b[200~hi\x1b[201~".to_vec()
        );
    }

    #[test]
    fn bracketed_paste_mode_follows_the_stream() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn");
        assert!(!pane.screen().bracketed_paste());
        pane.feed_for_test(b"\x1b[?2004h");
        assert!(pane.screen().bracketed_paste(), "enabled by the program");
        pane.feed_for_test(b"\x1b[?2004l");
        assert!(!pane.screen().bracketed_paste(), "disabled again");
        pane.kill();
        rt.shutdown_background();
    }

    /// DECSET mouse modes map onto report behavior: 9 press-only, 1000
    /// press+release, 1002 adds drag motion, 1003 adds free motion, and
    /// 1006 picks SGR encoding.
    #[test]
    fn mouse_state_follows_decset() {
        use crate::mouse::MouseEncoding;
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn");
        let off = pane.mouse_state();
        assert!(!off.tracking, "no tracking by default");
        assert_eq!(off.encoding, MouseEncoding::X10);

        // DECSET 9 (X10): presses only.
        pane.feed_for_test(b"\x1b[?9h");
        let press = pane.mouse_state();
        assert!(press.tracking);
        assert!(!press.release, "9 reports no releases");
        assert!(!press.wants_motion(false) && !press.wants_motion(true));
        pane.feed_for_test(b"\x1b[?9l");

        // DECSET 1000: press + release.
        pane.feed_for_test(b"\x1b[?1000h");
        let rel = pane.mouse_state();
        assert!(rel.tracking && rel.release);
        assert!(!rel.wants_motion(false) && !rel.wants_motion(true));

        pane.feed_for_test(b"\x1b[?1006h");
        let sgr = pane.mouse_state();
        assert!(sgr.tracking && sgr.release);
        assert_eq!(sgr.encoding, MouseEncoding::Sgr);

        // DECSET 1002: drag motion while held, but no free motion.
        pane.feed_for_test(b"\x1b[?1002h");
        let drag = pane.mouse_state();
        assert!(drag.tracking && drag.release);
        assert!(!drag.wants_motion(false), "no free motion in 1002");
        assert!(drag.wants_motion(true), "drag motion in 1002");

        // DECSET 1003: motion regardless of buttons.
        pane.feed_for_test(b"\x1b[?1003h");
        let motion = pane.mouse_state();
        assert!(motion.tracking && motion.release);
        assert!(motion.wants_motion(false), "any-motion reports freely");
        assert!(motion.wants_motion(true));

        pane.feed_for_test(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l");
        assert!(!pane.mouse_state().tracking, "disabled again");
        pane.kill();
        rt.shutdown_background();
    }

    #[test]
    fn grid_lines_cover_pane_width() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn pane");
        let size = pane.screen().size();
        let lines = pane.grid_lines();
        let mut pane = pane;
        pane.kill();
        assert_eq!(lines.len(), size.0 as usize, "one line per row");
        assert_eq!(
            lines.first().map(|l| l.chars().count()),
            Some(size.1 as usize),
            "each line padded to the pane width, size={size:?}"
        );
        rt.shutdown_background();
    }

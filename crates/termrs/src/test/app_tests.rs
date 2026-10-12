    use super::*;
    use crate::config::Config;

    fn key(key: Key, mods: Mods) -> KeyPress {
        KeyPress {
            key,
            mods,
            text: None,
            kind: KeyKind::Press,
        }
    }

    /// Closing a pane keeps it in the tree with a Close transition queued,
    /// then removes it once the effect deadline passes.
    #[test]
    fn close_pane_plays_transition_then_removes() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("close");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        assert_eq!(app.ws().leaf_ids().len(), 1);

        // ctrl+s splits (default bindings), ctrl+w closes.
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().leaf_ids().len(), 2);

        app.handle_key(key(Key::Char('w'), Mods::CONTROL));
        // Still present while the close transition plays.
        assert_eq!(app.ws().leaf_ids().len(), 2);
        assert!(!app.closing.is_empty());
        let pending = app.drain_fx();
        assert!(pending.iter().any(|p| p.kind == FxKind::Close));

        // After the deadline the leaf is gone; the layout is only persisted
        // on demand, never automatically.
        std::thread::sleep(std::time::Duration::from_millis(500));
        app.poll_panes();
        assert!(app.closing.is_empty());
        assert_eq!(app.ws().leaf_ids().len(), 1);
        assert!(!layout.is_file(), "layout must not be auto-saved");
        app.run_command(Command::SaveLayout);
        assert!(layout.is_file(), "Save layout command persists the layout");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// With fx disabled the pane is removed immediately.
    #[test]
    fn close_pane_instant_when_fx_disabled() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("close-off");
        let mut cfg = Config::default();
        cfg.fx.enabled = false;
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().leaf_ids().len(), 2);
        app.handle_key(key(Key::Char('w'), Mods::CONTROL));
        assert!(app.closing.is_empty());
        assert_eq!(app.ws().leaf_ids().len(), 1);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// ctrl+shift+z maximizes the focused pane; focus keys restore the layout
    /// AND move in one press (directional focus uses tiled geometry);
    /// splitting clears zoom.
    #[test]
    fn zoom_pane_toggle_and_focus_moves() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("zoom-pane");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        // ctrl+shift+z is bound to ZoomPane by default.
        assert!(matches!(
            app.lookup_command(&key(Key::Char('Z'), Mods::CONTROL | Mods::SHIFT)),
            Some(Command::ZoomPane)
        ));
        // Single pane: zoom is a no-op.
        app.run_command(Command::ZoomPane);
        assert_eq!(app.ws().zoomed(), None);

        // Split (stacked: old on top, new focused below), then maximize the
        // focused pane via the real keypress.
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        let ids = app.ws().leaf_ids();
        assert_eq!(ids.len(), 2);
        let (top, bottom) = (ids[0], ids[1]);
        assert_eq!(app.ws().focused, bottom);
        app.handle_key(key(Key::Char('Z'), Mods::CONTROL | Mods::SHIFT));
        assert_eq!(app.ws().zoomed(), Some(bottom));
        assert!(app.status.contains("maximized"));

        // Seed tiled geometry the way a drawn frame would (tests never draw).
        app.ws_mut().set_tiled_rects(&[
            (top, ratatui::layout::Rect::new(0, 0, 80, 20)),
            (bottom, ratatui::layout::Rect::new(0, 20, 80, 20)),
        ]);
        // A focus key restores AND moves in one press.
        app.run_command(Command::FocusUp);
        assert_eq!(app.ws().zoomed(), None);
        assert_eq!(app.ws().focused, top);
        assert!(app.status.contains("restored"));
        // Cycling also restores and moves (wraps to the other pane).
        app.run_command(Command::ZoomPane);
        assert_eq!(app.ws().zoomed(), Some(top));
        app.run_command(Command::FocusNext);
        assert_eq!(app.ws().zoomed(), None);
        assert_eq!(app.ws().focused, bottom);
        // Nothing that way: still restores, focus stays put.
        app.run_command(Command::ZoomPane);
        app.run_command(Command::FocusDown);
        assert_eq!(app.ws().zoomed(), None);
        assert_eq!(app.ws().focused, bottom);

        // Toggling twice restores as well.
        app.run_command(Command::ZoomPane);
        assert!(app.ws().zoomed().is_some());
        app.run_command(Command::ZoomPane);
        assert_eq!(app.ws().zoomed(), None);

        // Splitting while zoomed restores first so the new pane is visible.
        app.run_command(Command::ZoomPane);
        assert!(app.ws().zoomed().is_some());
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().zoomed(), None);
        assert_eq!(app.ws().leaf_ids().len(), 3);

        // Closing the zoomed pane clears the zoom.
        let target = app.ws().focused;
        app.run_command(Command::ZoomPane);
        assert_eq!(app.ws().zoomed(), Some(target));
        app.ws_mut().remove_leaf(target);
        assert_eq!(app.ws().zoomed(), None);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Splitting opens the new shell in the focused pane's directory; a
    /// deleted directory falls back to the process directory.
    #[test]
    fn split_inherits_focused_cwd() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("split-cwd");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let dir = std::env::temp_dir().join(format!(
            "termrs-split-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Pretend the focused shell reported this directory.
        let fid = app.ws().focused;
        app.ws_mut()
            .pane_mut(fid)
            .expect("focused pane")
            .set_cwd_for_test(dir.clone());
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().leaf_ids().len(), 2);
        let nid = app.ws().focused;
        assert_ne!(nid, fid);
        assert_eq!(
            app.ws().pane(nid).expect("new pane").cwd(),
            dir.as_path(),
            "split shell starts in the focused directory"
        );

        // Deleted directory: split still works, from the process directory.
        let gone = dir.join("gone-missing");
        app.ws_mut()
            .pane_mut(nid)
            .expect("new pane")
            .set_cwd_for_test(gone);
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().leaf_ids().len(), 3);
        let nid2 = app.ws().focused;
        assert_eq!(
            app.ws().pane(nid2).expect("third pane").cwd(),
            std::env::current_dir().unwrap().as_path()
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Helix-style text objects and g-motions.
    #[test]
    fn select_text_objects_and_goto() {
        let line = "let foo = bar(1, 2);".to_string();
        // miw: inside word under cursor.
        let mut sel = SelectMode::from_lines(vec![line.clone()], (0, 5));
        sel.select_object('w', true);
        assert_eq!(sel.selected_text(), "foo");
        // maw includes trailing spaces.
        let mut sel = SelectMode::from_lines(vec![line.clone()], (0, 5));
        sel.select_object('w', false);
        assert_eq!(sel.selected_text(), "foo");
        // mi( : inside parentheses.
        let mut sel = SelectMode::from_lines(vec![line.clone()], (0, 15));
        sel.select_object('(', true);
        assert_eq!(sel.selected_text(), "1, 2");
        // ma( : around parentheses.
        let mut sel = SelectMode::from_lines(vec![line.clone()], (0, 15));
        sel.select_object('(', false);
        assert_eq!(sel.selected_text(), "(1, 2)");
        // gl / gh / gg / ge.
        let mut sel = SelectMode::from_lines(vec![line.clone(); 3], (1, 4));
        assert!(sel.goto('l'));
        assert_eq!(sel.cursor(), (1, 19));
        assert!(sel.goto('h'));
        assert_eq!(sel.cursor(), (1, 0));
        assert!(sel.goto('g'));
        assert_eq!(sel.cursor(), (0, 0));
        assert!(sel.goto('e'));
        assert_eq!(sel.cursor(), (2, 0));
        assert!(!sel.goto('z'));
    }

    /// `;` swaps the selection ends; `gl` hits the last char; `f`/`F` find.
    #[test]
    fn select_swap_goto_and_find() {
        let line = "a,b,c   ".to_string();
        let mut sel = SelectMode::from_lines(vec![line], (0, 0));
        sel.anchor = Some((0, 2));
        sel.cur = (0, 4);
        sel.swap_ends();
        assert_eq!(sel.anchor, Some((0, 4)));
        assert_eq!(sel.cur, (0, 2));
        // gl lands on the last non-blank character, not the padded end.
        sel.goto('l');
        assert_eq!(sel.cur, (0, 4));
        // f forward to the next ','.
        sel.cur = (0, 0);
        sel.find_char(',', true);
        assert_eq!(sel.cur, (0, 1));
        // F backward from the end.
        sel.cur = (0, 4);
        sel.find_char(',', false);
        assert_eq!(sel.cur, (0, 3));
        // Missing target leaves the cursor put.
        sel.cur = (0, 1);
        sel.find_char('z', true);
        assert_eq!(sel.cur, (0, 1));
    }

    /// Helix word motions: `w`/`b`/`e` on words, `W`/`B`/`E` on WORDs,
    /// crossing line boundaries.
    #[test]
    fn select_helix_word_motions() {
        let lines = vec!["foo bar  ".to_string(), "  baz".to_string()];
        let mut sel = SelectMode::from_lines(lines.clone(), (0, 0));
        // w: next word start.
        sel.word_forward(false);
        assert_eq!(sel.cursor(), (0, 4));
        sel.word_forward(false);
        assert_eq!(sel.cursor(), (1, 2));
        // At the last word: stay.
        sel.word_forward(false);
        assert_eq!(sel.cursor(), (1, 4));
        // b: to the start of the same word, then previous (crosses lines).
        sel.word_backward(false);
        assert_eq!(sel.cursor(), (1, 2));
        sel.word_backward(false);
        assert_eq!(sel.cursor(), (0, 4));
        sel.word_backward(false);
        assert_eq!(sel.cursor(), (0, 0));
        // At the first cell: stay.
        sel.word_backward(false);
        assert_eq!(sel.cursor(), (0, 0));
        // e: next word end.
        sel.word_end(false);
        assert_eq!(sel.cursor(), (0, 2));
        sel.word_end(false);
        assert_eq!(sel.cursor(), (0, 6));
        // WORD motions skip punctuation runs: "foo,bar baz".
        let mut sel = SelectMode::from_lines(vec!["foo,bar baz".to_string()], (0, 0));
        sel.word_forward(true);
        assert_eq!(sel.cursor(), (0, 8));
        sel.cur = (0, 0);
        sel.word_end(true);
        assert_eq!(sel.cursor(), (0, 6));
        sel.word_backward(true);
        assert_eq!(sel.cursor(), (0, 0));
        // Lowercase stops at punctuation.
        let mut sel = SelectMode::from_lines(vec!["foo,bar baz".to_string()], (0, 0));
        sel.word_forward(false);
        assert_eq!(sel.cursor(), (0, 4));
    }

    /// Helix line selections: `x` (extend, then next line), `X` (bounds),
    /// `%` (whole grid), `;` (collapse onto cursor).
    #[test]
    fn select_helix_line_objects() {
        let lines = vec!["ab  ".to_string(), "cdef".to_string()];
        // x selects the cursor's line up to the last non-blank.
        let mut sel = SelectMode::from_lines(lines.clone(), (1, 1));
        sel.extend_line();
        assert_eq!(sel.anchor, Some((1, 0)));
        assert_eq!(sel.cursor(), (1, 3));
        assert_eq!(sel.selected_text(), "cdef");
        // x again on a full line extends through the next line's end.
        let mut sel = SelectMode::from_lines(lines.clone(), (0, 0));
        sel.extend_line();
        assert_eq!(sel.selected_text(), "ab");
        sel.cur = (0, 1);
        sel.extend_line();
        assert_eq!(sel.selected_text(), "ab\ncdef");
        // X extends to the line bounds.
        let mut sel = SelectMode::from_lines(lines.clone(), (1, 2));
        sel.extend_line_bounds();
        assert_eq!(sel.anchor, Some((1, 0)));
        assert_eq!(sel.cursor(), (1, 3));
        // % selects the whole grid.
        let mut sel = SelectMode::from_lines(lines.clone(), (1, 1));
        sel.select_all();
        assert_eq!(sel.anchor, Some((0, 0)));
        assert_eq!(sel.cursor(), (1, 3));
        // ; collapses onto the cursor (zero-width, still a range).
        sel.collapse();
        assert_eq!(sel.anchor, Some((1, 3)));
        assert_eq!(sel.range(), Some((1, 3, 1, 3)));
    }

    /// Helix `t`/`T` stop one cell short; `mm` jumps to the matching bracket.
    #[test]
    fn select_helix_till_and_match() {
        let mut sel = SelectMode::from_lines(vec!["abca".to_string()], (0, 0));
        sel.find_till('c', true);
        assert_eq!(sel.cursor(), (0, 1));
        // Adjacent target: stay put. No match ahead: stay put.
        sel.cur = (0, 0);
        sel.find_till('b', true);
        assert_eq!(sel.cursor(), (0, 0));
        sel.find_till('z', true);
        assert_eq!(sel.cursor(), (0, 0));
        sel.cur = (0, 3);
        sel.find_till('a', false);
        assert_eq!(sel.cursor(), (0, 1));
        // mm jumps between matching brackets, nesting-aware.
        let mut sel = SelectMode::from_lines(vec!["((a))".to_string()], (0, 0));
        sel.match_bracket();
        assert_eq!(sel.cursor(), (0, 4));
        sel.match_bracket();
        assert_eq!(sel.cursor(), (0, 0));
        let mut sel = SelectMode::from_lines(vec!["[x] (y)".to_string()], (0, 4));
        sel.match_bracket();
        assert_eq!(sel.cursor(), (0, 6));
        // Not on a bracket: stay put.
        let mut sel = SelectMode::from_lines(vec!["abc".to_string()], (0, 1));
        sel.match_bracket();
        assert_eq!(sel.cursor(), (0, 1));
    }

    /// Edit command quoting (paths with spaces work).
    #[test]
    fn edit_invocation_quotes_path() {
        let p = std::path::Path::new("C:\\a b\\config.toml");
        let cmd = edit_invocation("notepad", p);
        assert!(cmd.starts_with("notepad "), "cmd: {cmd}");
        assert!(cmd.contains("config.toml"), "cmd: {cmd}");
        // The path is quoted for the platform shell.
        assert!(
            cmd.contains("\"C:\\a b\\config.toml\"") || cmd.contains("'C:\\a b\\config.toml'"),
            "cmd: {cmd}"
        );
    }

    /// `cursor_cell` maps the focused pane's cursor into the inner (border-less)
    /// pane area, for the GPU bar/underline cursor.
    #[test]
    fn cursor_cell_uses_inner_pane_area() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cursorcell");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 20, 10))]);
        let (inner, row, col) = app.cursor_cell().expect("cursor visible");
        assert_eq!(inner, ratatui::layout::Rect::new(1, 1, 18, 8));
        assert_eq!((row, col), (0, 0));
        // Default theme asks for a bar cursor.
        assert_eq!(
            app.theme_colors().cursor_shape,
            crate::config::CursorShape::Bar
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        let _ = std::fs::remove_file(&layout);
    }

    /// While selecting, the GPU cursor hides: the selection overlay owns the
    /// cursor, so only the highlighted end shows.
    #[test]
    fn cursor_cell_hidden_while_selecting() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cursorselect");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 20, 10))]);
        assert!(app.cursor_cell().is_some(), "cursor visible normally");
        app.toggle_select();
        assert!(app.select.is_some(), "select mode entered");
        assert!(
            app.cursor_cell().is_none(),
            "GPU cursor hidden while selecting"
        );
        app.toggle_select();
        assert!(app.cursor_cell().is_some(), "cursor back after leaving");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        let _ = std::fs::remove_file(&layout);
    }

    /// Empty input completes from the shell's cwd; images rank first.
    #[test]
    fn completion_lists_directory_from_cwd() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("complete");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let dir = std::env::temp_dir().join(format!("termrs-cwd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("zed.png"), b"x").unwrap();
        std::fs::write(dir.join("alpha.txt"), b"x").unwrap();
        let fid = app.ws().focused;
        app.ws_mut()
            .pane_mut(fid)
            .expect("focused pane")
            .set_cwd_for_test(dir.clone());

        // Empty input -> completes an entry from the directory (image first).
        let ghost = app.complete_path("").expect("a completion");
        assert_eq!(ghost, "zed.png", "completes from empty input: {ghost:?}");
        // Typed prefix narrows it.
        assert_eq!(app.complete_path("al").expect("match"), "pha.txt");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// `exit` in the only pane quits the app instead of lingering.
    #[test]
    fn exit_in_last_pane_quits() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("exit");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .pane_mut(fid)
            .expect("focused pane")
            .write(b"exit\r");
        let mut quit = false;
        for _ in 0..200 {
            app.poll_panes();
            if app.should_quit {
                quit = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(quit, "app quits when its last shell exits");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// New workspaces reuse the smallest free `ws-N` number.
    #[test]
    fn workspace_index_reused_after_close() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("idx");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.new_workspace();
        app.new_workspace();
        assert_eq!(app.workspaces[1].name, "ws-1");
        assert_eq!(app.workspaces[2].name, "ws-2");
        // Close ws-1 (switch to it first), then create: ws-1 comes back.
        app.switch_to(1);
        app.close_current_workspace();
        assert_eq!(app.workspaces.len(), 2);
        app.new_workspace();
        assert_eq!(app.workspaces[2].name, "ws-1");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        // Detach without waiting for stuck PTY readers (see run_inner).
        rt.shutdown_background();
    }

    /// Zoom keys queue a font-size request for the window host.
    #[test]
    fn zoom_queues_font_size_request() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("zoom");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        assert_eq!(app.font_size, 16);
        app.handle_key(key(Key::Char('+'), Mods::CONTROL));
        assert_eq!(app.take_zoom_request(), Some(17));
        assert_eq!(app.font_size, 17);
        app.handle_key(key(Key::Char('-'), Mods::CONTROL));
        assert_eq!(app.take_zoom_request(), Some(16));
        app.handle_key(key(Key::Char('0'), Mods::CONTROL));
        assert_eq!(app.take_zoom_request(), None);
        assert!(app.status.contains("16px"));
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The font picker filters fuzzily and always offers the system default
    /// first (pure logic, no panes needed).
    #[test]
    fn font_picker_filters_and_lists_default_first() {
        let entries = vec![
            FontEntry {
                name: "Zeta".to_string(),
                path: PathBuf::from("/fonts/Zeta.ttf"),
                mono: Some(true),
            },
            FontEntry {
                name: "alpha".to_string(),
                path: PathBuf::from("/fonts/alpha.ttf"),
                mono: Some(false),
            },
        ];
        let mut p = FontPicker::new(entries, "/fonts/Zeta.ttf", 18);
        assert_eq!(p.results.len(), 3);
        assert!(p.results[0].item.path.as_os_str().is_empty());
        assert_eq!(p.active, "/fonts/Zeta.ttf");
        assert_eq!(p.size, 18);
        assert_eq!(p.focus, PickerFocus::List);
        // The active font is preselected so its preview shows immediately.
        assert_eq!(p.results[p.selected].item.name, "Zeta");
        p.query = "alp".to_string();
        p.refilter();
        assert_eq!(p.results.len(), 1);
        assert_eq!(p.results[0].item.name, "alpha");
        // The probe flag survives filtering (drives the row warning tag).
        assert_eq!(p.results[0].mono, Some(false));
        p.query = "zzz-no-match".to_string();
        p.refilter();
        assert!(p.results.is_empty());
        p.query.clear();
        p.refilter();
        assert_eq!(p.results.len(), 3);
    }

    /// Visible-row probing fills unknown flags and the cache survives
    /// refilters; unparsable files stay untagged without panicking.
    #[test]
    fn font_probe_visible_fills_and_caches() {
        let dir = std::env::temp_dir().join(format!(
            "termrs-fontprobe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("junk.ttf");
        std::fs::write(&junk, b"not a font").unwrap();
        let mut p = FontPicker::new(
            vec![FontEntry {
                name: "junk".to_string(),
                path: junk,
                mono: None,
            }],
            "",
            16,
        );
        p.probe_visible(0, 10);
        assert!(p.results[1].mono.is_none(), "unparsable stays untagged");

        // A real installed font (when one exists) probes to Some and the
        // flag survives refiltering.
        if let Some(real) = crate::font::scan_fonts().into_iter().next() {
            let mut p = FontPicker::new(vec![real], "", 16);
            p.probe_visible(0, 10);
            assert!(p.results[1].mono.is_some());
            p.query = "zzz-no-match-ever".to_string();
            p.refilter();
            p.query.clear();
            p.refilter();
            assert!(p.results[1].mono.is_some(), "cached across refilters");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Picking a font updates `[general] font` in memory and on disk and
    /// queues a live rebuild for the window host.
    #[test]
    fn font_pick_updates_config_and_queues_rebuild() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let dir = std::env::temp_dir().join(format!(
            "termrs-fontpick-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "# comment\n[general]\nfont_size = 16\n").unwrap();
        let fake_font = dir.join("Fake.ttf");
        std::fs::write(&fake_font, b"not a real font, only readable").unwrap();
        let layout = dir.join("layout.toml");

        let cfg = Config::load(Some(cfg_path.as_path()));
        let mut app =
            App::new(cfg, Some(layout), rt.handle(), &wake, None, None, false).expect("app boots");
        let entry = FontEntry {
            name: "Fake".to_string(),
            path: fake_font.clone(),
            mono: None,
        };
        app.font_picker = Some(FontPicker::new(vec![entry], "", 16));
        // Move past the "system default" first row onto the fake font.
        app.handle_key(key(Key::Down, Mods::empty()));
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.font_picker.is_none(), "picker closes on pick");
        let want = fake_font.to_string_lossy().into_owned();
        assert_eq!(app.config.general.font, want);
        assert_eq!(app.config.general.font_size, 16);
        assert_eq!(app.take_font_request(), Some(want.clone()));
        assert!(app.status.contains("Fake"), "status: {}", app.status);
        let on_disk = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(on_disk.contains("# comment"), "comments survive: {on_disk}");
        let reloaded: Config = toml::from_str(&on_disk).unwrap();
        assert_eq!(reloaded.general.font, want);
        assert_eq!(reloaded.general.font_size, 16);

        // The default entry clears the field back to auto-detect.
        app.font_picker = Some(FontPicker::new(Vec::new(), &want, 16));
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert_eq!(app.config.general.font, "");
        assert_eq!(app.take_font_request(), Some(String::new()));

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The preview worker answers off-thread: the latest request wins, stale
    /// results are dropped, repeats are not re-sent, and closing the picker
    /// drops the worker channels.
    #[test]
    fn font_preview_worker_delivers_latest_only() {
        let Some(real) = crate::font::scan_fonts().into_iter().next() else {
            return; // No installed fonts to render; nothing to prove.
        };
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("fontpreview");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let entry = FontEntry {
            name: real.name.clone(),
            path: real.path.clone(),
            mono: None,
        };
        app.font_picker = Some(FontPicker::new(vec![entry], "", 24));
        // Past the "system default" row onto the real font.
        app.font_picker.as_mut().expect("picker").selected = 1;
        app.start_preview_worker();
        assert!(!app.font_preview_loading());

        // Two rapid requests (simulating fast up/down): only the second
        // generation may land on screen.
        app.request_font_preview();
        let gen1 = app.font_preview_gen;
        assert!(app.font_preview_loading());
        app.font_picker.as_mut().expect("picker").nudge_size(8);
        app.request_font_preview();
        assert_eq!(app.font_preview_gen, gen1 + 1);
        // Same (path, size) twice: no resend, no generation bump.
        app.request_font_preview();
        assert_eq!(app.font_preview_gen, gen1 + 1);

        // Poll until the latest render lands (stale ones are discarded).
        let want_size = app.font_picker.as_ref().expect("picker").size;
        let mut landed = false;
        for _ in 0..400 {
            app.poll_font_preview();
            if !app.font_preview_loading()
                && let Some(((path, size), _, _)) = app.font_preview_meta()
                && path == real.path.to_string_lossy()
                && size == want_size
            {
                landed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(landed, "latest preview never landed");
        assert!(!app.font_preview_loading());

        // Closing the picker drops the worker channels.
        app.cancel_font_picker();
        assert!(app.font_preview_tx.is_none());
        assert!(app.font_preview_rx.is_none());
        assert!(!app.font_preview_loading());

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Tab focuses the size field; arrows/j/k and digits adjust the size
    /// live (queued as a zoom rebuild); Enter commits it to the config.
    #[test]
    fn font_picker_size_field_adjusts_and_commits() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let dir = std::env::temp_dir().join(format!(
            "termrs-fontsize-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "[general]\nfont_size = 16\n").unwrap();
        let layout = dir.join("layout.toml");

        let cfg = Config::load(Some(cfg_path.as_path()));
        let mut app =
            App::new(cfg, Some(layout), rt.handle(), &wake, None, None, false).expect("app boots");
        app.open_font_picker();
        assert_eq!(app.config.general.font_size, 16);

        // Tab -> size field.
        app.handle_key(key(Key::Tab, Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().focus, PickerFocus::Size);
        // Up / j step the size; each queues a live zoom rebuild.
        app.handle_key(key(Key::Up, Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 17);
        assert_eq!(app.take_zoom_request(), Some(17));
        app.handle_key(key(Key::Char('k'), Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 18);
        assert_eq!(app.take_zoom_request(), Some(18));
        app.handle_key(key(Key::Down, Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 17);
        app.handle_key(key(Key::Char('j'), Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 16);
        assert_eq!(app.take_zoom_request(), Some(16));
        // Typing digits sets an exact size.
        app.handle_key(key(Key::Char('2'), Mods::empty()));
        app.handle_key(key(Key::Char('4'), Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 24);
        assert_eq!(app.take_zoom_request(), Some(24));

        // Enter on the system-default row commits size (font stays auto).
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.font_picker.is_none());
        assert_eq!(app.config.general.font, "");
        assert_eq!(app.config.general.font_size, 24);
        let reloaded: Config =
            toml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(reloaded.general.font_size, 24);

        // Esc restores the entry state (font + size) without writing.
        let mut app = app;
        app.open_font_picker();
        app.handle_key(key(Key::Tab, Mods::empty()));
        app.handle_key(key(Key::Up, Mods::empty()));
        assert_eq!(app.font_size, 25);
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(app.font_picker.is_none());
        assert_eq!(app.font_size, 24, "size restored on cancel");
        assert_eq!(app.take_zoom_request(), Some(24));

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A macro binding types its text (plus Enter) into the focused pane.
    #[test]
    fn macro_types_into_focused_pane() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("macro");
        let mut cfg = Config::default();
        cfg.macros.push(crate::config::Macro {
            keys: vec!["ctrl+m".to_string()],
            send: "macro_probe_xyz".to_string(),
            enter: false,
        });
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        assert!(app.handle_key(key(Key::Char('m'), Mods::CONTROL)));
        let mut seen = false;
        for _ in 0..80 {
            app.poll_panes();
            let fid = app.ws().focused;
            if let Some(p) = app.ws().pane(fid)
                && p.screen().contents().contains("macro_probe_xyz")
            {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(seen, "macro text reached the pane");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// POST /command text with doubled characters reaches the pane byte-
    /// intact. Regression test: per-character key events arrived as a
    /// microsecond burst of key-down records with no key-up in between,
    /// and ConPTY coalesced back-to-back identical key-downs (`a--b`
    /// arrived as `a-b`). Both the echoed line and the command output
    /// must contain the marker verbatim.
    #[test]
    fn http_command_preserves_doubled_chars() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("httpcmd");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.handle_http_command("echo probe--xx++yy");
        let mut seen = false;
        for _ in 0..80 {
            app.poll_panes();
            let fid = app.ws().focused;
            if let Some(p) = app.ws().pane(fid)
                && p.screen().contents().contains("probe--xx++yy")
            {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(seen, "doubled chars survived /command injection");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Selection text: whole grid without an anchor, sub-rect with one.
    #[test]
    fn select_mode_extracts_text() {
        let lines = vec![
            "hello world ".to_string(),
            "second line ".to_string(),
            "third       ".to_string(),
        ];
        let mut sel = SelectMode::from_lines(lines, (1, 0));
        assert_eq!(sel.selected_text(), "hello world\nsecond line\nthird");
        sel.anchor = Some((0, 0));
        sel.cur = (1, 5);
        assert_eq!(sel.selected_text(), "hello world\nsecond");
    }

    /// Movement and anchor toggling clamp to the grid.
    #[test]
    fn select_mode_moves_and_clamps() {
        let mut sel = SelectMode::from_lines(vec!["abc".to_string(); 3], (1, 1));
        sel.move_by(5, 5);
        assert_eq!(sel.cursor(), (2, 2));
        sel.move_by(-9, -9);
        assert_eq!(sel.cursor(), (0, 0));
        assert!(sel.range().is_none());
        sel.toggle_anchor();
        assert!(sel.range().is_some());
        sel.toggle_anchor();
        assert!(sel.range().is_none());
    }

    /// Snapshot refresh after a scroll carries the anchor with the content
    /// (and clamps a cursor/anchor that scrolled out of view).
    #[test]
    fn select_refresh_after_scroll_shifts_anchor() {
        let mut sel = SelectMode::from_lines(vec!["a".to_string(), "b".to_string()], (0, 0));
        sel.anchor = Some((0, 0));
        // Scrolled one line up into history: old content moved down one row.
        sel.refresh_after_scroll(vec!["H".to_string(), "a".to_string()], 1);
        assert_eq!(sel.cursor(), (0, 0));
        assert_eq!(sel.anchor, Some((1, 0)));
        // Scrolled back down to live: content moved back up.
        sel.refresh_after_scroll(vec!["a".to_string(), "b".to_string()], -1);
        assert_eq!(sel.anchor, Some((0, 0)));
        // Anchor at the edge that scrolls out of view clamps instead of
        // under/overflowing.
        sel.anchor = Some((1, 0));
        sel.refresh_after_scroll(vec!["H".to_string(), "a".to_string()], 1);
        assert_eq!(sel.anchor, Some((1, 0)));
    }

    /// In select mode, `k` on the top row scrolls back into history (and
    /// `j` on the bottom row scrolls forward), keeping the cursor on its
    /// edge row over the newly revealed line.
    #[test]
    fn select_edge_scrolls_pane_history() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("select-scroll");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let fid = app.ws().focused;
        // More lines than fit on screen, so scrollback history exists.
        let mut fill = String::new();
        for i in 0..60 {
            fill.push_str(&format!("line{i:02}\r\n"));
        }
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(fill.as_bytes());
        app.toggle_select();
        assert!(app.select.is_some(), "select mode entered");
        // Jump to the top row (gg).
        app.handle_key(key(Key::Char('g'), Mods::empty()));
        app.handle_key(key(Key::Char('g'), Mods::empty()));
        assert_eq!(app.select.as_ref().unwrap().cursor().0, 0);
        let before = app.ws().pane(fid).expect("pane").screen().scrollback();
        assert_eq!(before, 0);
        // `k` at the top edge scrolls one line into history.
        app.handle_key(key(Key::Char('k'), Mods::empty()));
        let after = app.ws().pane(fid).expect("pane").screen().scrollback();
        assert_eq!(after, before + 1, "k at top scrolls back");
        assert!(app.select.is_some(), "still in select mode");
        assert_eq!(
            app.select.as_ref().unwrap().cursor().0,
            0,
            "cursor stays on the top row over new content"
        );
        // Jump to the bottom row (ge), then `j` scrolls back toward live.
        app.handle_key(key(Key::Char('g'), Mods::empty()));
        app.handle_key(key(Key::Char('e'), Mods::empty()));
        let rows = app.select.as_ref().unwrap().cursor().0 + 1;
        assert!(rows > 1);
        app.handle_key(key(Key::Char('j'), Mods::empty()));
        let back = app.ws().pane(fid).expect("pane").screen().scrollback();
        assert_eq!(back, after - 1, "j at bottom scrolls forward");
        assert!(app.select.is_some(), "still in select mode");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// ctrl+o opens the inbox: menu flag set and the dropdown group active.
    #[test]
    fn ctrl_o_opens_menu_dropdown() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("menu");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        assert!(!app.menu_open);
        app.handle_key(key(Key::Char('o'), Mods::CONTROL));
        assert!(app.menu_open, "ctrl+o opens the menu");
        assert!(app.menu.is_active(), "dropdown group is highlighted/opened");
        // Navigate and select a workspace entry: menu closes again.
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(!app.menu_open);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Opening the inbox highlights the current workspace's row (not always
    /// the first row), so Enter keeps you where you are.
    #[test]
    fn inbox_opens_on_current_workspace() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("menucur");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.new_workspace(); // ws-1
        app.new_workspace(); // ws-2, current
        assert_eq!(app.current, 2);
        app.handle_key(key(Key::Char('o'), Mods::CONTROL));
        assert!(app.menu_open);
        assert!(
            matches!(
                app.menu.highlight().and_then(|i| i.data.clone()),
                Some(WsAction::Switch(2))
            ),
            "highlight starts on the current workspace"
        );
        // Enter selects it: still on ws-2, menu closes.
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(!app.menu_open);
        assert_eq!(app.current, 2);
        assert_eq!(app.ws().name, "ws-2");
        // Same when opening from the middle.
        app.switch_to(0);
        app.handle_key(key(Key::Char('o'), Mods::CONTROL));
        assert!(
            matches!(
                app.menu.highlight().and_then(|i| i.data.clone()),
                Some(WsAction::Switch(0))
            ),
            "highlight follows a switch"
        );
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(!app.menu_open);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The top menu is populated even before the inbox is opened, so the bar
    /// shows `[Workspaces]` from startup.
    #[test]
    fn menu_is_populated_without_opening() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("menubar");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.sync_menu();
        // Render the bar into a buffer and check the group labels appear.
        let area = ratatui::layout::Rect::new(0, 0, 60, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::StatefulWidget::render(
            tui_menu::Menu::new(),
            area,
            &mut buf,
            app.menu_mut(),
        );
        let mut text = String::new();
        for x in 0..area.width {
            text.push_str(buf[(x, 0)].symbol());
        }
        assert!(
            text.contains("Workspaces") && text.contains("Actions") && text.contains("Commands"),
            "bar text: {text:?}"
        );
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The top-bar "Commands" menu opens the database picker and the save form.
    #[test]
    fn menu_commands_open_overlays() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("menucmd");
        let (cfg, dbpath) = config_with_temp_db("menucmd");
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.open_inbox();
        assert!(app.menu_open, "inbox opened");

        app.apply_menu_action(WsAction::SavedCommands);
        assert!(
            app.command_picker_ref().is_some(),
            "picker opened from menu"
        );
        app.command_picker = None;

        app.apply_menu_action(WsAction::NewCommand);
        assert!(app.command_form_mut().is_some(), "form opened from menu");
        app.command_form = None;

        app.apply_menu_action(WsAction::ImportCheat);
        assert!(
            matches!(app.prompt().map(|p| p.kind), Some(PromptKind::CheatSheet)),
            "import opens the cheat.sh topic prompt"
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// J/K (and shift+up/down) in the inbox reorder workspaces: the order
    /// changes, `current` follows its workspace, the menu stays open on the
    /// moved row, and the new order hits the layout file.
    #[test]
    fn inbox_move_reorders_workspaces() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("movews");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.new_workspace(); // ws-1
        app.new_workspace(); // ws-2
        let names = |a: &App| {
            a.workspaces
                .iter()
                .map(|w| w.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&app), ["main", "ws-1", "ws-2"]);
        app.switch_to(0);
        app.open_inbox();
        assert!(app.menu_open);

        // Row 0 (main) moves down with J; current follows it.
        app.handle_key(key(Key::Char('J'), Mods::empty()));
        assert!(app.menu_open, "menu stays open after a move");
        assert_eq!(names(&app), ["ws-1", "main", "ws-2"]);
        assert_eq!(app.current, 1);
        assert_eq!(app.workspaces[app.current].name, "main");
        assert!(
            matches!(
                app.menu.highlight().and_then(|i| i.data.clone()),
                Some(WsAction::Switch(1))
            ),
            "highlight follows the moved row"
        );
        // K moves it back up.
        app.handle_key(key(Key::Char('K'), Mods::empty()));
        assert_eq!(names(&app), ["main", "ws-1", "ws-2"]);
        assert_eq!(app.current, 0);
        // Clamp: K at the top is a no-op (menu still open, order kept).
        app.handle_key(key(Key::Char('K'), Mods::empty()));
        assert_eq!(names(&app), ["main", "ws-1", "ws-2"]);
        assert!(app.menu_open);
        // Shift+Down moves too.
        app.handle_key(key(Key::Down, Mods::SHIFT));
        assert_eq!(names(&app), ["ws-1", "main", "ws-2"]);
        // Moving another row across `current` keeps current on its workspace.
        app.handle_key(key(Key::Char('J'), Mods::empty()));
        assert_eq!(names(&app), ["ws-1", "ws-2", "main"]);
        assert_eq!(app.workspaces[app.current].name, "main");
        assert_eq!(app.current, 2);
        // Persistence is explicit: nothing is written until "Save layout".
        assert!(!layout.is_file(), "reorder must not auto-save");
        app.run_command(Command::SaveLayout);
        let text = std::fs::read_to_string(&layout).expect("layout saved");
        let file: crate::layout::LayoutFile = toml::from_str(&text).expect("layout parses");
        let saved: Vec<String> = file.workspaces.iter().map(|w| w.name.clone()).collect();
        assert_eq!(saved, ["ws-1", "ws-2", "main"]);

        // Plain j/k still navigate instead of moving.
        app.handle_key(key(Key::Char('j'), Mods::empty()));
        assert_eq!(names(&app), ["ws-1", "ws-2", "main"]);
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(!app.menu_open);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// `move_workspace` tracks `current` through removals/insertions.
    #[test]
    fn move_workspace_keeps_current() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("movecur");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.new_workspace(); // ws-1
        app.new_workspace(); // ws-2
        app.new_workspace(); // ws-3 -> [main, ws-1, ws-2, ws-3]
        let names = |a: &App| {
            a.workspaces
                .iter()
                .map(|w| w.name.clone())
                .collect::<Vec<_>>()
        };
        // Move first to last while current sits in the middle.
        app.switch_to(1); // ws-1
        app.move_workspace(0, 3);
        assert_eq!(names(&app), ["ws-1", "ws-2", "ws-3", "main"]);
        assert_eq!(app.workspaces[app.current].name, "ws-1");
        // Move last to first while current is last.
        app.switch_to(3); // main
        app.move_workspace(3, 0);
        assert_eq!(names(&app), ["main", "ws-1", "ws-2", "ws-3"]);
        assert_eq!(app.workspaces[app.current].name, "main");
        // No-ops and out-of-range moves change nothing.
        app.move_workspace(1, 1);
        app.move_workspace(0, 9);
        app.move_workspace(9, 0);
        assert_eq!(names(&app), ["main", "ws-1", "ws-2", "ws-3"]);
        assert_eq!(app.workspaces[app.current].name, "main");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// ctrl+p opens the palette and Enter runs the highlighted command.
    #[test]
    fn palette_runs_selected_command() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("palette");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.handle_key(key(Key::Char('p'), Mods::CONTROL));
        assert!(app.palette.is_some(), "ctrl+p opens the palette");
        // First entry is "Split horizontally".
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.palette.is_none());
        assert_eq!(app.ws().leaf_ids().len(), 2, "command executed");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The palette must offer the terminal-sharing commands (regression: they
    /// were added to `Command::ALL` and must be reachable via ctrl+p).
    #[test]
    fn palette_offers_share_commands() {
        let mut p = Palette::new();
        let labels: Vec<&str> = p.results.iter().map(|i| i.label).collect();
        assert!(
            labels.iter().any(|l| l.contains("Share terminal")),
            "palette missing Share terminal: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("Stop sharing")),
            "palette missing Stop sharing: {labels:?}"
        );
        // Fuzzy search for "share" finds it.
        for c in "share".chars() {
            p.push_char(c);
        }
        assert!(
            p.results.iter().any(|i| i.label.contains("Share terminal")),
            "typing 'share' does not match: {:?}",
            p.results.iter().map(|i| i.label).collect::<Vec<_>>()
        );
    }

    /// Typing fuzzy-filters the palette; the query is editable.
    #[test]
    fn palette_fuzzy_filters_by_typing() {
        let mut p = Palette::new();
        let all = p.results.len();
        assert!(all > 5);
        for c in "spl".chars() {
            p.push_char(c);
        }
        assert_eq!(p.query, "spl");
        assert!(p.results.len() < all, "query narrows the list");
        // Contiguous matches rank first (fuzzy is a subsequence, so e.g.
        // "Focus pane left" also matches s..p..l).
        assert!(
            p.results[0].label.contains("Split") && p.results[1].label.contains("Split"),
            "best matches first: {:?}",
            p.results.iter().map(|i| i.label).collect::<Vec<_>>()
        );
        // Typing more is case-insensitive and non-contiguous (fuzzy).
        p.backspace();
        p.backspace();
        p.backspace();
        assert_eq!(p.query, "");
        assert_eq!(p.results.len(), all, "clearing restores everything");
        for c in "pt".chars() {
            p.push_char(c);
        }
        assert!(
            p.results.iter().any(|i| i.label.contains("Split")),
            "fuzzy subsequence matches Split"
        );
    }

    /// The palette's SELECTED command runs on Enter.
    #[test]
    fn palette_enter_runs_matched_command() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("palette-run");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.handle_key(key(Key::Char('p'), Mods::CONTROL));
        for c in "quit".chars() {
            app.handle_key(key(Key::Char(c), Mods::empty()));
        }
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.should_quit, "typing 'quit' then enter quits");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Running "Save layout" from the palette writes the file straight away.
    #[test]
    fn palette_save_layout_writes_immediately() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("palette-savelayout");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        assert!(!layout.is_file(), "nothing written before the command");
        app.handle_key(key(Key::Char('p'), Mods::CONTROL)); // command palette
        for c in "save layout".chars() {
            app.handle_key(key(Key::Char(c), Mods::empty()));
        }
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(
            layout.is_file() && app.status.starts_with("layout saved"),
            "status: {}",
            app.status
        );
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Hot reload: valid edits apply; broken edits keep the old config and
    /// surface the parser's line/column in the status line.
    #[test]
    fn config_reload_applies_or_reports_error() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("reload");
        let cfg_path = unique_temp_path("cfg");
        std::fs::write(&cfg_path, "[general]\nscrollback = 1234\n").unwrap();
        let cfg = Config::load(Some(&cfg_path));
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("boots");
        assert_eq!(app.config.general.scrollback, 1234);

        // Valid change applies.
        std::fs::write(&cfg_path, "[general]\nscrollback = 4321\n").unwrap();
        app.reload_config();
        assert_eq!(app.config.general.scrollback, 4321);
        assert!(app.status.contains("reloaded"));

        // Broken change is rejected; old value and an error are shown.
        std::fs::write(&cfg_path, "[general]\nscrollback = ???\n").unwrap();
        app.reload_config();
        assert_eq!(app.config.general.scrollback, 4321, "kept old config");
        assert!(
            app.status.contains("config error"),
            "status: {}",
            app.status
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        let _ = std::fs::remove_file(&cfg_path);
        rt.shutdown_background();
    }

    #[test]
    fn select_mode_enters_and_yanks() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("select");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");

        let mut mods = Mods::empty();
        mods.insert(Mods::CONTROL);
        mods.insert(Mods::SHIFT);
        app.handle_key(KeyPress {
            key: Key::Space,
            mods,
            text: None,
            kind: KeyKind::Press,
        });
        assert!(app.select.is_some(), "ctrl+shift+space enters select mode");

        // Home then End stay on the same row.
        app.handle_key(key(Key::Home, Mods::empty()));
        assert_eq!(app.select.as_ref().unwrap().cursor().1, 0);
        app.handle_key(key(Key::End, Mods::empty()));
        let row = app.select.as_ref().unwrap().cursor().0;
        assert_eq!(app.select.as_ref().unwrap().cursor().1, 79);
        app.handle_key(key(Key::Home, Mods::empty()));
        assert_eq!(app.select.as_ref().unwrap().cursor().0, row);

        // Anchor with v, move down, yank with y -> mode exits.
        app.handle_key(key(Key::Char('v'), Mods::empty()));
        assert!(app.select.as_ref().unwrap().range().is_some());
        app.handle_key(key(Key::Char('j'), Mods::empty()));
        app.handle_key(key(Key::Char('y'), Mods::empty()));
        assert!(app.select.is_none(), "y leaves select mode");
        assert!(app.status.starts_with("yanked") || app.status.contains("yank"));

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Mouse routing: ignored without tracking, forwarded with it, gated by
    /// modal state and the `[mouse]` switch.
    #[test]
    fn mouse_button_move_wheel_routing() {
        use crate::mouse::MouseButton as MButton;

        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mouse");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 80, 24))]);
        // Fill scrollback so wheel scrolling has somewhere to go.
        let mut fill = String::new();
        for i in 0..30 {
            fill.push_str(&format!("line{i:02}\r\n"));
        }
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(fill.as_bytes());
        // Full-area (5,5) -> inner grid (4,4).
        let press = |app: &mut App| app.mouse_button(5, 5, MButton::Left, true, Mods::empty());

        // No tracking: clicks are ignored here (caller still focuses).
        assert_eq!(press(&mut app), MouseClickOutcome::Ignored);
        assert!(!app.mouse_move(5, 5, Mods::empty(), false));
        assert!(!app.mouse_move(5, 5, Mods::empty(), true));
        // Wheel without tracking scrolls back instead (returns false).
        assert!(!app.mouse_wheel(5, 5, 3, Mods::empty()));
        let sb = app.ws().pane(fid).expect("pane").screen().scrollback();
        assert_eq!(sb, 3);

        // App enables tracking: presses forward, releases too (1000 covers
        // both), motion stays off without 1003.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(press(&mut app), MouseClickOutcome::Forwarded);
        assert_eq!(
            app.mouse_button(5, 5, MButton::Left, false, Mods::empty()),
            MouseClickOutcome::Forwarded
        );
        assert!(!app.mouse_move(5, 5, Mods::empty(), true));
        // Wheel is forwarded now (returns true); writing to the pane
        // jumps back to the live screen, clearing the 3 above.
        assert!(app.mouse_wheel(5, 5, 3, Mods::empty()));
        let sb2 = app.ws().pane(fid).expect("pane").screen().scrollback();
        assert_eq!(sb2, 0, "write() returns to the live screen");

        // Any-motion mode reports free moves as well.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[?1003h");
        assert!(app.mouse_move(5, 5, Mods::empty(), false));

        // An open menu consumes pane clicks (dismiss) instead of
        // forwarding them; the `[mouse] enabled = false` switch gates
        // everything else back to Ignored.
        app.menu_open = true;
        assert_eq!(press(&mut app), MouseClickOutcome::Menu);
        assert!(!app.menu_open, "pane click dismisses the menu");
        app.config.mouse.enabled = false;
        assert_eq!(press(&mut app), MouseClickOutcome::Ignored);
        assert!(!app.mouse_move(5, 5, Mods::empty(), false));
        app.config.mouse.enabled = true;

        // Modifier+click on empty space falls through (no link, no browser).
        let mut ctrl = Mods::empty();
        ctrl.insert(Mods::CONTROL);
        assert_eq!(
            app.mouse_button(5, 5, MButton::Left, true, ctrl),
            MouseClickOutcome::Forwarded
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// MCP clicks are pane-relative: they map onto the inner grid (border
    /// inset) and forward only when the pane's app tracks the mouse.
    #[test]
    fn mcp_mouse_click_translates_pane_coords() {
        use crate::mcp::McpReply;
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mcpclick");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 1, 80, 24))]);

        // Without tracking the click is rejected with a helpful message.
        match app.mcp_mouse_click(fid, 3, 2, crate::mouse::MouseButton::Left, None) {
            McpReply::Err(e) => assert!(e.contains("mouse tracking"), "err: {e}"),
            _ => panic!("expected rejection without tracking"),
        }

        // Out-of-grid coordinates are rejected before any hit-test.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        match app.mcp_mouse_click(fid, 200, 2, crate::mouse::MouseButton::Left, None) {
            McpReply::Err(e) => assert!(e.contains("outside the pane grid"), "err: {e}"),
            _ => panic!("expected out-of-grid rejection"),
        }

        // In-bounds: press+release forwarded, reported by cell.
        match app.mcp_mouse_click(fid, 3, 2, crate::mouse::MouseButton::Left, None) {
            McpReply::Text(t) => assert!(t.contains("(3, 2)"), "text: {t}"),
            _ => panic!("expected a click report"),
        }

        // Drag with a non-left button: press, motion along the line, release.
        match app.mcp_mouse_drag(fid, (1, 1), (5, 4), crate::mouse::MouseButton::Right, None) {
            McpReply::Text(t) => assert!(t.contains("dragged Right"), "drag: {t}"),
            _ => panic!("expected a drag report"),
        }

        // An out-of-grid drag end is rejected before any bytes go out.
        match app.mcp_mouse_drag(fid, (1, 1), (200, 4), crate::mouse::MouseButton::Left, None) {
            McpReply::Err(e) => assert!(e.contains("outside the pane grid"), "err: {e}"),
            _ => panic!("expected out-of-grid drag rejection"),
        }

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Holding the button no longer streams identical motion reports: a
    /// repeat move in the same cell is coalesced, a new cell reports, and a
    /// fresh button gesture resets the cache.
    #[test]
    fn mouse_motion_is_coalesced_per_cell() {
        use crate::mouse::MouseButton as MButton;
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mousecoalesce");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 80, 24))]);
        // Drag tracking (1002) + SGR.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[?1002h\x1b[?1006h");

        // First drag report at a cell is sent; an immediate repeat is not.
        assert!(
            app.mouse_move(5, 5, Mods::empty(), true),
            "first move reports"
        );
        assert!(
            !app.mouse_move(5, 5, Mods::empty(), true),
            "same cell is coalesced"
        );
        // A different cell reports again.
        assert!(
            app.mouse_move(6, 5, Mods::empty(), true),
            "new cell reports"
        );
        // Releasing and pressing again resets the cache, so the next move
        // at the very same cell reports.
        app.mouse_button(6, 5, MButton::Left, false, Mods::empty());
        app.mouse_button(6, 5, MButton::Left, true, Mods::empty());
        assert!(
            app.mouse_move(6, 5, Mods::empty(), true),
            "gesture reset re-reports"
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// `url_at` finds a fed link at inner-grid coords without opening
    /// anything (opening a real browser is never exercised in tests).
    #[test]
    fn mouse_url_at_finds_fed_link() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mouseurl");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 80, 24))]);
        // Parser untouched by the live shell so far (no polls): home the
        // cursor and write one link row deterministically.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[Hhttps://example.com/x");
        // Full-area (1,1) is inner grid (0,0): the link starts there.
        assert_eq!(app.url_at(1, 1), Some("https://example.com/x".to_string()));
        assert_eq!(app.url_at(10, 1), Some("https://example.com/x".to_string()));
        assert_eq!(app.url_at(1, 5), None);
        // Default url_mod is ctrl; alt also parses via config.
        let mut alt = Mods::empty();
        alt.insert(Mods::ALT);
        assert_eq!(app.url_mods(), Mods::CONTROL);
        app.config.mouse.url_mod = "alt".to_string();
        assert_eq!(app.url_mods(), Mods::ALT);
        app.config.mouse.url_mod = "bogus".to_string();
        assert_eq!(app.url_mods(), Mods::CONTROL);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The keymap routes config bindings to commands (single source of truth).
    #[test]
    fn keymap_routes_bindings_to_commands() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("keymap");
        let app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let ctrl = |c: char| KeyPress {
            key: Key::Char(c),
            mods: Mods::CONTROL,
            text: None,
            kind: KeyKind::Press,
        };
        assert!(matches!(
            app.lookup_command(&ctrl('o')),
            Some(Command::Inbox)
        ));
        assert!(matches!(
            app.lookup_command(&ctrl('p')),
            Some(Command::CommandPalette)
        ));
        assert!(matches!(
            app.lookup_command(&ctrl('e')),
            Some(Command::EditConfig)
        ));
        assert!(matches!(
            app.lookup_command(&ctrl('y')),
            Some(Command::YankLast)
        ));
        // Unbound key -> no command (falls through to the terminal).
        assert!(app.lookup_command(&ctrl('z')).is_none());
        let mut app = app;
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The state pattern reports the right modal owner.
    #[test]
    fn mode_reflects_active_overlay() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mode");
        let mut app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        assert_eq!(app.mode(), Mode::Normal);
        app.open_palette();
        assert_eq!(app.mode(), Mode::Palette);
        app.palette = None;
        app.prompt = Some(Prompt::open(PromptKind::AskAi));
        assert_eq!(app.mode(), Mode::Prompt);
        app.prompt = None;
        app.toggle_select();
        assert_eq!(app.mode(), Mode::Select);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Window title reflects the focused pane and workspace.
    #[test]
    fn window_title_names_focus() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("title");
        let app = App::new(
            Config::default(),
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let title = app.window_title();
        assert!(title.starts_with("termrs - "), "title: {title}");
        assert!(title.contains("main"), "title: {title}");
        let mut app = app;
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Config whose command database is a throwaway temp file.
    fn config_with_temp_db(tag: &str) -> (Config, std::path::PathBuf) {
        let db = unique_temp_path(tag).with_extension("db");
        let mut cfg = Config::default();
        cfg.commands.db = db.to_string_lossy().into_owned();
        (cfg, db)
    }

    fn remove_db(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    /// Type text into whichever overlay owns the keyboard.
    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            let k = if c == ' ' { Key::Space } else { Key::Char(c) };
            app.handle_key(key(k, Mods::empty()));
        }
    }

    /// Picking a saved command without placeholders types it and runs it.
    #[test]
    fn command_picker_runs_saved_command() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdpick");
        let (cfg, dbpath) = config_with_temp_db("cmdpick");
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.db
            .as_ref()
            .unwrap()
            .add("echo hi", "greet", "")
            .unwrap();

        app.handle_key(key(Key::Char('r'), Mods::CONTROL));
        assert!(app.command_picker_ref().is_some(), "ctrl+r opens picker");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_args.is_none());
        assert!(
            app.status.starts_with("run: echo hi"),
            "status: {}",
            app.status
        );
        assert_eq!(app.db.as_ref().unwrap().all().unwrap()[0].uses, 1);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// A command with `{name}` placeholders opens a form and fills them in,
    /// quoting values that contain spaces.
    #[test]
    fn placeholder_command_prompts_then_fills() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdargs");
        let (cfg, dbpath) = config_with_temp_db("cmdargs");
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.db
            .as_ref()
            .unwrap()
            .add("cp {src} {dst}", "copy", "")
            .unwrap();
        app.open_commands();
        app.handle_key(key(Key::Enter, Mods::empty()));
        {
            let args = app.command_args.as_ref().expect("placeholder form");
            assert_eq!(args.names, vec!["src", "dst"]);
        }
        type_text(&mut app, "a b.txt");
        app.handle_key(key(Key::Tab, Mods::empty()));
        type_text(&mut app, "out");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_args.is_none());
        assert!(app.status.contains("\"a b.txt\""), "status: {}", app.status);
        assert!(app.status.starts_with("run: cp"), "status: {}", app.status);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// The save form (ctrl+shift+r) persists command + comment.
    #[test]
    fn command_save_form_persists() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdsave");
        let (cfg, dbpath) = config_with_temp_db("cmdsave");
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");

        app.handle_key(key(Key::Char('r'), Mods::CONTROL | Mods::SHIFT));
        assert!(app.command_form_mut().is_some(), "ctrl+shift+r opens form");
        assert!(app.command_picker_ref().is_none(), "not the picker");
        type_text(&mut app, "git log --oneline -n {count}");
        app.handle_key(key(Key::Tab, Mods::empty()));
        type_text(&mut app, "recent commits");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_form_mut().is_none(), "form closed on save");

        let all = app.db.as_ref().unwrap().all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].command, "git log --oneline -n {count}");
        assert_eq!(all[0].comment, "recent commits");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Ctrl+e in the picker opens the edit table for the highlighted row;
    /// Enter updates it in place (tags and use counter kept) and Esc
    /// returns to the picker.
    #[test]
    fn command_picker_ctrl_e_edits_selected() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdedit");
        let (cfg, dbpath) = config_with_temp_db("cmdedit");
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        let id = app
            .db
            .as_ref()
            .unwrap()
            .add("git log", "recent commits", "git")
            .unwrap();
        app.db.as_ref().unwrap().mark_used(id).unwrap();

        // Esc from the edit table returns to the picker.
        app.handle_key(key(Key::Char('r'), Mods::CONTROL));
        assert!(app.command_picker_ref().is_some(), "picker opens");
        app.handle_key(key(Key::Char('e'), Mods::CONTROL));
        assert!(app.command_picker_ref().is_none(), "picker closed");
        let form = app.command_form_mut().expect("edit table opens");
        assert_eq!(form.edit_id, Some(id));
        assert_eq!(form.command_text(), "git log");
        assert_eq!(form.comment_text(), "recent commits");
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(app.command_form_mut().is_none(), "form closed on esc");
        assert!(app.command_picker_ref().is_some(), "esc returns to picker");

        // Re-open, edit the command, Enter updates it in place.
        app.handle_key(key(Key::Char('e'), Mods::CONTROL));
        type_text(&mut app, " --oneline");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_form_mut().is_none(), "form closed on save");
        let row = &app.db.as_ref().unwrap().all().unwrap()[0];
        assert_eq!(row.command, "git log --oneline");
        assert_eq!(row.comment, "recent commits");
        assert_eq!(row.tags, "git", "tags kept");
        assert_eq!(row.uses, 1, "use counter kept");
        assert!(
            app.status.contains("updated command"),
            "status: {}",
            app.status
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Ctrl+e with a non-empty filter types `e` into the query instead of
    /// opening the edit table (fuzzy search keeps working).
    #[test]
    fn command_picker_plain_e_types_into_filter() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdetype");
        let (cfg, dbpath) = config_with_temp_db("cmdetype");
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.db
            .as_ref()
            .unwrap()
            .add("git log", "commits", "")
            .unwrap();

        app.handle_key(key(Key::Char('r'), Mods::CONTROL));
        type_text(&mut app, "e");
        assert!(app.command_picker_ref().is_some(), "picker stays open");
        assert!(app.command_form_mut().is_none(), "no edit table");
        assert_eq!(app.command_picker_ref().expect("picker").query, "e");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// A finished cheat.sh import opens the tick-box picker with fresh rows
    /// unticked (dupes skipped); space ticks, Enter adds tagged rows.
    #[test]
    fn cheat_import_opens_picker_and_adds_ticked() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cheatimport");
        let (cfg, dbpath) = config_with_temp_db("cheatimport");
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.db
            .as_ref()
            .unwrap()
            .add("tar -czvf a.tgz dir", "old", "")
            .unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(Ok(CheatImport {
            topic: "tar".into(),
            entries: vec![
                crate::cheatsheet::CheatEntry {
                    command: "tar -xvf {archive}".into(),
                    comment: "extract".into(),
                },
                crate::cheatsheet::CheatEntry {
                    command: "tar -czvf a.tgz dir".into(),
                    comment: "create".into(),
                },
            ],
        }))
        .unwrap();
        app.cheat_rx = Some(rx);
        app.poll_cheat();
        assert!(app.cheat_rx.is_none(), "channel consumed");
        assert_eq!(
            app.db.as_ref().unwrap().all().unwrap().len(),
            1,
            "nothing saved yet"
        );
        let picker = app.cheat_picker_ref().expect("review picker opens");
        assert_eq!(picker.results.len(), 1, "dupe filtered");
        assert_eq!(picker.checked_count(), 0, "starts unticked");
        assert!(
            app.status.contains("already saved"),
            "status: {}",
            app.status
        );

        // Empty Enter keeps it open; space ticks, Enter adds.
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(
            app.cheat_picker_ref().is_some(),
            "empty submit keeps picker"
        );
        app.handle_key(key(Key::Space, Mods::empty()));
        assert_eq!(app.cheat_picker_ref().expect("picker").checked_count(), 1);
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.cheat_picker_ref().is_none(), "submit closes picker");
        let all = app.db.as_ref().unwrap().all().unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            all.iter().any(|c| c.tags == "cheat.sh:tar"),
            "tagged: {all:?}"
        );
        assert!(
            app.status.contains("added 1 commands"),
            "status: {}",
            app.status
        );
        assert!(
            app.command_picker_ref().is_some(),
            "command picker opens after"
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Ctrl+e in the import picker opens the edit table for the highlighted
    /// row; Enter rewrites the pending row (nothing saved), Esc returns.
    #[test]
    fn cheat_picker_ctrl_e_edits_row() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cheatedit");
        let (cfg, dbpath) = config_with_temp_db("cheatedit");
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.cheat_picker = Some(CheatPicker::new(
            "tar".into(),
            vec![
                crate::cheatsheet::CheatEntry {
                    command: "tar -xvf {archive}".into(),
                    comment: "extract".into(),
                },
                crate::cheatsheet::CheatEntry {
                    command: "tar -tzvf {archive}".into(),
                    comment: "list".into(),
                },
            ],
        ));

        // Ctrl+e on the first row opens the edit table.
        app.handle_key(key(Key::Char('e'), Mods::CONTROL));
        let form = app.command_form_mut().expect("edit table opens");
        assert_eq!(form.edit_cheat, Some(0));
        assert_eq!(form.origin, FormOrigin::CheatPicker);
        assert_eq!(form.command_text(), "tar -xvf {archive}");
        assert_eq!(form.comment_text(), "extract");

        // Modify command + comment (cursor starts at end), Enter rewrites
        // the pending row.
        type_text(&mut app, " extra");
        app.handle_key(key(Key::Tab, Mods::empty()));
        assert_eq!(app.command_form_mut().expect("form").field, 1);
        type_text(&mut app, " done");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_form_mut().is_none(), "form closed");
        let picker = app.cheat_picker_ref().expect("import picker still open");
        assert_eq!(picker.items[0].entry.command, "tar -xvf {archive} extra");
        assert_eq!(picker.items[0].entry.comment, "extract done");
        // Nothing reached the database.
        assert!(app.db.as_ref().unwrap().all().unwrap().is_empty());
        assert!(
            app.status.contains("edited import row"),
            "status: {}",
            app.status
        );

        // Esc from the edit table returns to the import picker.
        app.handle_key(key(Key::Char('e'), Mods::CONTROL));
        assert!(app.command_form_mut().is_some());
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(app.command_form_mut().is_none());
        assert!(
            app.cheat_picker_ref().is_some(),
            "esc returns to import picker"
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Select-all / clear-all / Esc in the import review picker.
    #[test]
    fn cheat_picker_select_all_clear_and_cancel() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cheatpick");
        let (cfg, dbpath) = config_with_temp_db("cheatpick");
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.cheat_picker = Some(CheatPicker::new(
            "tar".into(),
            vec![
                crate::cheatsheet::CheatEntry {
                    command: "tar -xvf {a}".into(),
                    comment: String::new(),
                },
                crate::cheatsheet::CheatEntry {
                    command: "tar -tzvf {a}".into(),
                    comment: String::new(),
                },
            ],
        ));
        app.handle_key(key(Key::Char('a'), Mods::CONTROL));
        assert_eq!(app.cheat_picker_ref().expect("picker").checked_count(), 2);
        app.handle_key(key(Key::Char('u'), Mods::CONTROL));
        assert_eq!(app.cheat_picker_ref().expect("picker").checked_count(), 0);
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(app.cheat_picker_ref().is_none());
        assert!(app.db.as_ref().unwrap().all().unwrap().is_empty());
        assert!(app.status.contains("cancelled"), "status: {}", app.status);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Unique temp layout path per test invocation, so a panicking run can
    /// never pollute the next one with a stale file.
    fn unique_temp_path(tag: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "termrs-test-{tag}-{}-{nanos}.toml",
            std::process::id()
        ))
    }

    /// A pane-scoped MCP server driving the real App: open a server on the
    /// focused pane, then call a tool over HTTP while the UI thread answers
    /// pane queries (which only happens inside `poll_panes`).
    #[test]
    fn mcp_pane_serves_real_pane_queries() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mcp");
        let mut cfg = Config::default();
        cfg.general.mcp_port = 0; // ephemeral: no clash with a real server
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");

        app.open_mcp_pane(None);
        let pane_id = app.ws().focused;
        let port = app
            .mcp_servers
            .get(&pane_id)
            .expect("MCP server attached")
            .port;

        // Type a unique marker so the screen query must read real pane state.
        app.ws_mut()
            .pane_mut(pane_id)
            .expect("pane")
            .write(b"echo mcp-probe-123\r");
        let mut seen = false;
        for _ in 0..80 {
            app.poll_panes();
            let hit = app
                .ws()
                .pane(pane_id)
                .map(|p| p.screen().contents().contains("mcp-probe-123"))
                .unwrap_or(false);
            if hit {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(seen, "shell produced the probe line");

        // Drive MCP on a worker thread; the UI thread must keep polling to
        // answer the tool's pane query.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let url = format!("http://127.0.0.1:{port}/mcp");
            let post = |body: &str, session: Option<&str>| -> (String, Option<String>) {
                let mut req = ureq::post(&url)
                    .set("Content-Type", "application/json")
                    .set("Accept", "application/json, text/event-stream")
                    .set("MCP-Protocol-Version", "2025-11-25");
                if let Some(s) = session {
                    req = req.set("Mcp-Session-Id", s);
                }
                let resp = req.send_string(body).expect("response");
                let sid = resp.header("mcp-session-id").map(str::to_string);
                (resp.into_string().unwrap_or_default(), sid)
            };
            let init = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                }
            })
            .to_string();
            let (_, session) = post(&init, None);
            let session = session.expect("session id");
            let ready = serde_json::json!({
                "jsonrpc": "2.0", "method": "notifications/initialized"
            })
            .to_string();
            let _ = post(&ready, Some(&session));
            let call = serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "termrs_get_screen", "arguments": {} }
            })
            .to_string();
            let (out, _) = post(&call, Some(&session));
            let _ = done_tx.send(out);
        });

        let mut screen = None;
        for _ in 0..400 {
            app.poll_panes();
            if let Ok(v) = done_rx.try_recv() {
                screen = Some(v);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let screen = screen.expect("MCP get_screen completed");
        assert!(
            screen.contains("mcp-probe-123"),
            "MCP screen did not contain the probe: {screen}"
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Opening MCP in the current pane must target the current workspace's
    /// focused pane, not the first workspace's same-numbered pane (pane ids
    /// used to collide as `0` in every workspace).
    #[test]
    fn mcp_open_here_targets_current_workspace() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mcp-ws");
        let mut cfg = Config::default();
        cfg.general.mcp_port = 0;
        let mut app = App::new(
            cfg,
            Some(layout.clone()),
            rt.handle(),
            &wake,
            None,
            None,
            false,
        )
        .expect("app boots");
        app.new_workspace();
        assert_eq!(app.workspaces.len(), 2);
        // Every live pane id must be unique across workspaces.
        let mut all: Vec<usize> = app
            .workspaces
            .iter()
            .flat_map(|w| w.leaf_ids())
            .collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(
            all.len(),
            app.workspaces.iter().map(|w| w.leaf_ids().len()).sum::<usize>(),
            "pane ids collide across workspaces: {:?}",
            app.workspaces
                .iter()
                .map(|w| w.leaf_ids())
                .collect::<Vec<_>>()
        );

        // Open MCP in the current (second) workspace's focused pane.
        let current = app.current;
        let focused = app.ws().focused;
        assert!(
            app.workspaces[current].leaf_ids().contains(&focused),
            "focused pane must belong to current workspace"
        );
        app.open_mcp_pane(None);
        assert!(
            app.mcp_servers.contains_key(&focused),
            "MCP server must be keyed by the current pane"
        );
        // The server's pane must resolve to the current workspace, and the
        // status line must land on that pane (not workspace 0's pane).
        let pane = app.find_pane(focused).expect("pane resolves");
        let ws_idx = app
            .workspaces
            .iter()
            .position(|w| w.pane(focused).is_some())
            .expect("workspace resolves");
        assert_eq!(ws_idx, current, "MCP pane resolved to wrong workspace");
        assert!(
            pane.mcp_status.is_some(),
            "MCP status must be set on the current pane"
        );
        assert!(
            app.workspaces[0].pane(focused).is_none() || current == 0,
            "first workspace must not own the MCP pane"
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

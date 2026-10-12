    use super::*;

    #[test]
    fn defaults_parse() {
        let cfg: Config = toml::from_str(&Config::default_toml()).unwrap();
        assert_eq!(cfg.general.scrollback, 2000);
        assert_eq!(cfg.keys.quit, vec!["ctrl+q".to_string()]);
        assert_eq!(cfg.window.opacity, 1.0);
        assert!(cfg.window.decorations);
        assert!(cfg.window.background_image.is_empty());
        assert!(cfg.fx.enabled);
        assert_eq!(cfg.fx.resolve(FxKind::Fresh), Some(("dissolve".into(), 450)));
        assert_eq!(cfg.fx.resolve(FxKind::Close), Some(("dissolve".into(), 350)));
        assert_eq!(cfg.fx.resolve(FxKind::Focus), Some(("flash".into(), 300)));
        assert_eq!(cfg.ai.timeout_s, 60);
        assert_eq!(cfg.keys.pane_zoom, vec!["ctrl+shift+z".to_string()]);
        assert!(cfg.mouse.enabled);
        assert_eq!(cfg.mouse.url_mod, "ctrl");
    }

    /// A custom `[mouse]` section parses; unknown url mods are tolerated
    /// here (the app falls back to ctrl when interpreting them).
    #[test]
    fn mouse_section_parses() {
        let cfg: Config = toml::from_str("[mouse]\nenabled = false\nurl_mod = \"alt\"\n").unwrap();
        assert!(!cfg.mouse.enabled);
        assert_eq!(cfg.mouse.url_mod, "alt");
        // Absent section keeps defaults.
        let cfg: Config = toml::from_str("[keys]\nquit = \"alt+q\"\n").unwrap();
        assert!(cfg.mouse.enabled);
        assert_eq!(cfg.mouse.url_mod, "ctrl");
    }

    #[test]
    fn share_section_parses() {
        let cfg: Config = toml::from_str(
            "[share]\npage_url = \"https://example.com/termrs\"\nallow_control = false\ncode_len = 10\n",
        )
        .unwrap();
        assert_eq!(cfg.share.page_url, "https://example.com/termrs");
        assert!(!cfg.share.allow_control);
        assert_eq!(cfg.share.code_len, 10);
        // Absent section keeps defaults.
        let cfg: Config = toml::from_str("[keys]\nquit = \"alt+q\"\n").unwrap();
        assert_eq!(cfg.share.page_url, "https://Owlyat.github.io/Termrs/");
        assert!(cfg.share.allow_control);
        assert_eq!(cfg.share.code_len, 6);
        // A `[share]` section without page_url also gets the default URL.
        let cfg: Config = toml::from_str("[share]\nallow_control = true\n").unwrap();
        assert_eq!(cfg.share.page_url, "https://Owlyat.github.io/Termrs/");
    }

    /// The `.config` home is the canonical config location and the first
    /// search candidate; bootstrapping writes parseable defaults there.
    #[test]
    fn config_home_is_dotconfig_first() {
        let home = Config::home_dir().expect("home dir known");
        assert!(
            home.ends_with(".config/termrs") || home.ends_with(".config\\termrs"),
            "home: {}",
            home.display()
        );
        let def = Config::default_path().expect("default path known");
        assert_eq!(def, home.join("config.toml"));
        let cands = Config::candidates();
        assert_eq!(cands.first(), Some(&def), "candidates: {cands:?}");

        // Bootstrap into an isolated temp dir (never the real home).
        let dir = std::env::temp_dir().join(format!(
            "termrs-cfg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("termrs").join("config.toml");
        let written = Config::bootstrap_default_at(&path).expect("bootstrap");
        assert_eq!(written, path);
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).expect("bootstrapped config parses");
        assert_eq!(cfg.ai.timeout_s, 60);
        // Second call keeps the existing file (does not overwrite edits).
        std::fs::write(&path, "[general]\nscrollback = 1234\n").unwrap();
        assert_eq!(Config::bootstrap_default_at(&path), Some(path.clone()));
        assert!(std::fs::read_to_string(&path).unwrap().contains("1234"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_accept_string_or_list() {
        let single: Config = toml::from_str("[keys]\nquit = \"alt+q\"\n").unwrap();
        assert_eq!(single.keys.quit, vec!["alt+q".to_string()]);
        let multi: Config =
            toml::from_str("[keys]\nquit = [\"ctrl+q\", \"alt+q\"]\n").unwrap();
        assert_eq!(
            multi.keys.quit,
            vec!["ctrl+q".to_string(), "alt+q".to_string()]
        );
    }

    #[test]
    fn theme_and_macros_parse() {
        let cfg: Config = toml::from_str(
            "[theme]\nforeground = \"#6d7655\"\nbackground = \"#1f1f1f\"\ncursor = \"#c3cea0\"\naccent = \"#c3cea0\"\n\
             [[macros]]\nkeys = [\"ctrl+B\"]\nsend = \"wsl w3m\"\nenter = true\n",
        )
        .unwrap();
        let colors = cfg.theme.resolve();
        assert_eq!(colors.fg, [0x6d, 0x76, 0x55]);
        assert_eq!(colors.bg, [0x1f, 0x1f, 0x1f]);
        assert_eq!(colors.cursor, Some([0xc3, 0xce, 0xa0]));
        assert_eq!(colors.accent, Some([0xc3, 0xce, 0xa0]));
        assert_eq!(colors.cursor_shape, CursorShape::Bar);
        assert_eq!(cfg.macros.len(), 1);
        assert_eq!(cfg.macros[0].keys, vec!["ctrl+B".to_string()]);
        assert_eq!(cfg.macros[0].send, "wsl w3m");
        assert!(cfg.macros[0].enter);
    }

    #[test]
    fn cursor_shape_parses() {
        for (text, want) in [
            ("bar", CursorShape::Bar),
            ("underline", CursorShape::Underline),
            ("block", CursorShape::Block),
            ("weird", CursorShape::Bar),
        ] {
            let cfg: Config = toml::from_str(&format!("[theme]\ncursor_shape = \"{text}\"\n")).unwrap();
            assert_eq!(cfg.theme.resolve().cursor_shape, want, "shape {text}");
        }
    }

    #[test]
    fn theme_bad_hex_falls_back() {
        let cfg: Config = toml::from_str("[theme]\nforeground = \"nope\"\n").unwrap();
        let colors = cfg.theme.resolve();
        assert_eq!(colors.fg, [0xdc, 0xdc, 0xdc]);
        assert_eq!(colors.cursor, None);
    }

    #[test]
    fn fx_resolve_honours_switch_and_values() {
        let off = FxConfig {
            enabled: false,
            ..FxConfig::default()
        };
        assert_eq!(off.resolve(FxKind::Fresh), None);
        let unknown = FxConfig {
            open: "spin".into(),
            ..FxConfig::default()
        };
        assert_eq!(unknown.resolve(FxKind::Fresh), None);
        let zero = FxConfig {
            close_ms: 0,
            ..FxConfig::default()
        };
        assert_eq!(zero.resolve(FxKind::Close), None);
        let big = FxConfig {
            focus_ms: 99999,
            ..FxConfig::default()
        };
        assert_eq!(big.resolve(FxKind::Focus), Some(("flash".into(), 2000)));
    }

    #[test]
    fn window_section_parses_and_clamps() {
        let cfg: Config =
            toml::from_str("[window]\nbackground_image = \"bg.png\"\nopacity = 0.4\ndecorations = false\n")
                .unwrap();
        assert_eq!(cfg.window.background_image, "bg.png");
        assert_eq!(cfg.window.clamped_opacity(), 0.4);
        assert!(!cfg.window.decorations);
        let odd = Window {
            opacity: 9.0,
            ..Window::default()
        };
        assert_eq!(odd.clamped_opacity(), 1.0);
    }

    #[test]
    fn partial_file_keeps_defaults() {
        let cfg: Config = toml::from_str("[keys]\nquit = \"alt+q\"\n").unwrap();
        assert_eq!(cfg.keys.quit, vec!["alt+q".to_string()]);
        assert_eq!(cfg.keys.split_vertical, vec!["ctrl+v".to_string()]);
    }

    /// Configs compare by value: the window host uses this to notice
    /// hot-reloaded `[window]` edits (backdrop, decorations, transparency).
    #[test]
    fn config_equality_detects_edits() {
        let a = Config::default();
        assert_eq!(a, a.clone());
        let mut b = Config::default();
        b.window.background_image = "bg.png".into();
        assert_ne!(a, b);
        b.window.background_image.clear();
        assert_eq!(a, b);
        b.window.transparent = !b.window.transparent;
        assert_ne!(a, b);
    }

    /// The `[general]` key writer replaces in place, inserts after the
    /// header, or appends a section -- comments and siblings survive.
    #[test]
    fn set_general_key_edits_surgically() {
        // Replace existing, keep comments and other keys.
        let text = "# top\n[general]\n# keep me\nfont = \"old.ttf\"\nscrollback = 5\n[keys]\nquit = \"q\"\n";
        let out = set_general_key(text, "font", "\"new.ttf\"");
        assert!(out.contains("font = \"new.ttf\""), "out: {out}");
        assert!(!out.contains("old.ttf"), "out: {out}");
        assert!(out.contains("# keep me") && out.contains("scrollback = 5"));
        assert!(out.contains("[keys]"));
        // `fontx` is not `font`; commented lines are ignored.
        let tricky = "[general]\n# font = \"x\"\nfontx = 1\n";
        let out = set_general_key(tricky, "font", "\"n\"");
        assert!(out.contains("# font = \"x\"") && out.contains("fontx = 1"));
        assert!(out.contains("\nfont = \"n\""), "out: {out}");
        // Insert after the header when missing.
        let missing = "[general]\nscrollback = 5\n";
        assert_eq!(
            set_general_key(missing, "font", "\"n\""),
            "[general]\nfont = \"n\"\nscrollback = 5\n"
        );
        // Append a section when there is none; trailing newline preserved.
        assert_eq!(
            set_general_key("[keys]\nquit = \"q\"\n", "font", "\"n\""),
            "[keys]\nquit = \"q\"\n\n[general]\nfont = \"n\"\n"
        );
        assert_eq!(set_general_key("", "font", "\"n\""), "[general]\nfont = \"n\"");
    }

    /// `write_font_settings` round-trips through a real file: font + size
    /// written, file still parses, other content intact.
    #[test]
    fn write_font_settings_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "termrs-cfgfont-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# comment\n[general]\nfont_size = 18\n").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut cfg = Config::parse_with_source(&text, Some(path.clone())).unwrap();
        cfg.general.font = "C:\\Fonts\\My Font.ttf".to_string();
        cfg.general.font_size = 30;
        assert_eq!(cfg.write_font_settings().unwrap(), path);
        let back = std::fs::read_to_string(&path).unwrap();
        assert!(back.contains("# comment"), "comment kept: {back}");
        let reloaded: Config = toml::from_str(&back).unwrap();
        assert_eq!(reloaded.general.font, "C:\\Fonts\\My Font.ttf");
        assert_eq!(reloaded.general.font_size, 30);
        // An existing font_size line is replaced, not duplicated.
        assert_eq!(back.matches("font_size =").count(), 1, "back: {back}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn press(key: Key, mods: Mods) -> KeyPress {
        KeyPress {
            key,
            mods,
            text: None,
            kind: KeyKind::Press,
        }
    }

    #[test]
    fn control_chars_normalize_to_letters() {
        assert_eq!(control_to_letter('\u{f}'), 'o');
        assert_eq!(control_to_letter('\u{1}'), 'a');
        assert_eq!(control_to_letter('x'), 'x');
        // Binding matches either representation.
        assert!(matches(
            &press(Key::Char('o'), Mods::CONTROL),
            "ctrl+o"
        ));
    }

    #[test]
    fn classify_char_handles_all_representations() {
        // ctrl+o as a bare C0 char, no CONTROL flag: gains CONTROL.
        let (k, m) = classify_char('\u{f}', Mods::empty()).unwrap();
        assert_eq!(k, Key::Char('o'));
        assert!(m.contains(Mods::CONTROL));
        assert!(matches(
            &KeyPress {
                key: k,
                mods: m,
                text: None,
                kind: KeyKind::Press,
            },
            "ctrl+o"
        ));
        // ctrl+o as the letter with CONTROL.
        let (k, m) = classify_char('o', Mods::CONTROL).unwrap();
        assert_eq!(k, Key::Char('o'));
        assert!(m.contains(Mods::CONTROL));
        // Plain letter and space pass through.
        assert_eq!(classify_char('v', Mods::empty()).unwrap().0, Key::Char('v'));
        assert_eq!(classify_char(' ', Mods::empty()).unwrap().0, Key::Space);
        // ctrl+space (NUL) becomes Space + CONTROL.
        let (k, m) = classify_char('\0', Mods::empty()).unwrap();
        assert_eq!(k, Key::Space);
        assert!(m.contains(Mods::CONTROL));
    }

    #[test]
    fn ctrl_alt_tolerates_altgr() {
        // AltGr: press reports ALT only, spec asks for CONTROL|ALT.
        let mut only_alt = Mods::empty();
        only_alt.insert(Mods::ALT);
        assert!(matches(
            &press(Key::Char('s'), only_alt),
            "ctrl+alt+s"
        ));
        // Normal ctrl+alt still matches too.
        let mut both = Mods::empty();
        both.insert(Mods::CONTROL);
        both.insert(Mods::ALT);
        assert!(matches(&press(Key::Char('s'), both), "ctrl+alt+s"));
        // But alt-only must not satisfy a plain ctrl binding.
        assert!(!matches(&press(Key::Char('s'), only_alt), "ctrl+s"));
    }

    /// Ctrl+Alt+Shift+<letter> on an AltGr layout arrives with an
    /// `Unidentified` logical key; the modifier-free fallback must resolve it
    /// so the binding is not dropped.
    #[test]
    fn altgr_unidentified_uses_modifier_free_key() {
        use winit::keyboard::NativeKey;

        let unidentified = WinitKey::Unidentified(NativeKey::Unidentified);
        let base = WinitKey::Character("s".into());
        assert_eq!(resolve_logical(&unidentified, &base), base);

        // A resolvable logical key is kept (real AltGr characters survive).
        let euro = WinitKey::Character("\u{20ac}".into());
        assert_eq!(resolve_logical(&euro, &base), euro);

        // The resolved Char('s') plus control/shift/alt matches the binding.
        let mut mods = Mods::empty();
        mods.insert(Mods::CONTROL);
        mods.insert(Mods::SHIFT);
        mods.insert(Mods::ALT);
        assert!(matches(&press(Key::Char('s'), mods), "ctrl+shift+alt+s"));
    }

    #[test]
    fn select_mode_binding_matches_ctrl_shift_space() {
        let mut mods = Mods::empty();
        mods.insert(Mods::CONTROL);
        mods.insert(Mods::SHIFT);
        let p = KeyPress {
            key: Key::Space,
            mods,
            text: None,
            kind: KeyKind::Press,
        };
        assert!(matches(&p, "ctrl+shift+space"));
    }

    #[test]
    fn zoom_specs_parse() {
        assert_eq!(parse("ctrl+plus").unwrap().key, Key::Char('+'));
        assert_eq!(parse("ctrl+minus").unwrap().key, Key::Char('-'));
        assert_eq!(parse("ctrl+0").unwrap().key, Key::Char('0'));
    }

    #[test]
    fn arrow_aliases() {
        assert_eq!(parse_key("right_arrow"), Some(Key::Right));
        assert_eq!(parse_key("left_arrow"), Some(Key::Left));
        assert_eq!(parse_key("up_arrow"), Some(Key::Up));
        assert_eq!(parse_key("down_arrow"), Some(Key::Down));
        assert!(matches(
            &press(Key::Right, Mods::CONTROL | Mods::SHIFT),
            "ctrl+shift+right_arrow"
        ));
    }

    #[test]
    fn parse_ctrl_q() {
        let b = parse("ctrl+q").unwrap();
        assert_eq!(b.key, Key::Char('q'));
        assert!(b.mods.contains(Mods::CONTROL));
    }

    #[test]
    fn ctrl_binding_ignores_case() {
        assert!(matches(
            &press(Key::Char('b'), Mods::CONTROL),
            "ctrl+B"
        ));
        assert!(matches(
            &press(Key::Char('B'), Mods::CONTROL | Mods::SHIFT),
            "ctrl+b"
        ));
    }

    #[test]
    fn matches_any_hits_each_binding() {
        let specs = vec!["ctrl+q".to_string(), "alt+q".to_string()];
        assert!(matches_any(&press(Key::Char('q'), Mods::CONTROL), &specs));
        assert!(matches_any(&press(Key::Char('q'), Mods::ALT), &specs));
        assert!(!matches_any(&press(Key::Char('q'), Mods::empty()), &specs));
        let empty: Vec<String> = Vec::new();
        assert!(!matches_any(&press(Key::Char('q'), Mods::CONTROL), &empty));
    }

    #[test]
    fn match_ignores_char_shift() {
        assert!(matches(
            &press(Key::Char('q'), Mods::CONTROL),
            "ctrl+q"
        ));
        assert!(!matches(
            &press(Key::Char('q'), Mods::CONTROL),
            "ctrl+w"
        ));
    }

    /// A spec that asks for SHIFT requires it, so ctrl+r and ctrl+shift+r are
    /// different bindings (while ctrl+B still matches ctrl+b).
    #[test]
    fn explicit_shift_spec_is_required() {
        assert!(matches(
            &press(Key::Char('r'), Mods::CONTROL | Mods::SHIFT),
            "ctrl+shift+r"
        ));
        assert!(!matches(
            &press(Key::Char('r'), Mods::CONTROL),
            "ctrl+shift+r"
        ));
    }

    #[test]
    fn shift_pageup_spec() {
        let b = parse("shift+pageup").unwrap();
        assert_eq!(b.key, Key::PageUp);
        assert!(b.mods.contains(Mods::SHIFT));
        assert!(matches(&press(Key::PageUp, Mods::SHIFT), "shift+pageup"));
    }

    /// Alt+Tab is an OS window-switch shortcut: Windows delivers the Tab
    /// press to the app before stealing focus, and the stray `\t` would land
    /// in the shell's input line. Swallow it so it never reaches the pane.
    #[test]
    fn alt_tab_is_swallowed() {
        use winit::keyboard::{Key as WinitKey, NamedKey};

        let tab = WinitKey::Named(NamedKey::Tab);
        let mut alt = Mods::empty();
        alt.insert(Mods::ALT);
        assert!(is_os_shortcut(&tab, alt));
        assert!(!is_os_shortcut(&tab, Mods::empty()));
        let mut shift = Mods::empty();
        shift.insert(Mods::SHIFT);
        assert!(!is_os_shortcut(&tab, shift));
        // Other keys are not affected.
        let esc = WinitKey::Named(NamedKey::Escape);
        assert!(!is_os_shortcut(&esc, alt));
    }

    #[test]
    fn ctrl_bytes() {
        assert_eq!(ctrl_byte('a'), Some(1));
        assert_eq!(ctrl_byte('q'), Some(17));
        assert_eq!(to_bytes(&press(Key::Enter, Mods::empty())), vec![b'\r']);
        assert_eq!(to_bytes(&press(Key::Up, Mods::empty())), b"\x1b[A".to_vec());
    }

    /// Navigation keys carry modifiers (word movement, selection).
    #[test]
    fn modifier_aware_navigation() {
        let ctrl = |k| press(k, Mods::CONTROL);
        assert_eq!(to_bytes(&ctrl(Key::Left)), b"\x1b[1;5D".to_vec());
        assert_eq!(to_bytes(&ctrl(Key::Right)), b"\x1b[1;5C".to_vec());
        assert_eq!(
            to_bytes(&press(Key::Up, Mods::SHIFT)),
            b"\x1b[1;2A".to_vec()
        );
        assert_eq!(
            to_bytes(&press(Key::Right, Mods::CONTROL | Mods::SHIFT)),
            b"\x1b[1;6C".to_vec()
        );
        assert_eq!(to_bytes(&ctrl(Key::Delete)), b"\x1b[3;5~".to_vec());
        assert_eq!(to_bytes(&press(Key::PageUp, Mods::empty())), b"\x1b[5~".to_vec());
    }

    /// Ctrl+Backspace deletes the previous word; plain/alt backspace differ.
    #[test]
    fn ctrl_backspace_kills_word() {
        assert_eq!(to_bytes(&press(Key::Backspace, Mods::CONTROL)), vec![0x17]);
        assert_eq!(to_bytes(&press(Key::Backspace, Mods::empty())), vec![0x7f]);
        assert_eq!(to_bytes(&press(Key::Backspace, Mods::ALT)), vec![0x1b, 0x7f]);
    }

    #[test]
    fn text_is_preferred_for_printables() {
        let p = KeyPress {
            key: Key::Char('A'),
            mods: Mods::SHIFT,
            text: Some("A".into()),
            kind: KeyKind::Press,
        };
        assert_eq!(to_bytes(&p), b"A".to_vec());
    }

    #[test]
    fn alt_prefixes_escape() {
        let p = KeyPress {
            key: Key::Char('x'),
            mods: Mods::ALT,
            text: Some("x".into()),
            kind: KeyKind::Press,
        };
        assert_eq!(to_bytes(&p), b"\x1bx".to_vec());
    }

    /// `win32-input-mode` records carry Vk/Sc/Uc/Kd/Cs/Rc; press has Kd=1 and
    /// release Kd=0 so a held key (slider) can be observed by the child.
    #[cfg(windows)]
    #[test]
    fn win32_bytes_encode_press_and_release() {
        let params = |kind| {
            let p = KeyPress {
                key: Key::Char('z'),
                mods: Mods::empty(),
                text: Some("z".into()),
                kind,
            };
            let text = String::from_utf8(to_win32_bytes(&p)).unwrap();
            let body = text.strip_prefix("\x1b[").unwrap().strip_suffix("_").unwrap();
            body.split(';').map(str::to_string).collect::<Vec<_>>()
        };
        let down = params(KeyKind::Press);
        let up = params(KeyKind::Release);
        assert_eq!(down.len(), 6);
        assert_eq!(up.len(), 6);
        assert_eq!(down[2], "122"); // UnicodeChar 'z'
        assert_eq!(up[2], "122");
        assert_eq!(down[3], "1"); // bKeyDown
        assert_eq!(up[3], "0"); // key up
        assert!(down[0].parse::<u32>().unwrap() > 0, "virtual key code present");
    }

    /// Ctrl+<letter> must carry the control character (Ctrl+P -> 0x10), not the
    /// base letter, or ConPTY hands a plain letter to the hosted app.
    #[cfg(windows)]
    #[test]
    fn win32_ctrl_letter_uses_control_char() {
        let p = KeyPress {
            key: Key::Char('p'),
            mods: Mods::CONTROL,
            text: Some("p".into()),
            kind: KeyKind::Press,
        };
        let text = String::from_utf8(to_win32_bytes(&p)).unwrap();
        let body = text.strip_prefix("\x1b[").unwrap().strip_suffix("_").unwrap();
        let params: Vec<&str> = body.split(';').collect();
        assert_eq!(params[2], "16", "Ctrl+P must send UnicodeChar 0x10");
        assert_eq!(params[4], "8", "Ctrl state bit set");
    }

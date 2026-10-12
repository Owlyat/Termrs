    use super::*;

    fn sgr(button: u32, release: bool, col: u16, row: u16) -> Vec<u8> {
        encode(button, release, false, col, row, Mods::empty(), MouseEncoding::Sgr)
    }

    #[test]
    fn sgr_press_release_motion_vectors() {
        // Left press at (0,0) -> 1-based (1,1).
        assert_eq!(sgr(0, false, 0, 0), b"\x1b[<0;1;1M".to_vec());
        assert_eq!(
            encode_press(MouseButton::Left, 4, 9, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<0;5;10M".to_vec()
        );
        // SGR release carries the released button (left = 0) with `m`,
        // matching xterm/wezterm (what ConPTY and crossterm expect).
        assert_eq!(
            encode_release(MouseButton::Left, 4, 9, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<0;5;10m".to_vec()
        );
        assert_eq!(
            encode_release(MouseButton::Right, 4, 9, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<2;5;10m".to_vec()
        );
        // Drag adds the motion bit (0 + 32).
        assert_eq!(
            encode_motion(
                Some(MouseButton::Left),
                4,
                9,
                Mods::empty(),
                MouseEncoding::Sgr
            ),
            b"\x1b[<32;5;10M".to_vec()
        );
        // Free move reports button 3 with `M`.
        assert_eq!(
            encode_motion(None, 4, 9, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<35;5;10M".to_vec()
        );
        // Wheel up/down are buttons 64/65, press-only.
        assert_eq!(
            encode_wheel(true, 0, 0, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<64;1;1M".to_vec()
        );
        assert_eq!(
            encode_wheel(false, 79, 24, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<65;80;25M".to_vec()
        );
        // Modifiers OR into the button code: ctrl(16) + left drag (0+32).
        let mut mods = Mods::empty();
        mods.insert(Mods::CONTROL);
        assert_eq!(
            encode_motion(Some(MouseButton::Left), 0, 0, mods, MouseEncoding::Sgr),
            b"\x1b[<48;1;1M".to_vec()
        );
        // Middle and right buttons.
        assert_eq!(sgr(1, false, 0, 0), b"\x1b[<1;1;1M".to_vec());
        assert_eq!(sgr(2, false, 0, 0), b"\x1b[<2;1;1M".to_vec());
    }

    #[test]
    fn x10_and_utf8_vectors() {
        // Left press at (4,9): ESC M, 32+0, 32+5, 32+10.
        assert_eq!(
            encode_press(MouseButton::Left, 4, 9, Mods::empty(), MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 32, 37, 42]
        );
        // X10 release keeps the button-3 marker (no room for the button).
        assert_eq!(
            encode_release(MouseButton::Left, 4, 9, Mods::empty(), MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 35, 37, 42]
        );
        // X10 has no modifier bits: shift is ignored.
        let mut mods = Mods::empty();
        mods.insert(Mods::SHIFT);
        assert_eq!(
            encode_press(MouseButton::Left, 0, 0, mods, MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // X10 clamps coordinates to one byte.
        assert_eq!(
            encode_press(MouseButton::Left, 500, 500, Mods::empty(), MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 32, 255, 255]
        );
        // UTF-8 encodes coordinates as raw codepoints, not ASCII digits:
        // col 4 -> 5 (one byte 0x05), row 9 -> 10 (one byte 0x0A).
        assert_eq!(
            encode_press(MouseButton::Left, 4, 9, Mods::empty(), MouseEncoding::Utf8),
            vec![0x1b, b'[', b'M', 32, 5, 10]
        );
        // Large coordinates become multibyte sequences (301 -> U+012D).
        assert_eq!(
            encode_press(MouseButton::Left, 300, 9, Mods::empty(), MouseEncoding::Utf8),
            vec![0x1b, b'[', b'M', 32, 0xC4, 0xAD, 10]
        );
        // Wheel in X10: 32+64 / 32+65.
        assert_eq!(
            encode_wheel(true, 0, 0, Mods::empty(), MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 96, 33, 33]
        );
    }

    #[test]
    fn url_middle_and_edges() {
        let lines = vec!["see https://example.com/a?b=1 here".to_string()];
        assert_eq!(
            find_url(&lines, 0, 10),
            Some("https://example.com/a?b=1".to_string())
        );
        // First and last char of the URL.
        assert_eq!(
            find_url(&lines, 0, 4),
            Some("https://example.com/a?b=1".to_string())
        );
        assert_eq!(
            find_url(&lines, 0, 28),
            Some("https://example.com/a?b=1".to_string())
        );
        // Just outside either end: nothing.
        assert_eq!(find_url(&lines, 0, 3), None);
        assert_eq!(find_url(&lines, 0, 29), None);
        // No URL on this row / out of range.
        assert_eq!(find_url(&["plain text".to_string()], 0, 2), None);
        assert_eq!(find_url(&lines, 3, 0), None);
        assert_eq!(find_url(&lines, 0, 500), None);
    }

    #[test]
    fn url_trims_punctuation_and_balances_parens() {
        // Trailing sentence punctuation is not part of the link.
        let lines = vec!["go https://example.com/x., next".to_string()];
        assert_eq!(
            find_url(&lines, 0, 5),
            Some("https://example.com/x".to_string())
        );
        // Balanced parens (Wikipedia style) are kept...
        let lines = vec!["a https://en.wikipedia.org/wiki/X_(thing) ok".to_string()];
        assert_eq!(
            find_url(&lines, 0, 30),
            Some("https://en.wikipedia.org/wiki/X_(thing)".to_string())
        );
        // ...while an extra closer from surrounding prose is dropped.
        let lines = vec!["(see https://example.com/y))".to_string()];
        assert_eq!(
            find_url(&lines, 0, 10),
            Some("https://example.com/y".to_string())
        );
        // www. gains a scheme.
        let lines = vec!["open www.example.org/a!".to_string()];
        assert_eq!(
            find_url(&lines, 0, 7),
            Some("https://www.example.org/a".to_string())
        );
        // Bare markers with nothing after them are not links.
        let lines = vec!["go https:// next".to_string()];
        assert_eq!(find_url(&lines, 0, 5), None);
        let lines = vec!["visit www. now".to_string()];
        assert_eq!(find_url(&lines, 0, 7), None);
    }

    #[test]
    fn openable_rejects_non_http() {
        assert!(is_openable_url("https://example.com"));
        assert!(is_openable_url("http://localhost:3000/x"));
        assert!(!is_openable_url(""));
        assert!(!is_openable_url("https://"));
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("ftp://example.com"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("www.example.com"));
        assert!(!is_openable_url("https://exa mple.com"));
    }

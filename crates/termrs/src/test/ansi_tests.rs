    use super::*;

    fn strip_ansi(s: &str) -> String {
        strip_ansi_markers(s).0
    }

    #[test]
    fn strips_sgr_and_cursor_sequences() {
        let raw = "\x1b[1;32mhello\x1b[0m world\x1b[K\r\n\x1b[2Kprompt> ";
        assert_eq!(strip_ansi(raw), "hello world\nprompt> ");
    }

    #[test]
    fn strips_osc_hyperlinks_and_titles() {
        // OSC payloads (titles, hyperlinks) are dropped entirely.
        let raw = "\x1b]0;title\x07text\x1b]8;;http://x\x07link\x1b]8;;\x07";
        assert_eq!(strip_ansi(raw), "textlink");
    }

    #[test]
    fn lone_cr_becomes_newline() {
        assert_eq!(strip_ansi("a\rb"), "a\nb");
    }

    #[test]
    fn drops_other_controls_keeps_tabs() {
        assert_eq!(strip_ansi("a\x00b\tc\x07"), "ab\tc");
    }

    #[test]
    fn records_prompt_marker_offsets() {
        // ESC ] 133;A ST, then a prompt line.
        let raw = "out\x1b]133;A\x1b\\PS> ";
        let (text, marks) = strip_ansi_markers(raw);
        assert_eq!(text, "outPS> ");
        assert_eq!(marks, vec![3]);
    }

    #[test]
    fn osc7_bel_and_st_terminators() {
        let bel = "hi\x1b]7;file://host/C:/Users/me\x07there";
        let st = "hi\x1b]7;file:///home/user\x1b\\there";
        let got: Vec<String> = extract_osc7_dirs(bel)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, vec!["C:/Users/me"]);
        let got: Vec<String> = extract_osc7_dirs(st)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        // Unix path keeps its leading slash.
        assert_eq!(got, vec!["/home/user"]);
    }

    #[test]
    fn osc7_percent_decoding_and_skips_junk() {
        let raw = "\x1b]7;file://h/C:/My%20Docs%2Fa\x07\x1b]0;title\x07\x1b]7;notauri\x07";
        let got: Vec<String> = extract_osc7_dirs(raw)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, vec!["C:/My Docs/a"]);
        // Unterminated tail yields nothing (caller retries with more bytes).
        assert!(extract_osc7_dirs("\x1b]7;file://h/C:/half").is_empty());
        // Bad `%` survives literally, empty path is dropped.
        assert_eq!(percent_decode("a%2Fb%zzc%"), "a/b%zzc%");
        assert!(osc7_uri_to_path("file://h").is_none());
    }

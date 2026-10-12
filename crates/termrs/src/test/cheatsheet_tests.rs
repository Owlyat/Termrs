    use super::*;

    /// Real `cheat.sh/tar?T` shape (trimmed): markers, frontmatter,
    /// multi-line comments, stacked commands sharing one comment.
    const TAR_SAMPLE: &str = "#[cheat.sheets:tar]\n\
         # tar\n\
         # GNU version of the tar archiving utility\n\
         \n\
         # Delete file 'xdm' from the archive given to the `-f` flag.\n\
         tar --delete -f xdm_edited.tar.gz xdm\n\
         \n\
         # Extract the contents to the destination given to `-C`;\n\
         # falls back to the CWD when missing.\n\
         tar -C /mnt -xvf Tarball.tar\n\
         #[cheat:tar]\n\
         ---\n\
         tags: [ compression ]\n\
         ---\n\
         # To extract a .tgz or .tar.gz archive:\n\
         tar -xzvf /path/to/foo.tgz\n\
         tar -xzvf /path/to/foo.tar.gz\n\
         \n\
         # To create one:\n\
         $ tar -czvf /path/to/foo.tgz /path/to/foo/\n";

    #[test]
    fn parses_sections_comments_and_stacked_commands() {
        let got = parse_sheet(TAR_SAMPLE);
        assert_eq!(got.len(), 5, "entries: {got:?}");
        assert_eq!(got[0].command, "tar --delete -f xdm_edited.tar.gz xdm");
        assert!(got[0].comment.contains("Delete file"), "comment: {:?}", got[0].comment);
        // Multi-line comment block joins with a space.
        assert!(got[1].comment.contains("falls back"), "comment: {:?}", got[1].comment);
        // Markers, separators and frontmatter never become rows.
        assert!(!got.iter().any(|e| e.command.contains("cheat") || e.command == "---"));
        assert!(!got.iter().any(|e| e.comment.contains("cheat.sheets")));
        // Stacked commands share their comment block...
        assert_eq!(got[2].command, "tar -xzvf /path/to/foo.tgz");
        assert_eq!(got[3].command, "tar -xzvf /path/to/foo.tar.gz");
        assert_eq!(got[2].comment, got[3].comment);
        assert!(got[2].comment.contains("extract a .tgz"), "comment: {:?}", got[2].comment);
        // ...and a `$ ` prompt prefix is stripped.
        assert_eq!(got[4].command, "tar -czvf /path/to/foo.tgz /path/to/foo/");
    }

    /// Multi-source answer (`cheat:fd` + `tldr:fd` sections): headers are
    /// skipped, every other non-comment line is a command.
    #[test]
    fn parses_multisource_fd_sheet() {
        let sample = "cheat:fd\n\
             # Simple search:\n\
             fd <search query>\n\
             \n\
             # Specifying the root directory for the search:\n\
             fd <search query> <directory>\n\
             \n\
             # Searching for a particular file extension:\n\
             fd -e <file extension> <search query>\n\
             \n\
             tldr:fd\n\
             # fd\n\
             # An alternative to `find`.\n\
             # Aims to be faster and easier to use than `find`.\n\
             # More information: <https://github.com/sharkdp/fd>.\n\
             \n\
             # Recursively find files matching a specific pattern in the current directory:\n\
             fd \"string|regex\"\n\
             \n\
             # Execute a command on each search result returned:\n\
             fd \"string|regex\" --exec command\n";
        let got = parse_sheet(sample);
        assert_eq!(got.len(), 5, "entries: {got:?}");
        // Section headers never become rows...
        assert!(!got.iter().any(|e| e.command == "cheat:fd" || e.command == "tldr:fd"));
        // ...and every other line is picked up with its comment.
        assert_eq!(got[0].command, "fd {search_query}");
        assert!(got[0].comment.contains("Simple search"), "comment: {:?}", got[0].comment);
        assert_eq!(got[1].command, "fd {search_query} {directory}");
        assert_eq!(got[2].command, "fd -e {file_extension} {search_query}");
        assert_eq!(got[3].command, "fd \"string|regex\"");
        assert!(got[3].comment.contains("Recursively find"), "comment: {:?}", got[3].comment);
        assert_eq!(got[4].command, "fd \"string|regex\" --exec command");
    }

    #[test]
    fn section_headers_detected() {
        assert!(is_section_header("cheat:fd"));
        assert!(is_section_header("tldr:fd"));
        assert!(is_section_header("cheat:git/log"));
        assert!(is_section_header("section:"));
        assert!(!is_section_header("fd <search query>"));
        assert!(!is_section_header("C:\\path\\file:"));
        assert!(!is_section_header("http://host:8080"));
        assert!(!is_section_header("tags: [ x ]"));
        assert!(!is_section_header(":"));
        assert!(!is_section_header("not a header: with spaces"));
    }

    #[test]
    fn html_bodies_rejected() {
        assert!(is_html_body("<html>\n<head><title>cheat.sh/fd</title>"));
        assert!(is_html_body("<!DOCTYPE html><html lang=\"en\">"));
        assert!(is_html_body("  \n<HTML>"));
        assert!(!is_html_body("# tar\ntar -xvf foo.tar\n"));
        assert!(!is_html_body("cheat:fd\n# hi\nfd x\n"));
    }

    #[test]
    fn blanks_do_not_reset_comments() {
        // Every `#` line above a command is its comment, blanks or not.
        let got = parse_sheet("# what it does\n\ncmd --flag\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].comment, "what it does", "got: {got:?}");
        // ...including multi-line blocks split by blanks, and inheritance
        // past a blank after a consumed block.
        let got = parse_sheet("# line one\n\n# line two\ncmd-a\n\ncmd-b\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].comment, "line one line two", "got: {got:?}");
        assert_eq!(got[1].comment, "line one line two", "got: {got:?}");
    }

    #[test]
    fn placeholders_become_picker_values() {
        assert_eq!(cheats_to_placeholders("tar -xvf <archive>"), "tar -xvf {archive}");
        assert_eq!(
            cheats_to_placeholders("cp <source file> <dest>"),
            "cp {source_file} {dest}"
        );
        assert_eq!(
            cheats_to_placeholders("tar -czvf <target.tar> <dir>"),
            "tar -czvf {target_tar} {dir}"
        );
        // Shell syntax stays literal: redirect-from, heredoc, bare `<`.
        assert_eq!(cheats_to_placeholders("cmd <- file"), "cmd <- file");
        assert_eq!(cheats_to_placeholders("cat <<EOF"), "cat <<EOF");
        assert_eq!(cheats_to_placeholders("a<b"), "a<b");
        assert_eq!(cheats_to_placeholders("echo <>"), "echo <>");
        assert_eq!(cheats_to_placeholders("plain command"), "plain command");
    }

    #[test]
    fn topics_encode_safely() {
        assert_eq!(encode_topic("tar"), "tar");
        assert_eq!(encode_topic("git/log"), "git/log");
        assert_eq!(encode_topic("c++"), "c++");
        assert_eq!(encode_topic("a b"), "a%20b");
        assert_eq!(encode_topic("a?b"), "a%3Fb");
    }

    #[test]
    fn dedupe_skips_saved_commands() {
        let entries = vec![
            CheatEntry { command: "tar -xvf {f}".into(), comment: String::new() },
            CheatEntry { command: "tar -czvf a b".into(), comment: String::new() },
        ];
        let existing: HashSet<String> = ["tar -czvf a b".to_string()].into_iter().collect();
        let fresh = dedupe_new(entries, &existing);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].command, "tar -xvf {f}");
    }

    /// Live fetch (needs network): run explicitly, never in the suite.
    /// Proves text mode (HTML bodies are rejected) and multi-source parse.
    #[test]
    #[ignore = "needs network; run with --ignored"]
    fn live_fetch_tar_sheet() {
        let text = fetch_topic("tar").expect("cheat.sh reachable");
        let entries = parse_sheet(&text);
        assert!(!entries.is_empty(), "tar sheet parsed to nothing");
        assert!(
            entries.iter().any(|e| e.command.contains("tar")),
            "no tar command found"
        );
        // Multi-source topic: text (not HTML), only runnable rows.
        let fd = fetch_topic("fd").expect("cheat.sh fd reachable");
        let entries = parse_sheet(&fd);
        assert!(!entries.is_empty(), "fd sheet parsed to nothing");
        assert!(
            entries.iter().all(|e| e.command.contains("fd")),
            "non-fd rows imported: {entries:?}"
        );
        assert!(
            !entries.iter().any(|e| e.command.contains("cheat:") || e.command.contains("tldr:")),
            "section headers imported: {entries:?}"
        );
    }

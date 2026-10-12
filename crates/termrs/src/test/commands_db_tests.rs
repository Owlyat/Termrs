    use super::*;

    fn temp_db(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "termrs-db-{tag}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn add_query_delete_roundtrip() {
        let path = temp_db("roundtrip");
        let db = CommandDb::open(&path).expect("open");
        let id = db.add("git status", "show the working tree", "git").unwrap();
        assert!(id > 0);
        let all = db.all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].command, "git status");
        assert_eq!(all[0].comment, "show the working tree");
        assert_eq!(all[0].uses, 0);

        db.mark_used(id).unwrap();
        assert_eq!(db.all().unwrap()[0].uses, 1);

        db.delete(id).unwrap();
        assert!(db.all().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn placeholders_are_unique_and_ordered() {
        assert_eq!(placeholders("cp {src} {dst} {src}"), vec!["src", "dst"]);
        assert_eq!(placeholders("echo {missing_brace"), Vec::<String>::new());
        assert_eq!(placeholders("echo {a b}"), Vec::<String>::new());
        assert_eq!(placeholders("no placeholders here"), Vec::<String>::new());
    }

    #[test]
    fn fill_quotes_only_when_needed() {
        let v = vec![("file".to_string(), "a b.txt".to_string())];
        assert_eq!(fill("rm {file}", &v), "rm \"a b.txt\"");
        assert_eq!(fill("rm \"{file}\"", &v), "rm \"a b.txt\"");
        let one = vec![("file".to_string(), "notes.txt".to_string())];
        assert_eq!(fill("cat {file}", &one), "cat notes.txt");
    }

    use super::*;

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termrs-fonts-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn char_width(bytes: &[u8], size: u32) -> u32 {
        match advance_ratio(bytes) {
            Some((adv, height)) => (adv * size as f32 / height) as u32,
            None => 0,
        }
    }

    /// `normalize_fallback_metrics` never lowers the cell width a fallback
    /// would impose, so the primary font wins the `min` and cells stay 2:1.
    #[test]
    fn normalize_fallback_metrics_keeps_primary_width() {
        // Real primary + fallbacks when installed (vacuous otherwise).
        let Some(primary_path) = candidates().into_iter().find(|p| p.is_file()) else {
            return;
        };
        let primary = std::fs::read(&primary_path).unwrap();
        if advance_ratio(&primary).is_none() {
            return;
        }
        let target_w = char_width(&primary, 26);
        for p in fallback_candidates().into_iter().filter(|p| p.is_file()) {
            let mut b = std::fs::read(&p).unwrap();
            let before = char_width(&b, 26);
            normalize_fallback_metrics(&primary, &mut b);
            let after = char_width(&b, 26);
            assert!(
                after >= before,
                "{} shrank: {before} -> {after}",
                p.display()
            );
            assert!(
                after >= target_w,
                "{} still narrower than primary ({after} < {target_w})",
                p.display()
            );
        }
    }

    #[test]
    fn font_file_extension_check() {
        assert!(is_font_file(std::path::Path::new("a.ttf")));
        assert!(is_font_file(std::path::Path::new("B.OTF")));
        assert!(is_font_file(std::path::Path::new("c.ttc")));
        assert!(!is_font_file(std::path::Path::new("d.txt")));
        assert!(!is_font_file(std::path::Path::new("e.ttf.bak")));
        assert!(!is_font_file(std::path::Path::new("noext")));
    }

    /// Recursive scan finds font files, ignores the rest, sorts by name
    /// and dedupes overlapping directories.
    #[test]
    fn scan_dirs_finds_sorts_and_dedupes() {
        let dir = unique_dir("scan");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("Zeta.ttf"), b"x").unwrap();
        std::fs::write(dir.join("alpha.otf"), b"x").unwrap();
        std::fs::write(sub.join("mid.ttc"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        let found = scan_dirs(&[dir.clone(), sub.clone(), dir.clone()]);
        let names: Vec<&str> = found.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "Zeta"]);
        assert!(found.iter().all(|e| e.path.is_absolute() || e.path.starts_with(&dir)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Missing directories scan to nothing (no panic, no error).
    #[test]
    fn scan_dirs_tolerates_missing_dirs() {
        let missing = std::env::temp_dir().join(format!(
            "termrs-fonts-nope-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(scan_dirs(&[missing]).is_empty());
    }

    /// The OS-specific directory list is never empty.
    #[test]
    fn system_font_dirs_not_empty() {
        assert!(!system_font_dirs().is_empty());
    }

    /// Fallback paths mirror `load_set`: configured verbatim, else the
    /// existing auto candidates.
    #[test]
    fn fallback_paths_prefers_configured_then_existing() {
        let cfg = vec!["C:\\x.ttf".to_string(), "missing.ttf".to_string()];
        assert_eq!(
            fallback_paths(&cfg),
            vec![
                std::path::PathBuf::from("C:\\x.ttf"),
                std::path::PathBuf::from("missing.ttf")
            ]
        );
        for p in fallback_paths(&[]) {
            assert!(p.is_file(), "auto fallback must exist: {}", p.display());
        }
    }

    /// Auto fallbacks collect every readable candidate, monospace first;
    /// missing files are skipped and the total is capped.
    #[test]
    fn collect_auto_fallbacks_orders_and_caps() {
        let dir = unique_dir("fallback");
        let junk = dir.join("a-junk.ttf");
        std::fs::write(&junk, b"readable but not a font").unwrap();
        let junk2 = dir.join("b-junk.ttf");
        std::fs::write(&junk2, b"also not a font").unwrap();
        // A real monospace font when one is installed (vacuous otherwise).
        let real = scan_fonts()
            .into_iter()
            .find(|e| is_monospaced(&e.path).unwrap_or(false))
            .map(|e| {
                let dest = dir.join("z-real.ttf");
                std::fs::copy(&e.path, &dest).unwrap();
                dest
            });
        let missing = dir.join("missing.ttf");
        match real {
            Some(mono) => {
                // Junk first, monospace later: monospace still sorts first,
                // junk is kept (backend skips what it cannot parse).
                let mono_len = std::fs::read(&mono).unwrap().len();
                let junk_len = std::fs::read(&junk).unwrap().len();
                let junk2_len = std::fs::read(&junk2).unwrap().len();
                let got = collect_auto_fallbacks(&[
                    junk.clone(),
                    missing.clone(),
                    mono.clone(),
                    junk2.clone(),
                ]);
                let lens: Vec<usize> = got.iter().map(|b| b.len()).collect();
                assert_eq!(lens, vec![mono_len, junk_len, junk2_len]);
            }
            None => {
                // No monospace font installed: readable files still load.
                assert_eq!(collect_auto_fallbacks(&[junk]).len(), 1);
            }
        }
        assert!(collect_auto_fallbacks(&[]).is_empty());
        // Cap: eight readable files collapse to MAX_AUTO_FALLBACKS.
        let mut many = Vec::new();
        for i in 0..8 {
            let p = dir.join(format!("f{i:02}.ttf"));
            std::fs::write(&p, b"x").unwrap();
            many.push(p);
        }
        assert_eq!(collect_auto_fallbacks(&many).len(), MAX_AUTO_FALLBACKS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Monospace probe: garbage and missing files error; a real installed
    /// font (when one exists) parses to a bool.
    #[test]
    fn monospaced_probe_parses_or_errors() {
        let dir = unique_dir("probe");
        let junk = dir.join("junk.ttf");
        std::fs::write(&junk, b"this is not a font").unwrap();
        assert!(is_monospaced(&junk).is_err());
        assert!(is_monospaced(&dir.join("missing.ttf")).is_err());
        let _ = std::fs::remove_dir_all(&dir);

        // Pipeline check against reality (vacuous only with no fonts at all).
        if let Some(first) = scan_fonts().into_iter().next() {
            let r = is_monospaced(&first.path);
            assert!(r.is_ok(), "{}: {r:?}", first.path.display());
        }
    }

    /// Preview raster: garbage/missing/empty inputs error; a real font (when
    /// installed) yields correctly-sized RGBA with actual ink on it.
    #[test]
    fn render_preview_rasters_or_errors() {
        let dir = unique_dir("preview");
        let junk = dir.join("junk.ttf");
        std::fs::write(&junk, b"not a font").unwrap();
        assert!(render_preview(&junk, "T", "b", 24.0, [255, 255, 255], [0, 0, 0]).is_err());
        assert!(render_preview(&dir.join("missing.ttf"), "T", "b", 24.0, [255, 255, 255], [0, 0, 0]).is_err());
        let _ = std::fs::remove_dir_all(&dir);

        if let Some(first) = scan_fonts().into_iter().next() {
            // Empty text is rejected even for a good file.
            assert!(render_preview(&first.path, "  ", " ", 24.0, [255, 255, 255], [0, 0, 0]).is_err());
            let p = render_preview(&first.path, "Ag", "fox 0123", 28.0, [255, 255, 255], [0, 0, 0])
                .expect("real font rasterizes");
            assert!(p.width > 10 && p.height > 10, "{}x{}", p.width, p.height);
            assert_eq!(p.rgba.len(), (p.width * p.height) as usize * 4);
            // Ink check: some pixel must differ from the background.
            let ink = p.rgba.chunks_exact(4).any(|px| px[0] > 10 || px[1] > 10 || px[2] > 10);
            assert!(ink, "preview of {} drew nothing", first.path.display());
        }
    }

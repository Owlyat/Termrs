    use super::*;

    #[test]
    fn tree_round_trips_through_toml() {
        let file = LayoutFile {
            workspaces: vec![WsLayout {
                name: "main".into(),
                tree: TreeLayout::split(
                    SplitDir::Vertical,
                    0.5,
                    TreeLayout::Pane,
                    TreeLayout::split(
                        SplitDir::Horizontal,
                        0.6,
                        TreeLayout::Pane,
                        TreeLayout::Pane,
                    ),
                ),
                dirs: vec![
                    "C:\\proj\\a".to_string(),
                    "C:\\proj\\b".to_string(),
                    "C:\\proj\\c".to_string(),
                ],
            }],
        };
        let text = toml::to_string_pretty(&file).unwrap();
        let back: LayoutFile = toml::from_str(&text).unwrap();
        assert_eq!(back.workspaces.len(), 1);
        assert_eq!(back.workspaces[0].tree, file.workspaces[0].tree);
        assert_eq!(back.workspaces[0].tree.direction(), SplitDir::Vertical);
        assert_eq!(back.workspaces[0].tree.ratio(), 0.5);
        assert_eq!(back.workspaces[0].dirs, file.workspaces[0].dirs);
        // Old files without `dirs` still parse (empty list -> fallback).
        let legacy = toml::from_str::<LayoutFile>(
            "[[workspaces]]\nname = \"main\"\ntree = \"Pane\"\n",
        )
        .expect("legacy layout parses");
        assert!(legacy.workspaces[0].dirs.is_empty());
    }

    /// Build a workspace, save its layout, reload it and rebuild fresh panes.
    #[test]
    fn workspace_layout_survives_save_and_load() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut ws = Workspace::new("main".into(), "", 200, &handle, &wake).expect("workspace");
        ws.split(SplitDir::Vertical, "", 200, &handle).expect("split");
        ws.split(SplitDir::Horizontal, "", 200, &handle).expect("split");
        let original = ws.tree_layout();

        let file = LayoutFile::from_workspaces(std::slice::from_ref(&ws));
        assert_eq!(file.workspaces[0].dirs.len(), 3);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "termrs-layout-{}-{nanos}.toml",
            std::process::id()
        ));
        file.save(&path).expect("layout saves");

        let loaded = LayoutFile::load(&path).expect("layout loads");
        assert_eq!(loaded.workspaces[0].dirs, file.workspaces[0].dirs);
        let mut rebuilt = loaded.build("", 200, &handle, &wake);
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].name, "main");
        assert_eq!(rebuilt[0].tree_layout(), original);
        assert_eq!(rebuilt[0].leaf_ids().len(), 3);
        assert_eq!(rebuilt[0].leaf_cwds(), ws.leaf_cwds());
        ws.kill_all();
        for ws in &mut rebuilt {
            ws.kill_all();
        }
        let _ = std::fs::remove_file(&path);
        rt.shutdown_background();
    }

    /// Panes restart in their saved (configured) directories: distinct dirs
    /// per leaf survive a save/load round-trip and are used as spawn cwds.
    #[test]
    fn workspace_restores_configured_directories() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("termrs-dirs-{}-{nanos}", std::process::id()));
        let dir_a = base.join("a");
        let dir_b = base.join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let mut ws = Workspace::new("main".into(), "", 200, &handle, &wake).expect("workspace");
        ws.split(SplitDir::Vertical, "", 200, &handle).expect("split");
        let ids = ws.leaf_ids();
        assert_eq!(ids.len(), 2);
        ws.pane_mut(ids[0]).unwrap().set_cwd_for_test(dir_a.clone());
        ws.pane_mut(ids[1]).unwrap().set_cwd_for_test(dir_b.clone());

        let file = LayoutFile::from_workspaces(std::slice::from_ref(&ws));
        assert_eq!(file.workspaces[0].dirs.len(), 2);
        let mut rebuilt = file.build("", 200, &handle, &wake);
        assert_eq!(rebuilt.len(), 1);
        let got = rebuilt[0].leaf_cwds();
        assert_eq!(got, vec![dir_a.clone(), dir_b.clone()]);
        // A gone directory falls back instead of failing the restore.
        let mut missing = file.clone();
        missing.workspaces[0].dirs[0] = base.join("gone-missing").to_string_lossy().into_owned();
        let mut rebuilt2 = missing.build("", 200, &handle, &wake);
        assert_eq!(rebuilt2.len(), 1);
        let fallback = std::env::current_dir().unwrap();
        assert_eq!(rebuilt2[0].leaf_cwds()[0], fallback);

        ws.kill_all();
        for ws in &mut rebuilt {
            ws.kill_all();
        }
        for ws in &mut rebuilt2 {
            ws.kill_all();
        }
        let _ = std::fs::remove_dir_all(&base);
        rt.shutdown_background();
    }

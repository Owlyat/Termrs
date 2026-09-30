//! Persisted pane layout: workspaces and their split trees.
//!
//! Saved as `layout.toml` next to the config (or `./layout.toml`). Only the
//! structure plus each pane's directory is stored, never the shells
//! themselves: on startup each leaf is rebuilt as a fresh pane in its saved
//! directory, so a restored workspace behaves like a new one opened in the
//! right place. `dirs` holds one directory per leaf in visual (left-to-right,
//! top-to-bottom) order and is editable: set it to the directory you want a
//! pane to start in. Missing entries fall back to the process directory and
//! gone directories fall back the same way, so old files without `dirs`
//! still load.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::workspace::{SplitDir, Workspace};

/// Split tree as stored on disk (no pane ids: they are reassigned on load).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TreeLayout {
    Pane,
    Split {
        /// `"horizontal"` (stacked) or `"vertical"` (side-by-side).
        dir: String,
        /// First child's share (0.1..=0.9); missing entries load as even.
        #[serde(default = "half_ratio")]
        ratio: f32,
        first: Box<TreeLayout>,
        second: Box<TreeLayout>,
    },
}

fn half_ratio() -> f32 {
    0.5
}

impl TreeLayout {
    /// A split node with the given direction and first-child share.
    pub fn split(dir: SplitDir, ratio: f32, first: TreeLayout, second: TreeLayout) -> Self {
        TreeLayout::Split {
            dir: match dir {
                SplitDir::Horizontal => "horizontal".into(),
                SplitDir::Vertical => "vertical".into(),
            },
            ratio: ratio.clamp(0.1, 0.9),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// Parse the stored direction; unknown values fall back to vertical.
    pub fn direction(&self) -> SplitDir {
        match self {
            TreeLayout::Split { dir, .. } if dir == "horizontal" => SplitDir::Horizontal,
            _ => SplitDir::Vertical,
        }
    }

    /// Parse the stored share; out-of-range values fall back to even.
    pub fn ratio(&self) -> f32 {
        match self {
            TreeLayout::Split { ratio, .. } if (0.1..=0.9).contains(ratio) => *ratio,
            TreeLayout::Split { .. } => 0.5,
            TreeLayout::Pane => 0.5,
        }
    }
}

/// One workspace's saved shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsLayout {
    pub name: String,
    pub tree: TreeLayout,
    /// One directory per leaf in visual order (see [`Node::leaves`]).
    /// Missing/short lists load the remaining panes in the process
    /// directory; extra entries are ignored.
    #[serde(default)]
    pub dirs: Vec<String>,
}

/// Whole saved layout file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutFile {
    #[serde(default)]
    pub workspaces: Vec<WsLayout>,
}

impl LayoutFile {
    /// Read the layout; missing or unparseable files yield `None`.
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        match toml::from_str::<Self>(&text) {
            Ok(f) if !f.workspaces.is_empty() => Some(f),
            Ok(_) => None,
            Err(e) => {
                eprintln!("shellrs: bad layout {}: {e}; ignoring", path.display());
                None
            }
        }
    }

    /// Write the layout; failures are reported but never fatal.
    pub fn save(&self, path: &Path) {
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path, text) {
                    eprintln!("shellrs: cannot save layout {}: {e}", path.display());
                }
            }
            Err(e) => eprintln!("shellrs: cannot serialize layout: {e}"),
        }
    }

    /// Snapshot the live workspaces into a saveable file.
    pub fn from_workspaces(workspaces: &[Workspace]) -> Self {
        Self {
            workspaces: workspaces
                .iter()
                .map(|w| WsLayout {
                    name: w.name.clone(),
                    tree: w.tree_layout(),
                    dirs: w
                        .leaf_cwds()
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect(),
                })
                .collect(),
        }
    }

    /// Rebuild workspaces (fresh panes, each starting in its saved directory).
    pub fn build(
        &self,
        shell: &str,
        scrollback: usize,
        rt: &tokio::runtime::Handle,
        wake: &crossbeam_channel::Sender<()>,
    ) -> Vec<Workspace> {
        self.workspaces
            .iter()
            .filter_map(|w| {
                let dirs: Vec<std::path::PathBuf> =
                    w.dirs.iter().map(std::path::PathBuf::from).collect();
                match Workspace::from_layout(&w.name, &w.tree, &dirs, shell, scrollback, rt, wake) {
                Ok(ws) => Some(ws),
                Err(e) => {
                    eprintln!("shellrs: cannot restore workspace {}: {e}", w.name);
                    None
                }
            }})
            .collect()
    }
}

#[cfg(test)]
mod tests {
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
            "shellrs-layout-{}-{nanos}.toml",
            std::process::id()
        ));
        file.save(&path);

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
        let base = std::env::temp_dir().join(format!("shellrs-dirs-{}-{nanos}", std::process::id()));
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
}

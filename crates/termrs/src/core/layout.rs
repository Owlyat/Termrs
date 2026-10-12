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

// File order (§7-module-map §3): §2 `TreeLayout`/`LayoutFile` types → §4
// `load`/`build` entries → §7 ratio/dir helpers.

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
                eprintln!("termrs: bad layout {}: {e}; ignoring", path.display());
                None
            }
        }
    }

    /// Write the layout. The parent directory is created on demand; errors are
    /// returned so the caller can surface them (a locked/read-only file used
    /// to fail silently on stderr).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| format!("serialize: {e}"))?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
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
                    eprintln!("termrs: cannot restore workspace {}: {e}", w.name);
                    None
                }
            }})
            .collect()
    }
}

#[cfg(test)]
#[path = "../test/layout_tests.rs"]
mod tests;

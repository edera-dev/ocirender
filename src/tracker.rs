//! WhiteoutTracker, EmittedPathTracker, HardLinkTracker.

use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

use crate::canonical::CanonicalTarHeader;

// ─── WhiteoutTracker ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum WhiteoutState {
    Simple { layer_index: usize },
    Opaque { layer_index: usize },
}

#[derive(Debug, Default)]
struct WhiteoutNode {
    state: Option<WhiteoutState>,
    children: HashMap<String, WhiteoutNode>,
}

#[derive(Debug, Default)]
pub struct WhiteoutTracker {
    root: WhiteoutNode,
}

impl WhiteoutTracker {
    /// Mark a specific path as suppressed (simple `.wh.<name>` whiteout).
    pub fn insert_simple(&mut self, path: &Path, layer_index: usize) {
        let node = self.walk_or_create(path);
        node.state = Some(WhiteoutState::Simple { layer_index });
    }

    /// Mark a directory path as opaque (`.wh..wh..opq`).
    pub fn insert_opaque(&mut self, dir_path: &Path, layer_index: usize) {
        let node = self.walk_or_create(dir_path);
        node.state = Some(WhiteoutState::Opaque { layer_index });
    }

    /// Returns true if `path` from `current_layer` should be suppressed.
    pub fn is_suppressed(&self, path: &Path, current_layer: usize) -> bool {
        let components = normal_components(path);
        let mut node = &self.root;
        for (i, comp) in components.iter().enumerate() {
            if let Some(state) = &node.state {
                match state {
                    WhiteoutState::Opaque { layer_index }
                    | WhiteoutState::Simple { layer_index } => {
                        return current_layer < *layer_index;
                    }
                }
            }
            match node.children.get(comp.as_str()) {
                Some(child) => node = child,
                None => return false,
            }
            // At terminal component: check Simple or Opaque.
            if i == components.len() - 1 {
                if let Some(state) = &node.state {
                    match state {
                        WhiteoutState::Simple { layer_index }
                        | WhiteoutState::Opaque { layer_index } => {
                            return current_layer < *layer_index;
                        }
                    }
                }
            }
        }
        false
    }

    fn walk_or_create(&mut self, path: &Path) -> &mut WhiteoutNode {
        let components = normal_components(path);
        let mut node = &mut self.root;
        for comp in &components {
            node = node.children.entry(comp.clone()).or_default();
        }
        node
    }
}

fn normal_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

// ─── EmittedPathTracker ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct EmittedPathTracker(HashSet<PathBuf>);

impl EmittedPathTracker {
    pub fn insert(&mut self, path: &Path) {
        self.0.insert(path.to_path_buf());
    }
    pub fn contains(&self, path: &Path) -> bool {
        self.0.contains(path)
    }
}

// ─── HardLinkTracker ────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct HardLinkEntry {
    pub link_path: PathBuf,
    pub target_path: PathBuf,
    pub layer_index: usize,
    pub canonical: CanonicalTarHeader,
}

#[derive(Debug, Default)]
pub struct HardLinkTracker(pub Vec<HardLinkEntry>);

impl HardLinkTracker {
    pub fn record(
        &mut self,
        link_path: PathBuf,
        target_path: PathBuf,
        layer_index: usize,
        canonical: CanonicalTarHeader,
    ) {
        self.0.push(HardLinkEntry {
            link_path,
            target_path,
            layer_index,
            canonical,
        });
    }
    /// Return deferred links sorted by ascending layer index.
    pub fn drain_sorted(mut self) -> Vec<HardLinkEntry> {
        self.0.sort_by_key(|e| e.layer_index);
        self.0
    }
}

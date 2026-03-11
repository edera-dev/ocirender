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

/// A normal deferred hardlink whose target will be present in the output tar.
/// Emitted after all layers are processed, once we know the target path was
/// written.
#[derive(Debug)]
pub struct HardLinkEntry {
    pub link_path: PathBuf,
    pub target_path: PathBuf,
    pub layer_index: usize,
    pub canonical: CanonicalTarHeader,
}

/// A hardlink whose target path was suppressed by a higher-layer whiteout.
/// The link path itself is live and must be promoted to a standalone regular
/// file.  `file_data` is populated either immediately (same-layer: the
/// suppressed regular file was already buffered) or lazily (cross-layer: filled
/// in when we later encounter the suppressed regular file in an older layer).
/// If `file_data` is still `None` after all layers are processed the promotion
/// is silently dropped (the target never existed — malformed image).
#[derive(Debug)]
pub struct PromotionEntry {
    pub link_path: PathBuf,
    pub target_path: PathBuf,
    pub layer_index: usize,
    /// `(file_canonical, file_bytes)` from the suppressed regular file that
    /// this hardlink originally pointed to.
    pub file_data: Option<(CanonicalTarHeader, Vec<u8>)>,
}

#[derive(Debug, Default)]
pub struct HardLinkTracker {
    deferred: Vec<HardLinkEntry>,
    promotions: Vec<PromotionEntry>,
    /// Content buffered from suppressed regular files keyed by their normalised
    /// path.  Used to satisfy promotion entries:
    ///   - immediately, for same-layer hardlinks (regular file read before
    ///     hardlink within a single tar archive)
    ///   - retroactively, for cross-layer hardlinks (older layer processed after
    ///     the hardlink's layer because we traverse newest-first)
    suppressed_content: HashMap<PathBuf, (CanonicalTarHeader, Vec<u8>)>,
}

impl HardLinkTracker {
    /// Record a normal deferred hardlink whose target is not suppressed.
    pub fn record(
        &mut self,
        link_path: PathBuf,
        target_path: PathBuf,
        layer_index: usize,
        canonical: CanonicalTarHeader,
    ) {
        self.deferred.push(HardLinkEntry {
            link_path,
            target_path,
            layer_index,
            canonical,
        });
    }

    /// Buffer the header and full content of a suppressed regular file.
    ///
    /// Two things happen here:
    /// 1. Any pending `PromotionEntry` that is waiting on this path has its
    ///    `file_data` filled in (cross-layer scenario).
    /// 2. The content is stored in `suppressed_content` so that a hardlink
    ///    appearing later in the *same* layer can find it immediately
    ///    (same-layer scenario).
    pub fn note_suppressed_file(
        &mut self,
        path: PathBuf,
        canonical: CanonicalTarHeader,
        data: Vec<u8>,
    ) {
        // Fulfil any pending promotions that have been waiting for this path.
        for promo in &mut self.promotions {
            if promo.target_path == path && promo.file_data.is_none() {
                promo.file_data = Some((canonical.clone(), data.clone()));
            }
        }
        self.suppressed_content.insert(path, (canonical, data));
    }

    /// Record that `link_path` is a hardlink whose target (`target_path`) has
    /// been suppressed by a higher-layer whiteout.  `link_path` is alive and
    /// must be promoted to a regular file at emit time.
    pub fn record_promotion(
        &mut self,
        link_path: PathBuf,
        target_path: PathBuf,
        layer_index: usize,
    ) {
        // If we already buffered the suppressed content (same-layer case),
        // attach it immediately.
        let file_data = self
            .suppressed_content
            .get(&target_path)
            .map(|(c, d)| (c.clone(), d.clone()));
        self.promotions.push(PromotionEntry {
            link_path,
            target_path,
            layer_index,
            file_data,
        });
    }

    /// Discard the speculative content buffer accumulated during a single layer.
    ///
    /// `suppressed_content` is populated inside `process_layer` to allow
    /// `record_promotion` to attach file data immediately when a suppressed
    /// regular file and a hardlink to it appear in the same layer tar.  Once
    /// `process_layer` returns, no future layer can contain a hardlink pointing
    /// at a file from the layer just processed — the TAR format requires the
    /// regular file to precede its hardlinks within the same archive, and an
    /// older layer cannot reference a path that only exists in a newer layer.
    /// The buffer therefore has no further use and should be cleared to avoid
    /// accumulating memory across layers.
    pub fn end_layer(&mut self) {
        self.suppressed_content.clear();
    }

    /// Consume the tracker and return `(deferred, promotions)`, each sorted by
    /// ascending layer index.  Callers should emit `promotions` first so that
    /// any normal deferred hardlink that happens to target a promoted link path
    /// finds it already in the emitted-path set.
    pub fn drain_sorted(mut self) -> (Vec<HardLinkEntry>, Vec<PromotionEntry>) {
        self.deferred.sort_by_key(|e| e.layer_index);
        self.promotions.sort_by_key(|e| e.layer_index);
        (self.deferred, self.promotions)
    }
}

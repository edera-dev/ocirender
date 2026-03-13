//! State tracking data structures for the OCI layer merge algorithm.
//!
//! Three trackers maintain the in-memory state that the merge engine needs
//! as it processes layers newest-first:
//!
//! - [`WhiteoutTracker`]: records whiteout suppressions as a path trie and
//!   answers "should this path be suppressed?" queries.
//! - [`EmittedPathTracker`]: records paths that have already been written to
//!   the output, so older duplicate entries can be skipped.
//! - [`HardLinkTracker`]: defers hardlinks for emission after all layers are
//!   processed, and handles promotion of hardlinks whose targets were
//!   suppressed by a whiteout.
//!
//! All three are internal to the merge pipeline and are not part of the
//! public library API.

use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

use crate::canonical::CanonicalTarHeader;

// ─── WhiteoutTracker ─────────────────────────────────────────────────────────

/// The suppression state recorded at a trie node.
#[derive(Debug, Clone)]
enum WhiteoutState {
    /// A simple `.wh.<name>` whiteout: suppresses the exact path this node
    /// represents in all layers older than `layer_index`.
    Simple { layer_index: usize },
    /// An opaque `.wh..wh..opq` whiteout: suppresses all content under the
    /// directory this node represents in all layers older than `layer_index`.
    /// Files added to the same directory in the *same* layer are not affected.
    Opaque { layer_index: usize },
}

/// A single node in the [`WhiteoutTracker`] trie.
#[derive(Debug, Default)]
struct WhiteoutNode {
    /// Suppression state at this path component, if any.
    state: Option<WhiteoutState>,
    children: HashMap<String, WhiteoutNode>,
}

/// A path trie that records whiteout suppressions and answers suppression
/// queries during the layer merge.
///
/// Suppression is always keyed on layer index: a whiteout recorded at layer N
/// only suppresses entries from layers with index < N. This means a file added
/// in the same layer as an opaque whiteout on its parent directory is *not*
/// suppressed — the whiteout only removes content from older layers.
///
/// Both simple (`.wh.<name>`) and opaque (`.wh..wh..opq`) whiteouts are
/// supported. An opaque whiteout on a directory suppresses any ancestor match
/// encountered during a downward traversal of the trie, so a query for
/// `dir/subdir/file` will be caught by an opaque whiteout recorded at `dir`.
#[derive(Debug, Default)]
pub struct WhiteoutTracker {
    root: WhiteoutNode,
}

impl WhiteoutTracker {
    /// Record a simple whiteout for `path`, declared in `layer_index`.
    ///
    /// Suppresses exactly `path` in all layers older than `layer_index`.
    pub fn insert_simple(&mut self, path: &Path, layer_index: usize) {
        let node = self.walk_or_create(path);
        node.state = Some(WhiteoutState::Simple { layer_index });
    }

    /// Record an opaque whiteout for `dir_path`, declared in `layer_index`.
    ///
    /// Suppresses all content under `dir_path` in all layers older than
    /// `layer_index`.
    pub fn insert_opaque(&mut self, dir_path: &Path, layer_index: usize) {
        let node = self.walk_or_create(dir_path);
        node.state = Some(WhiteoutState::Opaque { layer_index });
    }

    /// Return `true` if `path` from `current_layer` should be suppressed.
    ///
    /// A path is suppressed if any prefix of it (including the path itself)
    /// has a recorded whiteout with a `layer_index` greater than
    /// `current_layer`. This handles both simple whiteouts (which match the
    /// exact path) and opaque whiteouts (which match any descendant of the
    /// whited-out directory).
    pub fn is_suppressed(&self, path: &Path, current_layer: usize) -> bool {
        let components = normal_components(path);
        let mut node = &self.root;
        for (i, comp) in components.iter().enumerate() {
            // An opaque or simple whiteout at an intermediate node suppresses
            // all descendants — check before descending further.
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
            // At the terminal component, check the node itself.
            if i == components.len() - 1
                && let Some(state) = &node.state
            {
                match state {
                    WhiteoutState::Simple { layer_index }
                    | WhiteoutState::Opaque { layer_index } => {
                        return current_layer < *layer_index;
                    }
                }
            }
        }
        false
    }

    /// Walk the trie to the node for `path`, creating nodes as needed.
    fn walk_or_create(&mut self, path: &Path) -> &mut WhiteoutNode {
        let components = normal_components(path);
        let mut node = &mut self.root;
        for comp in &components {
            node = node.children.entry(comp.clone()).or_default();
        }
        node
    }
}

/// Extract the [`Component::Normal`] segments of `path` as owned strings,
/// discarding root, prefix, and `.`/`..` components.
///
/// Used to build consistent trie keys regardless of how the path was
/// originally formatted.
fn normal_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

// ─── EmittedPathTracker ──────────────────────────────────────────────────────

/// Records paths that have already been written to the output tar stream.
///
/// When a path is first seen in the newest layer that contains it, it is
/// emitted and recorded here. Any subsequent encounter of the same path in
/// an older layer is skipped, implementing the "newest version wins" rule
/// without requiring explicit overwrite logic.
#[derive(Debug, Default)]
pub struct EmittedPathTracker(HashSet<PathBuf>);

impl EmittedPathTracker {
    /// Mark `path` as emitted.
    pub fn insert(&mut self, path: &Path) {
        self.0.insert(path.to_path_buf());
    }

    /// Return `true` if `path` has already been emitted.
    pub fn contains(&self, path: &Path) -> bool {
        self.0.contains(path)
    }
}

// ─── HardLinkTracker ─────────────────────────────────────────────────────────

/// A deferred hardlink whose target is expected to appear in the output.
///
/// Emitted after all layers are processed, once the target path can be
/// confirmed as present in the emitted-path set. If the target was suppressed
/// or never appeared, the link is silently dropped.
#[derive(Debug)]
pub struct HardLinkEntry {
    /// The path of the hardlink itself.
    pub link_path: PathBuf,
    /// The path this hardlink points to.
    pub target_path: PathBuf,
    /// Layer index the hardlink was declared in, used for ordering at emit
    /// time.
    pub layer_index: usize,
    /// Header captured from the original hardlink entry, used for inode
    /// metadata (mode, uid, gid, mtime) when emitting.
    pub canonical: CanonicalTarHeader,
}

/// A hardlink whose target was suppressed by a higher-layer whiteout but
/// whose link path itself is live and must be promoted to a regular file.
///
/// `file_data` is populated either immediately (same-layer case: the
/// suppressed regular file appeared before the hardlink in the same tar
/// archive) or lazily (cross-layer case: the suppressed file is encountered
/// in an older layer, after the hardlink's layer was already processed).
///
/// If `file_data` is still `None` after all layers are processed, the
/// promotion is silently dropped — the target never existed in any layer,
/// which indicates a malformed image.
#[derive(Debug)]
pub struct PromotionEntry {
    /// The path of the surviving hardlink, which will be emitted as a regular
    /// file.
    pub link_path: PathBuf,
    /// The path of the suppressed target, used as a lookup key when buffered
    /// content arrives.
    pub target_path: PathBuf,
    /// Layer index the hardlink was declared in.
    pub layer_index: usize,
    /// Header and file bytes from the suppressed regular file. `None` until
    /// the suppressed file's content is encountered and attached via
    /// [`HardLinkTracker::note_suppressed_file`].
    pub file_data: Option<(CanonicalTarHeader, Vec<u8>)>,
}

/// Tracks deferred hardlinks and hardlink promotions across the layer merge.
///
/// Because layers are processed newest-first, a hardlink's target may not
/// have been seen yet when the hardlink itself is encountered. All hardlinks
/// are therefore deferred into one of two buckets:
///
/// - **Normal deferred** ([`HardLinkEntry`]): the target is live (not
///   suppressed). Emitted after all layers, once the target's presence in
///   the output can be confirmed.
/// - **Promotion** ([`PromotionEntry`]): the target was suppressed by a
///   whiteout. The link path is emitted as a standalone regular file using
///   the suppressed target's buffered content. See
///   [`HardLinkTracker::record_promotion`] and [`HardLinkTracker::note_suppressed_file`].
#[derive(Debug, Default)]
pub struct HardLinkTracker {
    deferred: Vec<HardLinkEntry>,
    promotions: Vec<PromotionEntry>,
    /// Content buffered from suppressed regular files during the current
    /// layer, keyed by normalised path. Consulted by
    /// [`record_promotion`](HardLinkTracker::record_promotion) to attach file
    /// data to a promotion immediately when the regular file and its hardlink
    /// appear in the same layer tar. Cleared by
    /// [`end_layer`](HardLinkTracker::end_layer).
    suppressed_content: HashMap<PathBuf, (CanonicalTarHeader, Vec<u8>)>,
}

impl HardLinkTracker {
    /// Record a normal deferred hardlink whose target is not suppressed.
    ///
    /// The link will be emitted by `emit_deferred` after all layers are
    /// processed, provided its target appears in the emitted-path set.
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

    /// Buffer the header and content of a suppressed regular file.
    ///
    /// Called by the merge engine when a regular file entry is suppressed by a
    /// whiteout. The content is kept because a hardlink to this path may still
    /// be live and require promotion.
    ///
    /// Two things happen:
    /// 1. Any existing [`PromotionEntry`] waiting on this path has its
    ///    `file_data` filled in immediately (cross-layer case: the hardlink
    ///    was in a newer layer, already processed).
    /// 2. The content is stored in `suppressed_content` so that a hardlink
    ///    appearing later in the *same* layer tar can find it via
    ///    [`record_promotion`](HardLinkTracker::record_promotion).
    pub fn note_suppressed_file(
        &mut self,
        path: PathBuf,
        canonical: CanonicalTarHeader,
        data: Vec<u8>,
    ) {
        for promo in &mut self.promotions {
            if promo.target_path == path && promo.file_data.is_none() {
                promo.file_data = Some((canonical.clone(), data.clone()));
            }
        }
        self.suppressed_content.insert(path, (canonical, data));
    }

    /// Record that `link_path` must be promoted to a regular file because its
    /// target (`target_path`) was suppressed by a whiteout.
    ///
    /// If the suppressed target's content has already been buffered (same-layer
    /// case), it is attached to the promotion entry immediately. Otherwise the
    /// entry is recorded with `file_data = None` and filled in later when
    /// [`note_suppressed_file`](HardLinkTracker::note_suppressed_file) is
    /// called for the target path.
    pub fn record_promotion(
        &mut self,
        link_path: PathBuf,
        target_path: PathBuf,
        layer_index: usize,
    ) {
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

    /// Discard the per-layer suppressed-file buffer.
    ///
    /// Must be called after each layer is fully processed. The buffer exists
    /// only to serve same-layer promotions: within a single tar archive, a
    /// regular file must precede any hardlinks to it, so once the layer is
    /// complete no further same-layer lookups are possible. Older layers
    /// cannot reference paths that only exist in newer ones, so the buffer
    /// has no value beyond the layer boundary and clearing it avoids
    /// accumulating memory across layers.
    pub fn end_layer(&mut self) {
        self.suppressed_content.clear();
    }

    /// Consume the tracker and return `(deferred, promotions)`, both sorted
    /// by ascending `layer_index`.
    ///
    /// Callers should emit promotions before normal deferred hardlinks: a
    /// deferred hardlink may target a path that was itself promoted from a
    /// suppressed regular file, and that path must be in the emitted-path set
    /// before the hardlink referencing it is processed.
    pub fn drain_sorted(mut self) -> (Vec<HardLinkEntry>, Vec<PromotionEntry>) {
        self.deferred.sort_by_key(|e| e.layer_index);
        self.promotions.sort_by_key(|e| e.layer_index);
        (self.deferred, self.promotions)
    }
}

//! Inode management and path mapping for the OverlayFS.
//!
//! This module provides a thread-safe store to manage the mapping between
//! FUSE inode numbers and their corresponding relative paths within the overlay.

use fuser::INodeNo;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Defines the strategy for generating inode numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeMode {
    /// Inodes are generated sequentially and are not preserved across mounts.
    Virtual,
    /// Inodes are generated via hashing to maintain consistency for the same path.
    Persistent,
}

/// Tracks a single inode's path and its active reference count from the kernel.
struct InodeEntry {
    /// The relative path within the overlay associated with this inode.
    path: PathBuf,
    /// The current number of active references (lookups) held by the FUSE kernel.
    lookup_count: u64,
}

/// Inner table holding both bidirectional maps, protected by a single `Mutex`.
///
/// Using a **single** lock guarantees that the two maps are always mutated
/// atomically, eliminating the TOCTOU race that existed when `map` and `paths`
/// were protected by independent `RwLock`s: two threads competing to allocate
/// an inode for the same (or hash-colliding) path could both observe `None` in
/// the first map and proceed to insert conflicting entries.
struct InodeTable {
    /// Mapping from Inode numbers to their relative filesystem paths.
    map: HashMap<INodeNo, InodeEntry>,
    /// Mapping from relative filesystem paths back to their assigned Inode numbers.
    paths: HashMap<PathBuf, INodeNo>,
}

/// A thread-safe store that maintains bidirectional mapping between Inodes and Paths.
///
/// It ensures that FUSE can look up file paths by their Inode numbers and vice versa,
/// while supporting different generation strategies through `InodeMode`.
pub struct InodeStore {
    /// Both maps behind a single lock so every read-modify-write is atomic.
    table: Mutex<InodeTable>,
    /// The generation strategy (Virtual or Persistent) for this store.
    mode: InodeMode,
    /// Atomic counter used for sequential Inode generation in `Virtual` mode.
    ///
    /// Kept outside the `Mutex` so that the next available counter-value can be
    /// pre-fetched without holding the lock (the actual insertion still happens
    /// inside the lock, so there is no double-allocation risk).
    next_ino: AtomicU64,
}

impl InodeStore {
    /// Initializes a new `InodeStore` with a predefined root inode (1).
    ///
    /// # Arguments
    /// * `mode` - The `InodeMode` strategy to use for new assignments.
    ///
    /// # Returns
    /// * A new instance of `InodeStore` with the root directory already mapped.
    pub fn new(mode: InodeMode) -> Self {
        let mut map = HashMap::new();
        let mut paths = HashMap::new();

        let root_ino = INodeNo(1);
        let root_path = PathBuf::from("");

        map.insert(
            root_ino,
            InodeEntry {
                path: root_path.clone(),
                lookup_count: 1,
            },
        );
        paths.insert(root_path, root_ino);

        Self {
            table: Mutex::new(InodeTable { map, paths }),
            mode,
            next_ino: AtomicU64::new(2),
        }
    }

    /// Retrieves the path associated with a specific inode number.
    ///
    /// # Arguments
    /// * `ino` - The inode number to look up.
    ///
    /// # Returns
    /// * `Some(PathBuf)` if the mapping exists, or `None` if not found.
    pub fn get_path(&self, ino: INodeNo) -> Option<PathBuf> {
        self.table
            .lock()
            .ok()?
            .map
            .get(&ino)
            .map(|e| e.path.clone())
    }

    /// Returns the inode number for a path **without** incrementing the lookup count.
    ///
    /// Use this variant in `readdir` / `readdirplus`, where we allocate inodes for
    /// directory entries, but the kernel has **not** issued a corresponding
    /// `FUSE_LOOKUP` request.  Calling `get_ino` in those paths would inflate
    /// `lookup_count` beyond what the kernel tracks, causing entries to be retained
    /// in the table long after the kernel has forgotten them.
    ///
    /// If the path has no existing mapping, a new inode is allocated with
    /// `lookup_count = 0` so it will be cleaned up on the first `forget`.
    pub fn peek_ino(&self, rel_path: &Path) -> INodeNo {
        let mut t = self.table.lock().unwrap();

        if let Some(&ino) = t.paths.get(rel_path) {
            return ino;
        }

        let new_ino = match self.mode {
            InodeMode::Virtual => INodeNo(self.next_ino.fetch_add(1, Ordering::Relaxed)),
            InodeMode::Persistent => self.generate_persistent_ino(&t, rel_path),
        };

        let path_buf = rel_path.to_path_buf();
        t.paths.insert(path_buf.clone(), new_ino);
        t.map.insert(
            new_ino,
            InodeEntry {
                path: path_buf,
                lookup_count: 0,
            },
        );
        new_ino
    }

    /// Retrieves an existing inode number or generates a new one for a given path.
    ///
    /// This implementation increments the `lookup_count` every time an inode is
    /// requested, ensuring the kernel's reference counting is synchronized with
    /// our internal store.
    ///
    /// # Arguments
    /// * `rel_path` - The relative path within the overlay to map.
    ///
    /// # Returns
    /// * The assigned `INodeNo` with an updated reference count.
    pub fn get_ino(&self, rel_path: &Path) -> INodeNo {
        let mut t = self.table.lock().unwrap();

        if let Some(&ino) = t.paths.get(rel_path) {
            if let Some(entry) = t.map.get_mut(&ino) {
                entry.lookup_count += 1;
            }
            return ino;
        }

        let new_ino = match self.mode {
            InodeMode::Virtual => INodeNo(self.next_ino.fetch_add(1, Ordering::Relaxed)),
            InodeMode::Persistent => self.generate_persistent_ino(&t, rel_path),
        };

        let path_buf = rel_path.to_path_buf();
        t.paths.insert(path_buf.clone(), new_ino);
        t.map.insert(
            new_ino,
            InodeEntry {
                path: path_buf,
                lookup_count: 1,
            },
        );

        new_ino
    }

    /// Internal helper to generate a persistent inode using FNV-1a hashing.
    ///
    /// # Arguments
    /// * `t` - A reference to the locked `InodeTable`.
    /// * `path` - The path to hash.
    ///
    /// # Returns
    /// * A unique `INodeNo` for the given path.
    fn generate_persistent_ino(&self, t: &InodeTable, path: &Path) -> INodeNo {
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in path.as_os_str().as_encoded_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }

        let base = if hash < 100 { hash + 100 } else { hash };
        let mut candidate = INodeNo(base);

        loop {
            match t.map.get(&candidate) {
                None => break,
                Some(existing) if existing.path == path => break,
                _ => candidate = INodeNo(candidate.0.wrapping_add(1)),
            }
        }
        candidate
    }

    /// Decrements the reference counts and removes the mapping if it reaches zero.
    ///
    /// # Arguments
    /// * `ino` - The inode number to forget.
    /// * `nlookup` - The number of references the kernel is releasing.
    ///
    /// # Returns
    /// * This function does not return a value.
    pub fn forget_ino(&self, ino: INodeNo, nlookup: u64) {
        if ino.0 == 1 {
            return;
        }
        let mut t = self.table.lock().unwrap();

        if let Some(entry) = t.map.get_mut(&ino) {
            if entry.lookup_count <= nlookup {
                let path = entry.path.clone();
                t.map.remove(&ino);
                t.paths.remove(&path);
            } else {
                entry.lookup_count -= nlookup;
            }
        }
    }

    /// Removes the inode mapping for a given path.
    ///
    /// Should be called after `unlink` or `rename` to prevent unbounded
    /// growth of the inode table over long sessions with many create/delete cycles.
    ///
    /// # Arguments
    /// * `rel_path` - The relative path whose inode entry should be removed.
    pub fn remove_ino(&self, rel_path: &Path) {
        let mut t = self.table.lock().unwrap();
        if let Some(ino) = t.paths.remove(rel_path) {
            t.map.remove(&ino);
        }
    }

    /// Removes the inode mapping for a path and all of its descendants.
    ///
    /// Should be called after `rmdir` to clean up any child inodes that were
    /// registered during the lifetime of the directory. Without this, a session
    /// with many mkdir/rmdir cycles would leak entries in the inode table.
    ///
    /// # Arguments
    /// * `rel_path` - The root path whose subtree should be purged.
    pub fn remove_subtree(&self, rel_path: &Path) {
        let prefix = rel_path.to_path_buf();
        let mut t = self.table.lock().unwrap();

        let to_remove: Vec<PathBuf> = t
            .paths
            .keys()
            .filter(|p| {
                *p == &prefix
                    || p.strip_prefix(&prefix)
                        .ok()
                        .map(|rest| rest.components().next().is_some())
                        .unwrap_or(false)
            })
            .cloned()
            .collect();

        for p in to_remove {
            if let Some(ino) = t.paths.remove(&p) {
                t.map.remove(&ino);
            }
        }
    }

    /// Constructs a full relative path for a child entry based on its parent's inode.
    ///
    /// # Arguments
    /// * `parent` - The inode number of the parent directory.
    /// * `name` - The name of the child entry (file or subdirectory).
    ///
    /// # Returns
    /// * A `PathBuf` representing the joined path of the child.
    ///
    /// # Note
    /// Only single-component names are accepted. Multi-component or rooted names
    /// (e.g., paths containing `/`) are rejected, and the parent path is returned
    /// unchanged, matching the FUSE contract that `name` is always a single
    /// directory entry name, never a full path.
    pub fn child_path(&self, parent: INodeNo, name: &OsStr) -> PathBuf {
        let parent_path = self.get_path(parent).unwrap_or_default();
        let name_path = Path::new(name);

        let mut comps = name_path.components();

        match (comps.next(), comps.next()) {
            (Some(Component::Normal(s)), None) => parent_path.join(s),
            _ => parent_path,
        }
    }
}

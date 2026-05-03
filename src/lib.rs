//! OverlayFS FUSE Implementation
//!
//! This crate provides a FUSE-based overlay filesystem structure
//! with automated layer management.

mod files;
mod filter;
mod fuse;
mod inode;
mod layers;
mod overlay;

/// Defines how inodes are handled, including mapping and generation strategies
/// between the underlying storage and the virtual filesystem.
pub use inode::InodeMode;

/// Commit-time filtering for rootfs-aware overlays.
/// Use [`CommitFilter::rootfs()`] as the starting point when the overlay wraps
/// a Linux rootfs managed by `bwrap` or `proot`, then chain builder calls to
/// add project-specific exclusions.
pub use filter::CommitFilter;

/// Core components for managing the overlay lifecycle, including
/// the main filesystem structure, handles, and supported actions.
pub use overlay::{OverlayAction, OverlayFS};

#[cfg(test)]
mod tests;

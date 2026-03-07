//! OverlayFS FUSE Implementation
//!
//! This crate provides a FUSE-based overlay filesystem structure
//! with automated layer management.

mod files;
mod fuse_ops;
mod inode;
mod layers;
mod overlay;

/// Defines how inodes are handled, including mapping and generation strategies
/// between the underlying storage and the virtual filesystem.
pub use inode::InodeMode;


/// Core components for managing the overlay lifecycle, including
/// the main filesystem structure, handles, and supported actions.
pub use overlay::{OverlayAction, OverlayFS};


#[cfg(test)]
mod tests;

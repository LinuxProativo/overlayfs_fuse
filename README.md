<p align="center">
  <img src="logo.png" alt="ALPack" width="300"/>
</p>

<h1 align="center">OverlayFS Fuse - A FUSE-based Overlay Filesystem</h1>
<h3 align="center">Lightweight FUSE Overlay Filesystem for Portable Linux Environments.</h1>

<p align="center">
    <img src="https://img.shields.io/badge/Platform-POSIX-FCC624?&logo=linux&style=flat-square"/>
    <img src="https://img.shields.io/github/actions/workflow/status/LinuxProativo/overlayfs_fuse/rust.yml?label=Test&style=flat-square&logo=github"/>
    <img src="https://img.shields.io/badge/RustC-1.85+-orange?style=flat-square&logo=rust"/>
    <img src="https://img.shields.io/github/languages/code-size/LinuxProativo/overlayfs_fuse?style=flat-square&logo=rust&label=Code%20Size"/>
</p>

## Overview

A FUSE-based overlay filesystem written in Rust that stacks a read-only lower layer
under a read-write upper layer, exposing a unified mount point with full Copy-on-Write
(CoW) semantics, whiteout-based deletion, and flexible commit strategies.

**OverlayFS-Fuse** provides a simple and robust way to create
**ephemeral writable environments over immutable directories**.

It is designed for scenarios such as:

* 📦 Portable Linux application packaging;

* 🛠️ Temporary build environments;

* 🧩 AppImage-style runtime overlays;

* 🧱 Container-like rootfs sessions without full container runtimes;

* 🔒 Safe modification layers over read-only systems;

Unlike kernel OverlayFS, this implementation runs entirely in
**userspace via FUSE**, making it usable without special kernel privileges
in many environments.

The system works by combining three layers:

| Layer          | Role                                    |
| -------------- | --------------------------------------- |
| **Lower**      | Original read-only filesystem           |
| **Upper**      | Writable layer containing modifications |
| **Mountpoint** | Unified view exposed to applications    |

All writes occur in the **upper layer**, preserving the original filesystem
intact until an explicit **commit** is requested.

## ✨ Features

- 📝 **Copy-on-Write**  
  Writes are promoted to the upper layer; the lower layer is never modified.

- 🗑️ **Whiteout support**  
  Deletions and renames create `.wh.<name>` markers to hide lower-layer entries.

- 🔄 **Four finalization strategies**  
  `Preserve`, `Discard`, `Commit`, and `CommitAtomic` (crash-safe backup/swap merge).

- 🔗 **Symlink-aware**  
  Dangling symlinks in the lower layer are tolerated; CoW never follows symlinks when copying.

- 🧬 **Extended attribute preservation**  
  `xattrs`, permissions, ownership, and timestamps are carried through CoW and commits.

- 🧮 **Dual inode modes**  
  `Virtual` (sequential, ephemeral) or `Persistent` (FNV-1a hash, stable across remounts).

- 🔒 **Thread-safe inode store**  
  Single-`Mutex` design eliminates TOCTOU races in concurrent lookup/allocation.

- 🧾 **Content-based deduplication on commit**  
  Files are compared by size, mtime, and BLAKE3 hash before overwriting.


## 🚀 Quick Start

```rust
use overlay_fuse::{OverlayFS, OverlayAction};
use std::path::PathBuf;

// 1. Create an overlay over an existing directory.
let mut overlay = OverlayFS::new(PathBuf::from("/path/to/lower"));

// 2. Mount — changes go to <lower>.upper, visible at <lower>.mountpoint
overlay.mount().expect("mount failed");

// 3. Use the mount point like any normal directory.
let mp = overlay.handle().mount_point().to_path_buf();
std::fs::write(mp.join("hello.txt"), "world").unwrap();

// 4. Unmount and decide what to do with the changes.
overlay.umount();
overlay.overlay_action(OverlayAction::Commit); // merges upper → lower
```

### 📁 Custom upper path

```rust
let mut overlay = OverlayFS::new(PathBuf::from("/data/base"));
overlay.set_upper(PathBuf::from("/data/session-changes"));
overlay.mount().unwrap();
```

### 🧱 Persistent inodes

```rust
use overlay_fuse::InodeMode;

let mut overlay = OverlayFS::new(PathBuf::from("/opt/rootfs"));
overlay.set_inode_mode(InodeMode::Persistent);
overlay.mount().unwrap();
```

## 📚 OverlayAction Reference

| Action | Behaviour |
|---|---|
| `Preserve` | Upper layer kept on disk; mount point removed. |
| `Discard` | Upper layer and mount point deleted entirely. |
| `Commit` | Upper merged into lower (whiteouts processed); both cleaned up. |
| `CommitAtomic` | Backup-and-swap merge; upper removed only after successful write. |

## 📂 Path Conventions 

Given `OverlayFS::new(PathBuf::from("/base/lower"))`:

| Layer | Default path |
|---|---|
| Lower (read-only) | `/base/lower` |
| Upper (read-write) | `/base/lower.upper` |
| Mount point | `/base/lower.mountpoint` |

Override the upper path with `set_upper()` before calling `mount()`.

## 📝 Project Structure 

```
src/
├── lib.rs        # Public API re-exports
├── overlay.rs    # OverlayFS controller, commit strategies, xattr utilities
├── layers.rs     # LayerManager: resolve, CoW, whiteout management
├── inode.rs      # InodeStore: thread-safe inode ↔ path mapping
├── files.rs      # OverlayFiles: layer path configuration
└── fuse_ops.rs   # FUSE operation handlers
```

## 📜 MIT License

This repository has scripts that were created to be free software.
Therefore, they can be distributed and/or modified within the terms of the ***MIT License***.

> ### See the [LICENSE](LICENSE) file for details.

## 📬 Contact & Support

* 📧 **Email:** [m10ferrari1200@gmail.com](mailto:m10ferrari1200@gmail.com)
* 📧 **Email:** [contatolinuxdicaspro@gmail.com](mailto:contatolinuxdicaspro@gmail.com)

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

## 🔍 Overview

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

## 🚀 Full Lifecycle Example

This example demonstrates the complete flow: configuring, mounting, interacting
with the filesystem, and finally committing the changes back to the base directory.

```rust
use overlay_fuse::{OverlayFS, OverlayAction};
use std::path::PathBuf;

// 1. Create an overlay over an existing directory.
let mut overlay = OverlayFS::new(PathBuf::from("/path/to/lower")).mountpoint_as_home();

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

You can override the default storage for modifications. This is useful for redirecting
writes to a specific persistent disk or a custom session folder.

```rust
let mut overlay = OverlayFS::new(PathBuf::from("/data/base"));
overlay.set_upper(PathBuf::from("/data/session-changes"));
overlay.mount().unwrap();
```

### 🧱 Persistent inodes

By default, inodes are virtual and ephemeral. Use Persistent mode if your application
relies on stable inode numbers across different sessions (e.g., for some database
engines or backup tools).

```rust
use overlay_fuse::InodeMode;

let mut overlay = OverlayFS::new(PathBuf::from("/opt/rootfs"));
overlay.set_inode_mode(InodeMode::Persistent);
overlay.mount().unwrap();
```

### 🛠️ State Safety & Prevention

OverlayFS prevents configuration changes after the filesystem is already live. This
safeguard ensures that your mount point and inode logic remain consistent.

```rust
let mut overlay = OverlayFS::new(PathBuf::from("/data/base"));
overlay.mount().expect("Initial mount failed");

// The following will PANIC at runtime to prevent configuration drift:
// overlay.set_upper(PathBuf::from("/new/path")); 
// overlay.mountpoint_as_home();
```

### 🏠 Environment-Aware Mounting (Fluent API)

Use mountpoint_as_home() to automatically redirect the mount point to the user's local
cache. This is ideal for environments where writing to /tmp is restricted or where
you want to keep the host system clean.

```rust
use overlay_fuse::InodeMode;

let mut overlay = OverlayFS::new(PathBuf::from("/usr/lib/myapp"))
    .mountpoint_as_home()           // Relocates to ~/.cache/mount_...
    .set_inode_mode(InodeMode::Persistent)
    .set_upper(PathBuf::from("/my/custom/upper"));

overlay.mount().expect("Ensure fuse3 is installed and ~/.cache is writable");
```

### 🧪 Parallel Testing Support

The new naming convention uses session IDs and timestamps, allowing multiple instances
of the same application or parallel tests to run without directory collisions.

```rust
// Each instance automatically generates a unique path, for example:
// Instance 1: /tmp/mount_myapp_a1b2c3d4_1715631234567890123
// Instance 2: /tmp/mount_myapp_e5f6g7h8_1715631234567999999
let mut inst1 = OverlayFS::new(PathBuf::from("/app"));
let mut inst2 = OverlayFS::new(PathBuf::from("/app"));

inst1.mount().unwrap();
inst2.mount().unwrap(); // No "Directory already exists" error!
```

## 📚 OverlayAction Reference

| Action | Behaviour |
|---|---|
| `Preserve` | Upper layer kept on disk; mount point removed. |
| `Discard` | Upper layer and mount point deleted entirely. |
| `Commit` | Upper merged into lower (whiteouts processed); both cleaned up. |
| `CommitAtomic` | Backup-and-swap merge; upper removed only after successful write. |

## 📂 Path Conventions 

Given `OverlayFS::new(PathBuf::from("/base/dir"))`:

| Layer | Default path | Description |
|---|---|---|
| **Lower (Read-Only)** | `/base/dir` | The original base directory. |
| **Upper (Read-Write)** | `/base/dir_upper` | Stores modifications (no longer a hidden dot-folder). |
| **Mount Point** | `/tmp/mount_dir_id_ts` | Unique, collision-resistant path in the system temp folder. |
| **Mount Point as Home** | `~/.cache/mount_dir_id_ts` | Used if `.mountpoint_as_home()` is enabled. |

### ⚠️ Configuration Rules

- 📌 **Pre-mount only:** You must call `set_upper()`, `set_inode_mode()`, or `mountpoint_as_home()` **before** calling `mount()`. 
- 📌 **State Protection:** To prevent data corruption, these methods will panic if they detect the filesystem is already mounted.
- 📌 **Automatic Cleanup:** The `mount_point` directory is automatically created on `mount()` and removed on `umount()` or when the object is dropped.

## 🛡️ Safety & Error Prevention

The version 1.1.0 introduces strict state management to prevent common development errors:

* 🔒 **Mount State Protection**  
  Configuration methods like `set_upper()`, `set_inode_mode()`, and `mountpoint_as_home()`
  now check the real mount state via `is_mounted()`. They will **panic** if called while
  the filesystem is active to prevent configuration drift and data corruption.

* ✅ **Collision Avoidance**  
  The default mount point names now include a unique session ID and a nanosecond timestamp
  (e.g., `mount_name_session_ts`). This allows multiple instances of the same application
  to run in parallel without `Permission Denied` or directory conflicts.

* 🗑️ **Automatic Directory Cleanup**  
  When `umount()` is called or the `OverlayFS` object is dropped, the temporary mount
  point directory is automatically removed from the host system.

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

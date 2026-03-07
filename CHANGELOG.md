# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [1.0.0] — Initial Release

### Added

#### Core Overlay Filesystem (`overlay.rs`)
- `OverlayFS` struct as the primary controller for the FUSE-based overlay lifecycle.
- `mount()` / `umount()` methods with graceful session teardown and libc fallback (`MNT_DETACH`).
- `overlay_action()` supporting four finalization strategies via `OverlayAction`:
  - `Preserve` — retains the upper layer as-is after unmount.
  - `Discard` — removes the upper layer and mount point entirely.
  - `Commit` — merges upper layer changes back into the lower layer, then cleans up.
  - `CommitAtomic` — performs a backup-and-swap merge to guarantee data integrity on crash.
- `set_upper()` and `set_inode_mode()` for custom configuration before mounting.
- `OverlayHandle` struct providing read-only access to `lower`, `upper`, and `mount_point` paths.
- `copy_xattrs()` to preserve extended attributes during Copy-on-Write promotions and commits.
- `copy_if_different()` with BLAKE3-based content comparison and per-file `fsync` for durability on loop devices.

#### Layer Management (`layers.rs`)
- `LayerManager` coordinating read resolution and write promotion between lower and upper layers.
- `resolve()` — finds the physical path for a relative overlay path, checking upper before lower, respecting whiteouts, and handling dangling symlinks correctly via `symlink_metadata`.
- `is_hidden()` — checks for whiteout markers (`.wh.<name>`) in the upper layer.
- `create_whiteout()` / `clear_whiteout()` — manages whiteout files to mask deletions.
- `copy_on_write()` — promotes files, symlinks, and directories from lower to upper, preserving permissions, ownership (UID/GID), timestamps, and xattrs.
- `resolve_symlink_safe()` — recursive symlink resolution with a depth cap (`MAX_SYMLINK_DEPTH = 40`) to prevent `ELOOP`.

#### Inode Management (`inode.rs`)
- `InodeStore` — thread-safe bidirectional map between `INodeNo` and relative paths.
- Two inode generation strategies via `InodeMode`:
  - `Virtual` — sequential counter, ephemeral across mounts.
  - `Persistent` — FNV-1a hash-based, deterministic across mounts for the same path.
- Single-`Mutex` design on `InodeTable` eliminates TOCTOU races that existed with independent `RwLock`s.
- `get_ino()` — retrieves or allocates an inode, incrementing the kernel lookup reference count.
- `forget_ino()` — decrements reference counts and evicts the entry when it reaches zero.
- `remove_ino()` / `remove_subtree()` — explicit cleanup after `unlink` and `rmdir` to prevent table growth.
- `child_path()` — constructs child paths from a parent inode and a single-component name, enforcing the FUSE contract.

#### File Path Management (`files.rs`)
- `OverlayFiles` struct tracking `lower`, `upper`, and `mount_point` paths.
- Automatic derivation of `upper` and `mount_point` paths from the lower directory name (`<name>.upper`, `<name>.mountpoint`).

#### Public API (`lib.rs`)
- Re-exports `InodeMode`, `OverlayAction`, and `OverlayFS` as the stable public surface.

### Tests (`tests.rs`)
- Full lifecycle test: mount, write, unmount, discard.
- Custom upper path and inode mode configuration.
- `Commit` and `CommitAtomic` merge strategies, including whiteout/deletion handling.
- Copy-on-Write correctness: lower layer must remain unmodified after writes through the mount.
- Rename from lower: whiteout created at old path, file appears at new path in upper.
- Symlink visibility, CoW preservation (symlink stays a symlink in upper), and dangling symlink tolerance.
- Whiteout creation and file hiding after `unlink`.
- `rmdir` returning `ENOTEMPTY` on non-empty directories; success after manual emptying.
- `InodeStore` unit tests: stability, distinct-path uniqueness, `Persistent` determinism across instances, collision avoidance, `remove_ino` reassignment, `remove_subtree` cleanup, and concurrent `get_ino` safety under 16 threads.

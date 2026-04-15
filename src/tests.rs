use super::*;
use crate::inode::{InodeMode, InodeStore};
use crate::InodeMode::Persistent;
use std::fs;
use std::os::unix;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(name);
    let _ = fs::remove_dir_all(&p);
    p
}

fn which_proot() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let local = std::path::PathBuf::from(home).join(".local/bin/proot");
        if local.is_file() {
            return Some(local);
        }
    }

    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join("proot");
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

#[test]
fn test_overlay_full_lifecycle() {
    let tmp_dir = tmp("overlay_lifecycle_test");
    let lower_path = tmp_dir.join("lower");
    fs::create_dir_all(&lower_path).unwrap();
    fs::write(lower_path.join("base.txt"), "original content").unwrap();

    let mut overlay = OverlayFS::new(lower_path.clone());
    let mount_point = overlay.handle().mount_point().to_path_buf();
    let upper_path = overlay.handle().upper().to_path_buf();

    overlay.mount().expect("Failed to mount FUSE");

    assert!(mount_point.join("base.txt").exists());

    fs::write(mount_point.join("new.txt"), "new data").unwrap();

    overlay.umount();

    assert!(!mount_point.join("new.txt").exists());
    assert!(upper_path.join("new.txt").exists());

    overlay.overlay_action(OverlayAction::Discard);
    assert!(!upper_path.exists());

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_custom_upper_path() {
    let tmp = tmp("custom_test");
    let lower = tmp.join("lower");
    let custom_upper = tmp.join("my_persistent_changes");

    fs::create_dir_all(&lower).unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.set_upper(custom_upper.clone());
    overlay.set_inode_mode(InodeMode::Virtual);

    overlay.mount().expect("Failed to mount custom upper");
    fs::write(overlay.handle().mount_point().join("note.txt"), "session 1").unwrap();
    overlay.umount();

    assert!(custom_upper.join("note.txt").exists());

    let overlay_default = OverlayFS::new(lower);
    assert_eq!(overlay_default.handle().upper(), tmp.join("lower_upper"));
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_overlay_commit() {
    let tmp = tmp("commit_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();
    fs::write(lower.join("old.txt"), "old").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().expect("Failed to mount FUSE");

    fs::write(
        overlay.handle().mount_point().join("new_on_mount.txt"),
        "new data",
    )
    .unwrap();

    overlay.umount();
    overlay.overlay_action(OverlayAction::Commit);

    let final_file = lower.join("new_on_mount.txt");
    assert!(final_file.exists());
    assert_eq!(fs::read_to_string(final_file).unwrap(), "new data");
    assert!(!overlay.handle().upper().exists());
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_overlay_commit_with_deletion() {
    let tmp = tmp("commit_del_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();
    fs::write(lower.join("to_delete.txt"), "delete me").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();
    fs::remove_file(overlay.handle().mount_point().join("to_delete.txt")).unwrap();
    overlay.umount();
    overlay.overlay_action(OverlayAction::Commit);

    assert!(!lower.join("to_delete.txt").exists());
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_inode_virtual_stability() {
    let store = InodeStore::new(InodeMode::Virtual);
    let path = Path::new("a/b/c");

    let ino1 = store.get_ino(path);
    let ino2 = store.get_ino(path);

    assert_eq!(ino1, ino2, "mesmo path deve retornar mesmo inode");
}

#[test]
fn test_inode_virtual_distinct_paths() {
    let store = InodeStore::new(InodeMode::Virtual);

    let a = store.get_ino(Path::new("foo"));
    let b = store.get_ino(Path::new("bar"));

    assert_ne!(a, b);
}

#[test]
fn test_inode_persistent_deterministic() {
    let path = Path::new("usr/bin/env");

    let ino1 = InodeStore::new(Persistent).get_ino(path);
    let ino2 = InodeStore::new(Persistent).get_ino(path);

    assert_eq!(
        ino1, ino2,
        "modo Persistent deve ser determinístico entre instâncias"
    );
}

#[test]
fn test_inode_persistent_no_collision() {
    let store = InodeStore::new(Persistent);

    let a = store.get_ino(Path::new("etc/passwd"));
    let b = store.get_ino(Path::new("etc/shadow"));

    assert_ne!(a, b);
}

#[test]
fn test_inode_remove_and_reassign() {
    let store = InodeStore::new(InodeMode::Virtual);
    let path = Path::new("tmp/file");

    let original = store.get_ino(path);
    store.remove_ino(path);
    let reassigned = store.get_ino(path);

    assert_ne!(original, reassigned);
    assert_eq!(store.get_path(reassigned), Some(path.to_path_buf()));
    assert_eq!(store.get_path(original), None);
}

#[test]
fn test_inode_remove_subtree() {
    let store = InodeStore::new(InodeMode::Virtual);

    let root = Path::new("dir");
    let child = Path::new("dir/file");
    let deep = Path::new("dir/sub/deep");

    let ino_root = store.get_ino(root);
    let ino_child = store.get_ino(child);
    let ino_deep = store.get_ino(deep);

    store.remove_subtree(root);

    assert_eq!(store.get_path(ino_root), None);
    assert_eq!(store.get_path(ino_child), None);
    assert_eq!(store.get_path(ino_deep), None);
}

#[test]
fn test_inode_concurrent_get_ino() {
    use std::sync::Arc;

    let store = Arc::new(InodeStore::new(Persistent));
    let path = Path::new("concurrent/path");
    let expected = store.get_ino(path);

    let handles: Vec<_> = (0..16)
        .map(|_| {
            let store = Arc::clone(&store);
            let path = path.to_path_buf();
            std::thread::spawn(move || store.get_ino(&path))
        })
        .collect();

    for h in handles {
        assert_eq!(h.join().unwrap(), expected);
    }
}

#[test]
fn test_symlink_visible_from_lower() {
    let tmp = tmp("symlink_lower_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();

    fs::write(lower.join("target.txt"), "hello").unwrap();
    unix::fs::symlink("target.txt", lower.join("link.txt")).unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    let via_mount = overlay.handle().mount_point().join("link.txt");
    assert!(via_mount.exists(), "symlink deve ser visível pelo mount");
    assert_eq!(fs::read_to_string(&via_mount).unwrap(), "hello");

    overlay.umount();
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_symlink_cow_stays_as_symlink() {
    let tmp = tmp("symlink_cow_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();
    fs::write(lower.join("real.txt"), "real").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    let mount = overlay.handle().mount_point().to_path_buf();
    unix::fs::symlink("real.txt", mount.join("mylink")).unwrap();

    overlay.umount();

    let upper_link = overlay.handle().upper().join("mylink");
    assert!(
        fs::symlink_metadata(&upper_link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "upper deve conter o symlink, não o conteúdo do target"
    );
    assert_eq!(
        fs::read_link(&upper_link).unwrap().to_str().unwrap(),
        "real.txt"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_dangling_symlink_in_lower() {
    let tmp = tmp("dangling_symlink_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();

    unix::fs::symlink("/nonexistent/path", lower.join("dangling")).unwrap();
    fs::write(lower.join("normal.txt"), "ok").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    let mount = overlay.handle().mount_point().to_path_buf();

    let entries: Vec<_> = fs::read_dir(&mount)
        .expect("readdir não deve falhar")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    assert!(entries.contains(&"normal.txt".to_string()));
    assert!(entries.contains(&"dangling".to_string()));
    assert_eq!(fs::read_to_string(mount.join("normal.txt")).unwrap(), "ok");

    overlay.umount();
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_whiteout_hides_lower_file() {
    let tmp = tmp("whiteout_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();
    fs::write(lower.join("ghost.txt"), "boo").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    let mount = overlay.handle().mount_point().to_path_buf();
    assert!(mount.join("ghost.txt").exists());
    fs::remove_file(mount.join("ghost.txt")).unwrap();
    assert!(
        !mount.join("ghost.txt").exists(),
        "arquivo deve estar oculto após unlink"
    );

    overlay.umount();

    let wh = overlay.handle().upper().join(".wh.ghost.txt");
    assert!(wh.exists(), "whiteout deve estar presente no upper");

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_rmdir_notempty() {
    let tmp = tmp("rmdir_notempty_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(lower.join("mydir")).unwrap();
    fs::write(lower.join("mydir/file.txt"), "content").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    let mount = overlay.handle().mount_point().to_path_buf();
    let result = fs::remove_dir(mount.join("mydir"));
    assert!(result.is_err(), "rmdir em diretório não-vazio deve falhar");

    overlay.umount();
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_rmdir_after_emptying() {
    let tmp = tmp("rmdir_empty_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(lower.join("mydir")).unwrap();
    fs::write(lower.join("mydir/file.txt"), "bye").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    let mount = overlay.handle().mount_point().to_path_buf();
    fs::remove_file(mount.join("mydir/file.txt")).unwrap();
    fs::remove_dir(mount.join("mydir")).expect("rmdir deve funcionar após esvaziar");

    assert!(!mount.join("mydir").exists());

    overlay.umount();
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_commit_atomic_basic() {
    let tmp = tmp("commit_atomic_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();
    fs::write(lower.join("existing.txt"), "old").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    fs::write(overlay.handle().mount_point().join("new.txt"), "atomic").unwrap();

    overlay.umount();
    overlay.overlay_action(OverlayAction::CommitAtomic);

    assert!(lower.join("new.txt").exists());
    assert_eq!(fs::read_to_string(lower.join("new.txt")).unwrap(), "atomic");
    assert!(
        lower.join("existing.txt").exists(),
        "arquivos existentes devem ser preservados"
    );
    assert!(
        !overlay.handle().upper().exists(),
        "upper deve ser removido após CommitAtomic"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_commit_atomic_with_deletion() {
    let tmp = tmp("commit_atomic_del_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();
    fs::write(lower.join("remove_me.txt"), "gone").unwrap();
    fs::write(lower.join("keep_me.txt"), "stay").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    fs::remove_file(overlay.handle().mount_point().join("remove_me.txt")).unwrap();

    overlay.umount();
    overlay.overlay_action(OverlayAction::CommitAtomic);

    assert!(!lower.join("remove_me.txt").exists());
    assert!(lower.join("keep_me.txt").exists());

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_cow_does_not_modify_lower() {
    let tmp = tmp("cow_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();
    fs::write(lower.join("data.txt"), "original").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    fs::write(overlay.handle().mount_point().join("data.txt"), "modified").unwrap();

    overlay.umount();

    assert_eq!(
        fs::read_to_string(lower.join("data.txt")).unwrap(),
        "original"
    );

    assert_eq!(
        fs::read_to_string(overlay.handle().upper().join("data.txt")).unwrap(),
        "modified"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_rename_from_lower() {
    let tmp = tmp("rename_test");
    let lower = tmp.join("lower");
    fs::create_dir_all(&lower).unwrap();
    fs::write(lower.join("old_name.txt"), "content").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.mount().unwrap();

    let mount = overlay.handle().mount_point().to_path_buf();
    fs::rename(mount.join("old_name.txt"), mount.join("new_name.txt")).unwrap();

    assert!(!mount.join("old_name.txt").exists());
    assert!(mount.join("new_name.txt").exists());
    assert_eq!(
        fs::read_to_string(mount.join("new_name.txt")).unwrap(),
        "content"
    );

    overlay.umount();

    let upper = overlay.handle().upper().to_path_buf();
    assert!(
        upper.join(".wh.old_name.txt").exists(),
        "whiteout deve existir para o nome antigo"
    );
    assert!(upper.join("new_name.txt").exists());

    let _ = fs::remove_dir_all(&tmp);
}

fn mkfile(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, content).unwrap();
    p
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn test_filter_skip_dirs_single_and_batch() {
    let filter = CommitFilter::new()
        .skip_dir("/dev")
        .skip_dirs(["/proc", "/sys"]);

    assert!(filter.should_skip(Path::new("dev"), Path::new("/upper/dev")));
    assert!(filter.should_skip(Path::new("proc"), Path::new("/upper/proc")));
    assert!(filter.should_skip(Path::new("sys"), Path::new("/upper/sys")));
    assert!(filter.should_skip(Path::new("dev/null"), Path::new("/upper/dev/null")));
    assert!(filter.should_skip(Path::new("proc/1/maps"), Path::new("/upper/proc/1/maps")));
    assert!(!filter.should_skip(Path::new("devices"), Path::new("/upper/devices")));
    assert!(!filter.should_skip(Path::new("etc/passwd"), Path::new("/upper/etc/passwd")));
}

#[test]
fn test_filter_skip_files_single_and_batch() {
    let filter = CommitFilter::new()
        .skip_file("lost+found")
        .skip_files(["__pycache__", ".DS_Store"]);

    assert!(filter.should_skip(Path::new("lost+found"), Path::new("/upper/lost+found")));
    assert!(filter.should_skip(
        Path::new("usr/lib/__pycache__"),
        Path::new("/upper/usr/lib/__pycache__")
    ));
    assert!(filter.should_skip(
        Path::new("home/user/.DS_Store"),
        Path::new("/upper/home/user/.DS_Store")
    ));
    assert!(!filter.should_skip(
        Path::new("not_lost+found"),
        Path::new("/upper/not_lost+found")
    ));
    assert!(!filter.should_skip(Path::new("etc/passwd"), Path::new("/upper/etc/passwd")));
}

#[test]
fn test_filter_skip_zero_permissions() {
    let tmp = tmp("filter_zero_perm_test");
    fs::create_dir_all(&tmp).unwrap();

    let zero_file = mkfile(&tmp, "stub", b"");
    let normal_file = mkfile(&tmp, "normal", b"data");
    chmod(&zero_file, 0o000);
    chmod(&normal_file, 0o644);

    let filter = CommitFilter::new().skip_zero_permissions(true);

    assert!(filter.should_skip(Path::new("stub"), &zero_file));
    assert!(!filter.should_skip(Path::new("normal"), &normal_file));

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_filter_skip_empty_files_in() {
    let tmp = tmp("filter_empty_files_test");

    let cache_dir = tmp.join("var/cache/apt");
    let etc_dir = tmp.join("etc");
    fs::create_dir_all(&cache_dir).unwrap();
    fs::create_dir_all(&etc_dir).unwrap();

    let empty_in_cache = mkfile(&cache_dir, "pkgcache.bin", b"");
    let nonempty_in_cache = mkfile(&cache_dir, "real.db", b"data");
    let empty_in_etc = mkfile(&etc_dir, "passwd", b"");
    let filter = CommitFilter::new().skip_empty_files_in("/var/cache");

    assert!(
        filter.should_skip(Path::new("var/cache/apt/pkgcache.bin"), &empty_in_cache),
        "zero-byte file inside skip_empty_files_in dir must be skipped"
    );
    assert!(
        !filter.should_skip(Path::new("var/cache/apt/real.db"), &nonempty_in_cache),
        "non-empty file inside skip_empty_files_in dir must be allowed"
    );
    assert!(
        !filter.should_skip(Path::new("etc/passwd"), &empty_in_etc),
        "zero-byte file outside skip_empty_files_in dir must be allowed"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_filter_commit_atomic_skips_dirs() {
    let tmp = tmp("filter_commit_atomic_test");
    let lower = tmp.join("lower");

    fs::create_dir_all(lower.join("etc")).unwrap();
    fs::create_dir_all(lower.join("dev")).unwrap();
    fs::write(lower.join("etc/os-release"), "ID=test").unwrap();

    let mut overlay = OverlayFS::new(lower.clone());
    overlay.set_commit_filter(CommitFilter::new().skip_dir("dev"));
    overlay.mount().expect("mount failed");

    let mount = overlay.handle().mount_point().to_path_buf();

    fs::write(mount.join("etc/hostname"), "testbox").unwrap();
    fs::write(mount.join("dev/null"), "").unwrap();

    overlay.umount();
    overlay.overlay_action(OverlayAction::CommitAtomic);

    assert_eq!(
        fs::read_to_string(lower.join("etc/hostname")).unwrap(),
        "testbox",
        "etc/hostname must be committed to lower"
    );
    assert!(
        !lower.join("dev/null").exists() || fs::read(lower.join("dev/null")).unwrap().is_empty(),
        "dev/null must not be committed to lower when dev is filtered"
    );
    assert_eq!(
        fs::read_to_string(lower.join("etc/os-release")).unwrap(),
        "ID=test",
        "pre-existing lower files must be preserved"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_proot_meu_rootfs() {
    test_proot_alpine_apk_add_inner(Some(Path::new("/tmp/ALPack")));
}

fn test_proot_alpine_apk_add_inner(rootfs_override: Option<&Path>) {
    let proot_bin = match which_proot() {
        Some(p) => p,
        None => {
            eprintln!("test_proot_alpine_apk_add: proot not found — skipping");
            return;
        }
    };

    let rootfs_buf;
    let rootfs: &Path = if let Some(r) = rootfs_override {
        r
    } else if let Some(v) = std::env::var_os("ALPINE_ROOTFS") {
        rootfs_buf = PathBuf::from(v);
        &rootfs_buf
    } else {
        rootfs_buf = {
            let mut p = std::env::current_exe().unwrap();
            p.pop();
            p.push("alpine-rootfs");
            p
        };
        &rootfs_buf
    };

    if !rootfs.join("etc/alpine-release").exists() {
        eprintln!(
            "test_proot_alpine_apk_add: Alpine rootfs not found at {:?} — skipping.\n\
             Extract a rootfs tarball there or set ALPINE_ROOTFS=<path>.",
            rootfs
        );
        return;
    }

    let mut overlay = OverlayFS::new(rootfs.to_path_buf());
    overlay.set_inode_mode(Persistent);
    overlay.mount().expect("OverlayFS mount failed");

    let mount_point = overlay.handle().mount_point().to_path_buf();
    let upper = overlay.handle().upper().to_path_buf();

    let sh = mount_point.join("bin/sh");
    let busybox = mount_point.join("bin/busybox");

    eprintln!("mount_point: {:?}", mount_point);
    eprintln!(
        "bin/sh exists (symlink_metadata): {}",
        fs::symlink_metadata(&sh).is_ok()
    );
    eprintln!(
        "bin/sh is_symlink: {}",
        sh.symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    );
    eprintln!("bin/sh read_link: {:?}", fs::read_link(&sh));
    eprintln!("bin/busybox exists: {}", busybox.exists());
    eprintln!(
        "bin/busybox symlink_metadata: {}",
        fs::symlink_metadata(&busybox).is_ok()
    );

    if let Ok(entries) = fs::read_dir(mount_point.join("bin")) {
        let mut found_sh = false;
        for e in entries.flatten() {
            if e.file_name() == "sh" {
                found_sh = true;
                eprintln!(
                    "  FOUND /bin/sh  type={:?}  metadata={:?}",
                    e.file_type(),
                    e.metadata()
                );
            }
        }
        if !found_sh {
            eprintln!("  /bin/sh NAO aparece no readdir do mountpoint");
        }
    }

    let lower_sh = rootfs.join("bin/sh");
    eprintln!(
        "lower bin/sh symlink_metadata: {}",
        fs::symlink_metadata(&lower_sh).is_ok()
    );
    eprintln!("lower bin/sh read_link: {:?}", fs::read_link(&lower_sh));

    let output = std::process::Command::new(&proot_bin)
        .arg("-0")
        .arg("-R")
        .arg(mount_point.to_str().unwrap())
        .args(["/bin/sh", "-c", "apk add --no-cache curl wget"])
        .output()
        .expect("failed to spawn proot");

    let lower_installed = rootfs.join("lib/apk/db/installed");
    let lower_before = fs::read(&lower_installed).unwrap_or_default();

    overlay.umount();

    if !output.status.success() {
        panic!(
            "apk add failed with: {}\n\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let lower_after = fs::read(&lower_installed).unwrap_or_default();
    assert_eq!(
        lower_before,
        lower_after,
        "lower layer was modified in place — CoW isolation broken
         lower_before ({} bytes) != lower_after ({} bytes)",
        lower_before.len(),
        lower_after.len(),
    );

    let installed_upper = upper.join("lib/apk/db/installed");
    if installed_upper.exists() {
        let upper_content = fs::read(&installed_upper).unwrap_or_default();
        if upper_content == lower_before {
            eprintln!(
                "WARN: upper/lib/apk/db/installed is identical to lower — apk may not have installed the package (already satisfied via busybox?). stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    let _ = fs::remove_dir_all(&upper);
    let _ = fs::remove_dir_all(&mount_point);
}

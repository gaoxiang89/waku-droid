//! Non-destructive, restartable first-run import. No runtime endpoint/token or
//! SQLite file is copied. The daemon snapshots SQLite separately.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

const MARKER: &str = ".waku-import-v1";
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::other(
            "import destination must not be a symbolic link",
        ));
    }
    Ok(())
}

/// The persistent lock file is intentional: deleting it permits two processes
/// to lock different inodes. OS locks are released even after a crashed import.
pub fn with_import_lock<T>(
    destination: &Path,
    action: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    private_directory(destination)?;
    let path = destination.join(".waku-import.lock");
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other("import lock must not be a symbolic link"));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let lock = options.open(path)?;
    lock.lock()?;
    action()
}

/// Publish a fully written file without ever replacing an existing target.
/// The temporary file is on the same filesystem; a crash may leave an unused
/// uniquely named temp file, never a partially published settings/blob file.
pub fn publish_missing(
    destination: &Path,
    write: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if fs::symlink_metadata(destination).is_ok() {
        return Ok(());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    private_directory(parent)?;
    let temporary = parent.join(format!(
        ".narel-import-{}-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos(),
        NEXT_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        write(&temporary)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&temporary)?
            .sync_all()?;
        match fs::hard_link(&temporary, destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    publish_missing(destination, |temporary| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options.open(temporary)?;
        io::copy(&mut File::open(source)?, &mut output)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(
                temporary,
                fs::Permissions::from_mode(
                    0o600 | (fs::metadata(source)?.permissions().mode() & 0o100),
                ),
            )?;
        }
        output.sync_all()
    })
}

fn copy_tree(source: &Path, destination: &Path, root: bool) -> io::Result<()> {
    private_directory(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        // Never revive a running Waku endpoint or copy a live SQLite/WAL pair.
        if (root
            && matches!(
                name_text.as_ref(),
                "app.db"
                    | "app.db-wal"
                    | "app.db-shm"
                    | "resident-daemon.json"
                    | "daemon.json"
                    | "bridge.json"
            ))
            || name_text.starts_with(".waku-import")
            || name_text.starts_with(".narel-import-")
        {
            continue;
        }
        let target = destination.join(name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if fs::symlink_metadata(&target)
                .is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink())
            {
                continue;
            }
            copy_tree(&entry.path(), &target, false)?;
        } else if kind.is_file() {
            copy_file(&entry.path(), &target)?;
        }
        // Do not follow symlinks, sockets, devices, or other special files.
    }
    Ok(())
}

/// Existing Narel files win, legacy files are never modified, and completion
/// is recorded only after the entire tree succeeds. Retry fills missing files.
pub fn import_directory(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::symlink_metadata(source) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(io::Error::other("legacy data root must be a directory"));
        }
        Ok(_) => {}
    }
    with_import_lock(destination, || {
        if destination.join(MARKER).is_file() {
            return Ok(());
        }
        copy_tree(source, destination, true)?;
        publish_missing(&destination.join(MARKER), |temporary| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(temporary)?;
            file.write_all(b"Imported missing Waku files; originals retained.\n")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "narel-import-test-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn empty_destination_migrates_once_without_clobber_or_deleting_the_source() {
        let root = fixture();
        let old = root.join("old");
        let new_dir = root.join("new");
        fs::create_dir_all(old.join("blobs")).unwrap();
        fs::write(old.join("settings.json"), "waku settings").unwrap();
        fs::write(old.join("connections.json"), "saved connections").unwrap();
        fs::write(old.join("blobs/attachment"), "attachment bytes").unwrap();

        import_directory(&old, &new_dir).unwrap();
        assert_eq!(
            fs::read_to_string(new_dir.join("settings.json")).unwrap(),
            "waku settings"
        );
        assert_eq!(
            fs::read_to_string(new_dir.join("connections.json")).unwrap(),
            "saved connections"
        );
        assert_eq!(
            fs::read_to_string(new_dir.join("blobs/attachment")).unwrap(),
            "attachment bytes"
        );
        assert_eq!(
            fs::read_to_string(old.join("settings.json")).unwrap(),
            "waku settings"
        );
        assert!(old.is_dir());

        fs::write(new_dir.join("settings.json"), "narel settings").unwrap();
        fs::write(old.join("settings.json"), "older waku settings").unwrap();
        fs::write(old.join("later.json"), "must not appear").unwrap();
        import_directory(&old, &new_dir).unwrap();
        assert_eq!(
            fs::read_to_string(new_dir.join("settings.json")).unwrap(),
            "narel settings"
        );
        assert!(
            !new_dir.join("later.json").exists(),
            "a second import is a no-op"
        );
        assert!(
            old.join("later.json").is_file(),
            "the legacy directory stays as the backup"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imports_missing_files_preserves_both_products_and_does_not_resurrect() {
        let root = fixture();
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir_all(old.join("blobs")).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("app.json"), "old settings").unwrap();
        fs::write(new.join("app.json"), "new settings").unwrap();
        fs::write(old.join("blobs/history"), "transcript blob").unwrap();
        fs::write(old.join("blobs/app.db"), "user file, not the root database").unwrap();
        fs::write(old.join("app.db"), "not a SQLite snapshot").unwrap();
        fs::write(old.join("resident-daemon.json"), "stale token").unwrap();
        import_directory(&old, &new).unwrap();
        assert_eq!(
            fs::read_to_string(new.join("app.json")).unwrap(),
            "new settings"
        );
        assert_eq!(
            fs::read_to_string(new.join("blobs/history")).unwrap(),
            "transcript blob"
        );
        assert!(old.join("blobs/history").is_file());
        assert!(!new.join("app.db").exists());
        assert!(new.join("blobs/app.db").is_file());
        assert!(!new.join("resident-daemon.json").exists());
        fs::remove_file(new.join("blobs/history")).unwrap();
        import_directory(&old, &new).unwrap();
        assert!(!new.join("blobs/history").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_publication_can_retry_without_exposing_partial_bytes() {
        let root = fixture();
        let target = root.join("settings.json");
        assert!(
            publish_missing(&target, |temporary| {
                fs::write(temporary, "partial")?;
                Err(io::Error::other("simulated interruption"))
            })
            .is_err()
        );
        assert!(!target.exists());
        publish_missing(&target, |temporary| fs::write(temporary, "complete")).unwrap();
        assert_eq!(fs::read_to_string(target).unwrap(), "complete");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_importers_publish_complete_files() {
        let root = fixture();
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("connections.json"), vec![42; 65536]).unwrap();
        let handles = (0..4)
            .map(|_| {
                let (old, new) = (old.clone(), new.clone());
                std::thread::spawn(move || import_directory(&old, &new).unwrap())
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            fs::read(new.join("connections.json")).unwrap(),
            vec![42; 65536]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn subprocess_import_helper() {
        let Some(root) = std::env::var_os("NAREL_IMPORT_TEST_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        import_directory(&root.join("old"), &root.join("new")).unwrap();
    }

    #[test]
    fn separate_processes_serialize_the_same_import() {
        let root = fixture();
        fs::create_dir_all(root.join("old")).unwrap();
        fs::write(root.join("old/connections.json"), vec![13; 65536]).unwrap();
        let children = (0..4)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("subprocess_import_helper")
                    .env("NAREL_IMPORT_TEST_ROOT", &root)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(
            fs::read(root.join("new/connections.json")).unwrap(),
            vec![13; 65536]
        );
        fs::remove_dir_all(root).unwrap();
    }
}

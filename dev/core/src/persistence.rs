//! Publish complete snapshots so reconnect never reads a half-written file.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".olea-{:032x}.tmp", rand::random::<u128>()));
    // create_new prevents overwriting another writer's temporary file.
    let mut file = OpenOptions::new().write(true).create_new(true).open(&temporary)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        // Same-directory rename replaces the destination without truncating it.
        // On Windows Rust uses MoveFileExW with REPLACE_EXISTING.
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_only_observe_complete_snapshots() {
        let directory = std::env::temp_dir().join(format!("olea-persistence-{:032x}", rand::random::<u128>()));
        let path = directory.join("settings.conf");
        let old = "version = 13\npreset = custom\n".repeat(256);
        let new = "version = 13\npreset = range\n".repeat(256);
        atomic_write(&path, old.as_bytes()).unwrap();
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                for _ in 0..300 {
                    let text = fs::read_to_string(&path).unwrap();
                    assert!(text == old || text == new, "reader saw a partial snapshot");
                }
            });
            for index in 0..30 {
                atomic_write(&path, if index % 2 == 0 { new.as_bytes() } else { old.as_bytes() }).unwrap();
            }
            reader.join().unwrap();
        });
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn failed_replacement_preserves_the_previous_file_and_removes_temporary() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = std::env::temp_dir().join(format!("olea-persistence-{:032x}", rand::random::<u128>()));
        let path = directory.join("bonds.conf");
        atomic_write(&path, b"previous pairing keys").unwrap();
        // Model a reader that did not allow deletion/replacement (e.g. an editor).
        let held = OpenOptions::new().read(true).share_mode(1).open(&path).unwrap();
        assert!(atomic_write(&path, b"replacement").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"previous pairing keys");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        drop(held);
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}

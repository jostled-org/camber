//! Private-permission, same-directory atomic replacement of one cache file.
//!
//! The one writer the DNS-01 cache publishes through: certificate generations
//! and account credentials alike. A file is written whole to a fresh private
//! temporary beside its destination, synced, and renamed over it. So a reader
//! sees the old file or the new one, never a mix. The directory is then synced
//! so the rename survives a crash.

use std::io::Write;
use std::path::Path;

/// One stage of a publication, in the order they run.
///
/// A fault probe names one stage, and the publisher fails there as the
/// filesystem would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationStage {
    /// Creating the private temporary file.
    Create,
    /// Writing the contents; the fault lands after a partial write.
    Write,
    /// Syncing the temporary file's contents.
    Sync,
    /// Restricting the temporary file to owner read and write.
    Permissions,
    /// Renaming the temporary file over the destination.
    Rename,
    /// Syncing the directory after the rename.
    DirectorySync,
}

/// The stage a publication fails at. Production passes `None`.
pub(super) type Fault = Option<PublicationStage>;

/// Replace `path` with `contents` at private permissions, atomically.
///
/// # Errors
///
/// The I/O failure of the first stage that failed. A failure before the rename
/// leaves `path` as it was and removes the temporary file. A failure of the
/// directory sync comes after the rename, so `path` already holds `contents`.
pub(super) fn write_private_file(
    path: &Path,
    contents: &[u8],
    fault: Fault,
) -> Result<(), std::io::Error> {
    checkpoint(fault, PublicationStage::Create)?;
    PendingFile::create(path)?.commit(path, contents, fault)
}

/// Fail as `stage` would when `fault` names it.
fn checkpoint(fault: Fault, stage: PublicationStage) -> Result<(), std::io::Error> {
    match fault == Some(stage) {
        true => Err(std::io::Error::other(format!(
            "dns01 cache publication faulted at {stage:?}"
        ))),
        false => Ok(()),
    }
}

/// A private temporary file beside its destination, removed on drop unless the
/// rename committed it.
struct PendingFile {
    file: std::fs::File,
    path: Box<Path>,
    committed: bool,
}

impl PendingFile {
    fn create(destination: &Path) -> Result<Self, std::io::Error> {
        let parent = parent_of(destination);
        let file_name = destination
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("cache");
        for attempt in 0..16 {
            let path = parent.join(format!(
                ".{file_name}.{:016x}.{attempt}.tmp",
                crate::prng::next_u64(),
            ));
            match open_private_file(&path) {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        path: path.into_boxed_path(),
                        committed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique cache file",
        ))
    }

    fn commit(
        mut self,
        destination: &Path,
        contents: &[u8],
        fault: Fault,
    ) -> Result<(), std::io::Error> {
        self.write_contents(contents, fault)?;
        checkpoint(fault, PublicationStage::Sync)?;
        self.file.sync_all()?;
        checkpoint(fault, PublicationStage::Permissions)?;
        self.restrict_permissions()?;
        checkpoint(fault, PublicationStage::Rename)?;
        std::fs::rename(&self.path, destination)?;
        self.committed = true;
        checkpoint(fault, PublicationStage::DirectorySync)?;
        sync_parent_directory(destination)
    }

    /// Write `contents`, or half of them before a faulted write fails.
    fn write_contents(&mut self, contents: &[u8], fault: Fault) -> Result<(), std::io::Error> {
        match checkpoint(fault, PublicationStage::Write) {
            Ok(()) => self.file.write_all(contents),
            Err(error) => {
                let partial = contents.get(..contents.len() / 2).unwrap_or_default();
                self.file.write_all(partial)?;
                Err(error)
            }
        }
    }

    /// Set the file to exactly 0600, whatever the umask left at creation.
    #[cfg(unix)]
    fn restrict_permissions(&self) -> Result<(), std::io::Error> {
        use std::os::unix::fs::PermissionsExt;
        self.file
            .set_permissions(std::fs::Permissions::from_mode(0o600))
    }

    /// Report the file left at default permissions. Windows ACLs require a
    /// different approach, so the path is named rather than silently ignored.
    #[cfg(not(unix))]
    fn restrict_permissions(&self) -> Result<(), std::io::Error> {
        tracing::debug!(
            path = %self.path.display(),
            "dns01 acme: cache file permissions left at platform default"
        );
        Ok(())
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        match self.committed {
            true => {}
            false => remove_pending(&self.path),
        }
    }
}

fn remove_pending(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "dns01 acme: failed to remove temporary cache file"
        ),
    }
}

/// The directory that holds `path`. A bare file name's parent is empty, which
/// no directory open accepts, so it reads as the current directory.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn open_private_file(path: &Path) -> Result<std::fs::File, std::io::Error> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), std::io::Error> {
    std::fs::File::open(parent_of(path))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(path: &Path) -> Result<(), std::io::Error> {
    tracing::debug!(
        path = %path.display(),
        "dns01 acme: parent directory sync is unavailable on this platform"
    );
    Ok(())
}

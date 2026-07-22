use std::path::{Path, PathBuf};

pub struct TempRoot {
    inner: tempfile::TempDir,
}

impl TempRoot {
    pub fn new() -> std::io::Result<Self> {
        tempfile::tempdir().map(|inner| Self { inner })
    }

    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    pub fn close(self) -> std::io::Result<()> {
        self.inner.close()
    }
}

pub fn write_cert_files(
    dir: &tempfile::TempDir,
    cert_pem: &[u8],
    key_pem: &[u8],
) -> (PathBuf, PathBuf) {
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();
    (cert_path, key_path)
}

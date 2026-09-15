use anyhow::{ensure, Context, Result};
use lianli_shared::display::MAX_ENCODED_BYTES;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;

pub struct EncodedBuffer(File);

impl EncodedBuffer {
    pub fn create() -> Result<Self> {
        let fd = unsafe {
            libc::memfd_create(
                c"lianli-encoded".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        ensure!(fd >= 0, "Creating encoded frame storage failed");
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(MAX_ENCODED_BYTES as u64)?;
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } == 0,
            "Sealing frame storage failed"
        );
        Ok(Self(file))
    }

    pub fn from_descriptor(fd: OwnedFd, bytes: usize) -> Result<Self> {
        ensure!(
            bytes == MAX_ENCODED_BYTES,
            "Invalid encoded storage capacity"
        );
        let file = File::from(fd);
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() == bytes as u64,
            "Invalid encoded storage object"
        );
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        let required = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        ensure!(
            seals >= 0 && seals & required == required,
            "Encoded storage must have immutable capacity"
        );
        Ok(Self(file))
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_ENCODED_BYTES,
            "Encoded frame exceeds storage bounds"
        );
        self.0
            .write_all_at(bytes, 0)
            .context("Writing encoded frame")
    }

    pub fn read(&self, bytes: usize, target: &mut Vec<u8>) -> Result<()> {
        ensure!(
            bytes > 0 && bytes <= MAX_ENCODED_BYTES,
            "Invalid encoded frame length"
        );
        target.resize(bytes, 0);
        self.0
            .read_exact_at(target, 0)
            .context("Reading encoded frame")
    }
}

impl AsFd for EncodedBuffer {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transferred_storage_is_bounded_and_cannot_be_truncated() {
        let writer = EncodedBuffer::create().unwrap();
        let reader = EncodedBuffer::from_descriptor(
            writer.as_fd().try_clone_to_owned().unwrap(),
            MAX_ENCODED_BYTES,
        )
        .unwrap();
        writer.write(b"encoded packet").unwrap();
        let mut bytes = Vec::new();
        reader.read(14, &mut bytes).unwrap();
        assert_eq!(bytes, b"encoded packet");
        assert!(reader.read(MAX_ENCODED_BYTES + 1, &mut bytes).is_err());
        assert!(writer.0.set_len(0).is_err());
        let unsealed = tempfile::tempfile().unwrap();
        unsealed.set_len(MAX_ENCODED_BYTES as u64).unwrap();
        assert!(EncodedBuffer::from_descriptor(unsealed.into(), MAX_ENCODED_BYTES).is_err());
    }
}

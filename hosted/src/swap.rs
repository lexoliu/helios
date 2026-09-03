//! A swap backend over a sparse file in the runtime directory.
//!
//! `hosted` has no scratch block device, so the thing that stands in for
//! one is an ordinary file: extents are byte ranges in it, a swap-out is
//! a `pwrite` and a swap-in a `pread`, and the file is unlinked as soon
//! as it is opened so a crashed run leaves nothing behind. The file is
//! never read back after a restart — swap is not persistence — so a
//! deleted-but-open file is exactly the right lifetime.
//!
//! This exists so the swap policy, the accounting and the fault path
//! have a backend to run against on the host. It is not evidence for
//! anything about swap performance on a real target: the host page cache
//! sits underneath it, which a virtio-blk scratch disk does not have.
//!
//! Concurrency contract: extent allocation is a small first-fit free
//! list behind a `std::sync::Mutex` that is never held across an
//! `.await`. The I/O itself is positional (`pread`/`pwrite`), so
//! concurrent transfers to different extents do not need the lock at
//! all.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

use helios_hal::vmm::SwapBackend;
use thiserror::Error;

/// Byte range of the swap file backing one swapped-out page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSwapToken {
    offset: u64,
    byte_len: usize,
}

#[derive(Debug, Error)]
pub enum FileSwapError {
    #[error("swap payload is empty")]
    EmptyPayload,
    #[error("swap file has {available_bytes} free bytes, requested {requested_bytes}")]
    OutOfSwap {
        requested_bytes: usize,
        available_bytes: u64,
    },
    #[error("swap-in destination length {actual} does not match token byte length {expected}")]
    InvalidDestination { expected: usize, actual: usize },
    #[error("swap file I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// One free byte range of the swap file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileExtent {
    offset: u64,
    byte_len: u64,
}

#[derive(Debug, Default)]
struct FileSwapState {
    free: Vec<FileExtent>,
}

impl FileSwapState {
    fn allocate(&mut self, byte_len: u64) -> Option<u64> {
        let index = self
            .free
            .iter()
            .position(|extent| extent.byte_len >= byte_len)?;
        let extent = &mut self.free[index];
        let offset = extent.offset;
        extent.offset += byte_len;
        extent.byte_len -= byte_len;
        if extent.byte_len == 0 {
            self.free.swap_remove(index);
        }
        Some(offset)
    }

    fn release(&mut self, released: FileExtent) {
        if released.byte_len == 0 {
            return;
        }
        self.free.push(released);
        self.free.sort_by_key(|extent| extent.offset);
        let mut index = 0;
        while index + 1 < self.free.len() {
            let current = self.free[index];
            let next = self.free[index + 1];
            if current.offset + current.byte_len >= next.offset {
                let end = (current.offset + current.byte_len).max(next.offset + next.byte_len);
                self.free[index].byte_len = end - current.offset;
                self.free.remove(index + 1);
            } else {
                index += 1;
            }
        }
    }

    fn available_bytes(&self) -> u64 {
        self.free.iter().map(|extent| extent.byte_len).sum()
    }
}

/// A swap backend over a sparse file.
pub struct FileSwapBackend {
    file: File,
    capacity_bytes: u64,
    state: Mutex<FileSwapState>,
}

impl FileSwapBackend {
    /// Creates `path`, sizes it to `capacity_bytes`, and unlinks it so
    /// the storage lives exactly as long as this backend does.
    pub fn create(path: &Path, capacity_bytes: u64) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(capacity_bytes)?;
        // The file is only ever reached through this handle; swap is not
        // persistence, and an unlinked file cannot be left behind by a
        // crash.
        std::fs::remove_file(path)?;
        Ok(Self {
            file,
            capacity_bytes,
            state: Mutex::new(FileSwapState {
                free: Vec::from([FileExtent {
                    offset: 0,
                    byte_len: capacity_bytes,
                }]),
            }),
        })
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    /// Bytes currently holding swapped-out pages.
    pub fn used_bytes(&self) -> u64 {
        let available = self.state.lock().expect("swap state").available_bytes();
        self.capacity_bytes.saturating_sub(available)
    }

    fn allocate(&self, byte_len: usize) -> Result<FileSwapToken, FileSwapError> {
        if byte_len == 0 {
            return Err(FileSwapError::EmptyPayload);
        }
        let mut state = self.state.lock().expect("swap state");
        let offset = state
            .allocate(byte_len as u64)
            .ok_or_else(|| FileSwapError::OutOfSwap {
                requested_bytes: byte_len,
                available_bytes: state.available_bytes(),
            })?;
        Ok(FileSwapToken { offset, byte_len })
    }

    fn free(&self, token: FileSwapToken) {
        self.state.lock().expect("swap state").release(FileExtent {
            offset: token.offset,
            byte_len: token.byte_len as u64,
        });
    }
}

impl SwapBackend for FileSwapBackend {
    type Token = FileSwapToken;
    type Error = FileSwapError;

    async fn swap_out(&self, bytes: &[u8]) -> Result<Self::Token, Self::Error> {
        let token = self.allocate(bytes.len())?;
        if let Err(error) = self.file.write_all_at(bytes, token.offset) {
            self.free(token);
            return Err(error.into());
        }
        Ok(token)
    }

    async fn swap_in(&self, token: Self::Token, dst: &mut [u8]) -> Result<(), Self::Error> {
        if dst.len() != token.byte_len {
            return Err(FileSwapError::InvalidDestination {
                expected: token.byte_len,
                actual: dst.len(),
            });
        }
        self.file.read_exact_at(dst, token.offset)?;
        self.free(token);
        Ok(())
    }

    async fn release(&self, token: Self::Token) {
        self.free(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(capacity_bytes: u64) -> FileSwapBackend {
        let path = std::env::temp_dir().join(format!(
            "helios-hosted-swap-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        FileSwapBackend::create(&path, capacity_bytes).expect("swap file")
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        futures_lite::future::block_on(future)
    }

    #[test]
    fn a_page_comes_back_byte_for_byte() {
        let backend = backend(64 * 1024);
        let page: Vec<u8> = (0..4096).map(|index| (index % 251) as u8).collect();
        let token = block_on(backend.swap_out(&page)).expect("swap out");
        let mut restored = vec![0_u8; 4096];
        block_on(backend.swap_in(token, &mut restored)).expect("swap in");
        assert_eq!(restored, page);
    }

    #[test]
    fn swapping_in_gives_the_extent_back() {
        let backend = backend(8192);
        let page = vec![7_u8; 4096];
        let token = block_on(backend.swap_out(&page)).expect("swap out");
        assert_eq!(backend.used_bytes(), 4096);
        let mut restored = vec![0_u8; 4096];
        block_on(backend.swap_in(token, &mut restored)).expect("swap in");
        assert_eq!(backend.used_bytes(), 0);
    }

    #[test]
    fn releasing_gives_the_extent_back_without_reading_it() {
        let backend = backend(8192);
        let token = block_on(backend.swap_out(&[3_u8; 4096])).expect("swap out");
        assert_eq!(backend.used_bytes(), 4096);
        block_on(backend.release(token));
        assert_eq!(backend.used_bytes(), 0);
    }

    #[test]
    fn a_full_swap_file_refuses_rather_than_overwriting() {
        let backend = backend(4096);
        let _first = block_on(backend.swap_out(&[1_u8; 4096])).expect("first page fits");
        let second = block_on(backend.swap_out(&[2_u8; 4096]));
        assert!(
            matches!(second, Err(FileSwapError::OutOfSwap { .. })),
            "a full swap file must refuse, got {second:?}"
        );
    }

    #[test]
    fn released_extents_coalesce_so_a_large_page_still_fits() {
        let backend = backend(8192);
        let first = block_on(backend.swap_out(&[1_u8; 4096])).expect("first");
        let second = block_on(backend.swap_out(&[2_u8; 4096])).expect("second");
        block_on(backend.release(first));
        block_on(backend.release(second));
        let whole = block_on(backend.swap_out(&vec![3_u8; 8192])).expect("coalesced extent");
        assert_eq!(whole.byte_len, 8192);
    }

    #[test]
    fn a_mismatched_destination_is_refused() {
        let backend = backend(8192);
        let token = block_on(backend.swap_out(&[5_u8; 4096])).expect("swap out");
        let mut short = vec![0_u8; 2048];
        let result = block_on(backend.swap_in(token, &mut short));
        assert!(
            matches!(result, Err(FileSwapError::InvalidDestination { .. })),
            "got {result:?}"
        );
    }
}

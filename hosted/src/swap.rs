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

use helios_hal::dma::{BufError, BufResult};
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
    /// The file is written and read through ordinary host calls, so a page
    /// is staged in plain memory.
    type Buffer = Box<[u8]>;

    fn buffer(&self, bytes: usize) -> Result<Self::Buffer, Self::Error> {
        Ok(vec![0_u8; bytes].into_boxed_slice())
    }

    /// The transfer is a synchronous `pwrite` that has finished with the
    /// page before this returns, so the page always comes back.
    async fn swap_out(
        &self,
        page: Self::Buffer,
    ) -> BufResult<Self::Token, Self::Buffer, Self::Error> {
        let token = match self.allocate(page.len()) {
            Ok(token) => token,
            Err(error) => return Err(BufError::returned(error, page)),
        };
        if let Err(error) = self.file.write_all_at(&page, token.offset) {
            self.free(token);
            return Err(BufError::returned(error.into(), page));
        }
        Ok((token, page))
    }

    async fn swap_in(
        &self,
        token: Self::Token,
        mut page: Self::Buffer,
    ) -> BufResult<(), Self::Buffer, Self::Error> {
        if page.len() != token.byte_len {
            let actual = page.len();
            return Err(BufError::returned(
                FileSwapError::InvalidDestination {
                    expected: token.byte_len,
                    actual,
                },
                page,
            ));
        }
        if let Err(error) = self.file.read_exact_at(&mut page, token.offset) {
            return Err(BufError::returned(error.into(), page));
        }
        self.free(token);
        Ok(((), page))
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
        let mut page = backend.buffer(4096).expect("staging buffer");
        for (index, byte) in page.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        let expected = page.clone();
        let (token, _page) = block_on(backend.swap_out(page)).expect("swap out");
        let restored = backend.buffer(4096).expect("staging buffer");
        let ((), restored) = block_on(backend.swap_in(token, restored)).expect("swap in");
        assert_eq!(restored, expected);
    }

    /// A staging buffer of `bytes` bytes of `fill`.
    fn staged(backend: &FileSwapBackend, bytes: usize, fill: u8) -> Box<[u8]> {
        let mut buffer = backend.buffer(bytes).expect("staging buffer");
        buffer.fill(fill);
        buffer
    }

    /// Swaps `bytes` bytes of `fill` out, for the token.
    fn swapped(
        backend: &FileSwapBackend,
        bytes: usize,
        fill: u8,
    ) -> Result<FileSwapToken, FileSwapError> {
        block_on(backend.swap_out(staged(backend, bytes, fill)))
            .map(|(token, _page)| token)
            .map_err(BufError::into_error)
    }

    #[test]
    fn swapping_in_gives_the_extent_back() {
        let backend = backend(8192);
        let token = swapped(&backend, 4096, 7).expect("swap out");
        assert_eq!(backend.used_bytes(), 4096);
        block_on(backend.swap_in(token, staged(&backend, 4096, 0))).expect("swap in");
        assert_eq!(backend.used_bytes(), 0);
    }

    #[test]
    fn releasing_gives_the_extent_back_without_reading_it() {
        let backend = backend(8192);
        let token = swapped(&backend, 4096, 3).expect("swap out");
        assert_eq!(backend.used_bytes(), 4096);
        block_on(backend.release(token));
        assert_eq!(backend.used_bytes(), 0);
    }

    #[test]
    fn a_full_swap_file_refuses_rather_than_overwriting() {
        let backend = backend(4096);
        let _first = swapped(&backend, 4096, 1).expect("first page fits");
        let second = swapped(&backend, 4096, 2);
        assert!(
            matches!(second, Err(FileSwapError::OutOfSwap { .. })),
            "a full swap file must refuse, got {second:?}"
        );
    }

    #[test]
    fn released_extents_coalesce_so_a_large_page_still_fits() {
        let backend = backend(8192);
        let first = swapped(&backend, 4096, 1).expect("first");
        let second = swapped(&backend, 4096, 2).expect("second");
        block_on(backend.release(first));
        block_on(backend.release(second));
        let whole = swapped(&backend, 8192, 3).expect("coalesced extent");
        assert_eq!(whole.byte_len, 8192);
    }

    #[test]
    fn a_mismatched_destination_is_refused() {
        let backend = backend(8192);
        let token = swapped(&backend, 4096, 5).expect("swap out");
        let refused = block_on(backend.swap_in(token, staged(&backend, 2048, 0)))
            .expect_err("a short page is refused");
        assert!(
            matches!(refused.error, FileSwapError::InvalidDestination { .. }),
            "got {:?}",
            refused.error
        );
        assert!(refused.buffers.is_some(), "the page comes back");
    }
}

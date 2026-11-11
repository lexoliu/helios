//! The root filesystem: a writable tree over the read-only boot image.
//!
//! The boot image is mounted at `/` in every namespace and never copied:
//! [`RootFs`] layers a namespace's own [`MemoryFs`] over the one
//! [`BootFs`] the kernel indexed at bring-up, the way an overlay mount
//! stacks a writable upper directory on a read-only lower one.
//!
//! A path names the upper object when the upper tree has one, and the
//! boot object otherwise. Boot objects are read-only: writing one,
//! removing or renaming one, and creating a name inside a directory only
//! the boot image has all fail with [`FsError::ReadOnly`], and hard
//! linking a boot file into the upper tree fails with
//! [`FsError::CrossDevice`], because the two layers are two devices.
//!
//! Concurrency contract: the layers keep their own (the boot image is
//! immutable, the upper tree has its lock); this type holds no state.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use bytes::Bytes;
use helios_hal::fs::{
    DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult, Metadata, NodeKind, OpenFlags,
};

use super::MemoryFs;
use crate::BootFs;

/// A namespace's root: its own writable tree over the shared boot image.
#[derive(Clone)]
pub struct RootFs {
    upper: MemoryFs,
    lower: BootFs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layer {
    Upper,
    Lower,
}

/// A cookie with this bit set resumes in the boot image's listing; one
/// without it resumes in the upper tree's.
const LOWER_COOKIE: u64 = 1 << 63;

impl RootFs {
    pub fn new(upper: MemoryFs, lower: BootFs) -> Self {
        Self { upper, lower }
    }

    pub fn upper(&self) -> &MemoryFs {
        &self.upper
    }

    pub fn lower(&self) -> &BootFs {
        &self.lower
    }

    /// Whether the boot image, rather than this tree's own layer, serves
    /// `path`.
    pub async fn served_by_boot_image(&self, path: FsPath<'_>) -> FsResult<bool> {
        Ok(self.layer(path).await? == Layer::Lower)
    }

    /// Which layer serves `path`.
    async fn layer(&self, path: FsPath<'_>) -> FsResult<Layer> {
        match self.upper.stat(path).await {
            Ok(_) => Ok(Layer::Upper),
            Err(FsError::NoEntry) => self.lower.stat(path).await.map(|_| Layer::Lower),
            Err(error) => Err(error),
        }
    }

    /// The layer an existing object is changed in: only the upper tree's.
    async fn writable(&self, path: FsPath<'_>) -> FsResult<()> {
        match self.layer(path).await? {
            Layer::Upper => Ok(()),
            Layer::Lower => Err(FsError::ReadOnly),
        }
    }

    /// Checks that a new name at `path` can go into the upper tree: the
    /// name is free in both layers and its directory is an upper one.
    async fn creatable(&self, path: FsPath<'_>) -> FsResult<()> {
        let Some((parent, _)) = path.split_last() else {
            return Err(FsError::Exists);
        };
        if self.layer(path).await.is_ok() {
            return Err(FsError::Exists);
        }
        match self.layer(parent).await? {
            Layer::Upper => Ok(()),
            Layer::Lower => Err(FsError::ReadOnly),
        }
    }

    async fn read_lower(
        &self,
        path: FsPath<'_>,
        cookie: DirCookie,
        max_entries: usize,
    ) -> FsResult<Vec<DirEntry>> {
        let mut entries = Vec::new();
        let mut cookie = cookie;
        // Boot entries an upper name shadows are skipped, so one pass may
        // come back short of `max_entries` without being the last.
        while entries.len() < max_entries {
            let batch = self
                .lower
                .read_dir(path, cookie, max_entries - entries.len())
                .await?;
            let complete = batch.len() < max_entries - entries.len();
            for mut entry in batch {
                cookie = entry.next;
                let child = super::join_fs_path(path, &entry.name);
                let shadowed = FsPath::new(&child)
                    .ok_or(FsError::Invalid)
                    .map(|child| self.upper.stat(child));
                match shadowed?.await {
                    Ok(_) => continue,
                    Err(FsError::NoEntry) => {}
                    Err(error) => return Err(error),
                }
                entry.next = DirCookie::new(entry.next.raw() | LOWER_COOKIE);
                entries.push(entry);
            }
            if complete {
                break;
            }
        }
        Ok(entries)
    }
}

impl FileSystem for RootFs {
    async fn stat(&self, path: FsPath<'_>) -> FsResult<Metadata> {
        match self.upper.stat(path).await {
            Err(FsError::NoEntry) => self.lower.stat(path).await,
            served => served,
        }
    }

    async fn open(&self, path: FsPath<'_>, flags: OpenFlags, now: u64) -> FsResult<Metadata> {
        match self.layer(path).await {
            Ok(Layer::Upper) => self.upper.open(path, flags, now).await,
            Ok(Layer::Lower) => self.lower.open(path, flags, now).await,
            Err(FsError::NoEntry) if flags.contains(OpenFlags::CREATE) => {
                self.creatable(path).await?;
                self.upper.open(path, flags, now).await
            }
            Err(error) => Err(error),
        }
    }

    async fn read_at(&self, path: FsPath<'_>, offset: u64, max_bytes: usize) -> FsResult<Bytes> {
        match self.layer(path).await? {
            Layer::Upper => self.upper.read_at(path, offset, max_bytes).await,
            Layer::Lower => self.lower.read_at(path, offset, max_bytes).await,
        }
    }

    async fn write_at(&self, path: FsPath<'_>, offset: u64, data: &[u8], now: u64) -> FsResult<()> {
        self.writable(path).await?;
        self.upper.write_at(path, offset, data, now).await
    }

    async fn append(&self, path: FsPath<'_>, data: &[u8], now: u64) -> FsResult<u64> {
        self.writable(path).await?;
        self.upper.append(path, data, now).await
    }

    async fn set_size(&self, path: FsPath<'_>, size: u64, now: u64) -> FsResult<()> {
        self.writable(path).await?;
        self.upper.set_size(path, size, now).await
    }

    async fn set_times(
        &self,
        path: FsPath<'_>,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
        now: u64,
    ) -> FsResult<()> {
        self.writable(path).await?;
        self.upper
            .set_times(path, access_nanos, modified_nanos, now)
            .await
    }

    async fn read_dir(
        &self,
        path: FsPath<'_>,
        cookie: DirCookie,
        max_entries: usize,
    ) -> FsResult<Vec<DirEntry>> {
        let in_upper = match self.upper.stat(path).await {
            Ok(metadata) if metadata.kind != NodeKind::Directory => {
                return Err(FsError::NotDirectory);
            }
            Ok(_) => true,
            Err(FsError::NoEntry) => false,
            Err(error) => return Err(error),
        };
        if cookie.raw() & LOWER_COOKIE != 0 || !in_upper {
            let lower_cookie = DirCookie::new(cookie.raw() & !LOWER_COOKIE);
            return self.read_lower(path, lower_cookie, max_entries).await;
        }
        let mut entries = self.upper.read_dir(path, cookie, max_entries).await?;
        if entries.len() < max_entries {
            match self
                .read_lower(path, DirCookie::START, max_entries - entries.len())
                .await
            {
                Ok(lower) => entries.extend(lower),
                // An upper directory the boot image does not have.
                Err(FsError::NoEntry) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(entries)
    }

    async fn create_directory(&self, path: FsPath<'_>, now: u64) -> FsResult<()> {
        self.creatable(path).await?;
        self.upper.create_directory(path, now).await
    }

    async fn remove_directory(&self, path: FsPath<'_>) -> FsResult<()> {
        self.writable(path).await?;
        self.upper.remove_directory(path).await
    }

    async fn unlink(&self, path: FsPath<'_>) -> FsResult<()> {
        self.writable(path).await?;
        self.upper.unlink(path).await
    }

    async fn rename(&self, source: FsPath<'_>, destination: FsPath<'_>, now: u64) -> FsResult<()> {
        self.writable(source).await?;
        if source != destination {
            self.creatable(destination).await?;
        }
        self.upper.rename(source, destination, now).await
    }

    async fn link(&self, existing: FsPath<'_>, link: FsPath<'_>, now: u64) -> FsResult<()> {
        match self.layer(existing).await? {
            Layer::Upper => {}
            Layer::Lower => return Err(FsError::CrossDevice),
        }
        self.creatable(link).await?;
        self.upper.link(existing, link, now).await
    }

    async fn symlink(&self, target: &str, link: FsPath<'_>, now: u64) -> FsResult<()> {
        self.creatable(link).await?;
        self.upper.symlink(target, link, now).await
    }

    async fn read_link(&self, path: FsPath<'_>) -> FsResult<String> {
        match self.layer(path).await? {
            Layer::Upper => self.upper.read_link(path).await,
            Layer::Lower => self.lower.read_link(path).await,
        }
    }

    async fn sync(&self, path: FsPath<'_>) -> FsResult<()> {
        match self.layer(path).await? {
            Layer::Upper => self.upper.sync(path).await,
            Layer::Lower => self.lower.sync(path).await,
        }
    }

    async fn sync_data(&self, path: FsPath<'_>) -> FsResult<()> {
        match self.layer(path).await? {
            Layer::Upper => self.upper.sync_data(path).await,
            Layer::Lower => self.lower.sync_data(path).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec::Vec;

    use futures_lite::future::block_on;
    use helios_hal::fs::{DeviceId, DirCookie, FileSystem, FsError, FsPath, OpenFlags};

    use super::RootFs;
    use crate::vfs::MemoryFs;
    use crate::{BootFs, EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs};

    const IMAGE: EmbeddedBootFs = EmbeddedBootFs::new(
        &[EmbeddedBootDirectory::new("bin/empty", 43)],
        &[
            EmbeddedBootFile::new("bin/tool", b"tool", 42),
            EmbeddedBootFile::new("init.txt", b"hello", 1),
        ],
    );

    fn root() -> RootFs {
        RootFs::new(
            MemoryFs::new(DeviceId::new(4)),
            BootFs::new(IMAGE, DeviceId::new(1)),
        )
    }

    fn path(path: &str) -> FsPath<'_> {
        FsPath::new(path).expect("test paths are valid")
    }

    fn listing(fs: &RootFs, directory: &str, batch: usize) -> Vec<String> {
        let mut names = Vec::new();
        let mut cookie = DirCookie::START;
        loop {
            let entries =
                block_on(fs.read_dir(path(directory), cookie, batch)).expect("directory lists");
            let complete = entries.len() < batch;
            for entry in entries {
                cookie = entry.next;
                names.push(entry.name);
            }
            if complete {
                return names;
            }
        }
    }

    #[test]
    fn the_root_merges_both_layers_in_every_batch_size() {
        let fs = root();
        block_on(fs.create_directory(path("d"), 1)).expect("creates in the upper root");
        block_on(fs.open(path("d/f"), OpenFlags::CREATE, 1)).expect("creates");
        for batch in [1, 2, 64] {
            let mut names = listing(&fs, "", batch);
            names.sort();
            assert_eq!(names, ["bin", "d", "init.txt"], "batch of {batch}");
        }
        assert_eq!(listing(&fs, "bin", 1), ["empty", "tool"]);
        assert_eq!(listing(&fs, "d", 8), ["f"]);
    }

    #[test]
    fn boot_objects_are_read_only_and_their_directories_take_no_names() {
        let fs = root();
        assert_eq!(
            block_on(fs.read_at(path("bin/tool"), 0, 64))
                .expect("reads")
                .as_ref(),
            b"tool"
        );
        assert_eq!(
            block_on(fs.write_at(path("init.txt"), 0, b"x", 1)),
            Err(FsError::ReadOnly)
        );
        assert_eq!(
            block_on(fs.unlink(path("bin/tool"))),
            Err(FsError::ReadOnly)
        );
        assert_eq!(
            block_on(fs.open(path("bin/new"), OpenFlags::CREATE, 1)).map(drop),
            Err(FsError::ReadOnly)
        );
        assert_eq!(
            block_on(fs.create_directory(path("bin"), 1)),
            Err(FsError::Exists)
        );
        assert_eq!(
            block_on(fs.link(path("init.txt"), path("copy"), 1)),
            Err(FsError::CrossDevice)
        );
        assert_eq!(
            block_on(fs.rename(path("init.txt"), path("moved"), 1)),
            Err(FsError::ReadOnly)
        );
    }

    #[test]
    fn two_roots_over_one_image_keep_their_writes_apart() {
        let lower = BootFs::new(IMAGE, DeviceId::new(1));
        let first = RootFs::new(MemoryFs::new(DeviceId::new(4)), lower.clone());
        let second = RootFs::new(MemoryFs::new(DeviceId::new(4)), lower);
        block_on(first.open(path("mine"), OpenFlags::CREATE, 1)).expect("creates");
        assert_eq!(
            block_on(second.stat(path("mine"))).map(drop),
            Err(FsError::NoEntry)
        );
    }
}

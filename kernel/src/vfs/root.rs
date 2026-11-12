//! The root filesystem: a writable tree over the read-only boot image.
//!
//! The boot image is mounted at `/` in every namespace and never copied:
//! [`RootFs`] layers a namespace's own [`MemoryFs`] over the one
//! [`BootFs`] the kernel indexed at bring-up, with the semantics of an
//! overlay mount's writable upper directory over a read-only lower one.
//!
//! - A path names the upper object when the upper tree has one, and the
//!   boot object otherwise, unless the upper tree hid it.
//! - A new name may go into any directory, a boot one included: the
//!   directory, and every one above it, is copied up first, empty and
//!   still merging the boot image's entries.
//! - Changing a boot object copies it up first: its data, size or times,
//!   a hard link to it, a rename of it. A copy is the upper tree's, so its
//!   bytes are written bytes, never boot bytes ([`FileContents`]).
//! - Removing a boot object, or an upper object over one, leaves a
//!   whiteout that hides it; a directory made where a whiteout was is
//!   opaque and shows nothing of the boot directory beneath.
//! - Renaming a directory the boot image has a part in is
//!   [`FsError::CrossDevice`], as an overlay without redirects answers,
//!   so a caller copies the tree instead.
//!
//! Concurrency contract: this type holds no state. The boot image never
//! changes, so a check against it cannot go stale; every change to the
//! upper tree, with the whiteouts and opaque marks it carries, is one step
//! under that tree's lock, and the step itself refuses what a concurrent
//! change made wrong (a copy-up of an object removed meanwhile, a
//! directory that gained an entry). Two copy-ups of one object race
//! harmlessly: the first makes the copy and the second finds it.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;

use bytes::Bytes;
use helios_hal::fs::{
    DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult, Metadata, NodeKind, OpenFlags,
    WritePosition,
};

use super::memory::UpperView;
use super::{FileContents, MemoryFs, read_all};
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

/// An object the root shows, with what the upper tree knows of its path.
struct Found {
    layer: Layer,
    metadata: Metadata,
    upper: UpperView,
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

    /// The whole of the file at `path`, typed by the layer that served it:
    /// the boot image's own bytes, or this tree's.
    pub async fn read_whole(&self, path: FsPath<'_>) -> FsResult<FileContents> {
        match self.find(path)?.layer {
            Layer::Upper => read_all(&self.upper, path).await.map(FileContents::Written),
            Layer::Lower => self.lower.file_contents(path).map(FileContents::Boot),
        }
    }

    /// The object `path` names, and the layer it comes from.
    ///
    /// This and the other helpers below answer at once: both layers are in
    /// memory, and [`FileSystem::write_with`] must copy up before it
    /// returns.
    fn find(&self, path: FsPath<'_>) -> FsResult<Found> {
        let upper = self.upper.view(path)?;
        let (layer, metadata) = match &upper.object {
            Some(metadata) => (Layer::Upper, *metadata),
            None if upper.beneath => (Layer::Lower, self.lower.metadata(path)?),
            None => return Err(FsError::NoEntry),
        };
        Ok(Found {
            layer,
            metadata,
            upper,
        })
    }

    /// Whether the boot image has an object at `path` that shows once the
    /// upper tree's object there is gone: what a removal must hide.
    fn lower_beneath(&self, path: FsPath<'_>, found: &Found) -> bool {
        found.upper.beneath && (found.layer == Layer::Lower || self.lower.metadata(path).is_ok())
    }

    /// Makes `directory` and every directory above it the upper tree's,
    /// copying up the boot directories among them.
    fn copy_up_directories(&self, directory: FsPath<'_>) -> FsResult<()> {
        let full = directory.as_str();
        let ends = full
            .match_indices('/')
            .map(|(end, _)| end)
            .chain((!directory.is_root()).then_some(full.len()));
        for end in ends {
            let prefix = FsPath::new(&full[..end]).ok_or(FsError::Invalid)?;
            let found = self.find(prefix)?;
            if found.metadata.kind != NodeKind::Directory {
                return Err(FsError::NotDirectory);
            }
            if found.layer == Layer::Lower {
                self.upper
                    .copy_up(prefix, NodeKind::Directory, Bytes::new(), &found.metadata)?;
            }
        }
        Ok(())
    }

    /// Makes the object at `path` the upper tree's, copying a boot one up
    /// with its bytes and times, before anything changes it.
    fn copy_up(&self, path: FsPath<'_>) -> FsResult<Found> {
        let found = self.find(path)?;
        if found.layer == Layer::Upper {
            return Ok(found);
        }
        let contents = match found.metadata.kind {
            // The image's bytes, not a copy of them: the upper tree copies
            // them out only when they are first written.
            NodeKind::File => self.lower.file_contents(path)?.to_bytes(),
            NodeKind::Symlink => Bytes::from(self.lower.link_target(path)?),
            NodeKind::Directory => Bytes::new(),
        };
        if let Some((parent, _)) = path.split_last() {
            self.copy_up_directories(parent)?;
        }
        self.upper
            .copy_up(path, found.metadata.kind, contents, &found.metadata)?;
        self.find(path)
    }

    /// Checks that `path` names nothing and readies its directory in the
    /// upper tree for the new name.
    fn creatable(&self, path: FsPath<'_>) -> FsResult<()> {
        let Some((parent, _)) = path.split_last() else {
            return Err(FsError::Exists);
        };
        match self.find(path) {
            Ok(_) => return Err(FsError::Exists),
            Err(FsError::NoEntry) => {}
            Err(error) => return Err(error),
        }
        self.copy_up_directories(parent)
    }

    /// Whether the directory `found` names is one the boot image has a
    /// part in: a boot directory, or an upper one merging a boot one.
    fn merges_boot(&self, path: FsPath<'_>, found: &Found) -> bool {
        match found.layer {
            Layer::Lower => true,
            Layer::Upper => {
                !found.upper.opaque
                    && found.upper.beneath
                    && self
                        .lower
                        .metadata(path)
                        .is_ok_and(|lower| lower.kind == NodeKind::Directory)
            }
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
        // Boot entries the upper tree shadows or hides are skipped, so one
        // pass may come back short of `max_entries` without being the
        // last.
        while entries.len() < max_entries {
            let batch = self
                .lower
                .read_dir(path, cookie, max_entries - entries.len())
                .await?;
            let complete = batch.len() < max_entries - entries.len();
            for mut entry in batch {
                cookie = entry.next;
                let child = super::join_fs_path(path, &entry.name);
                let child = FsPath::new(&child).ok_or(FsError::Invalid)?;
                let upper = self.upper.view(child)?;
                if upper.object.is_some() || !upper.beneath {
                    continue;
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
        self.find(path).map(|found| found.metadata)
    }

    async fn open(&self, path: FsPath<'_>, flags: OpenFlags, now: u64) -> FsResult<Metadata> {
        match self.find(path) {
            Ok(found) if found.layer == Layer::Upper => self.upper.open(path, flags, now).await,
            Ok(_) if flags.contains(OpenFlags::TRUNCATE) => {
                self.copy_up(path)?;
                self.upper.open(path, flags, now).await
            }
            Ok(_) => self.lower.open(path, flags, now).await,
            Err(FsError::NoEntry) if flags.contains(OpenFlags::CREATE) => {
                self.creatable(path)?;
                self.upper.open(path, flags, now).await
            }
            Err(error) => Err(error),
        }
    }

    async fn read_at(&self, path: FsPath<'_>, offset: u64, max_bytes: usize) -> FsResult<Bytes> {
        match self.find(path)?.layer {
            Layer::Upper => self.upper.read_at(path, offset, max_bytes).await,
            Layer::Lower => self.lower.read_at(path, offset, max_bytes).await,
        }
    }

    fn write_with<'a, F>(
        &'a self,
        path: FsPath<'a>,
        position: WritePosition,
        len: usize,
        now: u64,
        fill: F,
    ) -> impl Future<Output = FsResult<u64>> + Send + 'a
    where
        F: FnOnce(&mut [u8]),
    {
        // The copy-up and the write are done here; the future only reports
        // them, so it holds nothing of `fill`.
        core::future::ready(
            self.copy_up(path)
                .and_then(|_| self.upper.write_now(path, position, len, now, fill)),
        )
    }

    async fn set_size(&self, path: FsPath<'_>, size: u64, now: u64) -> FsResult<()> {
        self.copy_up(path)?;
        self.upper.set_size(path, size, now).await
    }

    async fn set_times(
        &self,
        path: FsPath<'_>,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
        now: u64,
    ) -> FsResult<()> {
        self.copy_up(path)?;
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
        let found = self.find(path)?;
        if found.metadata.kind != NodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        let in_upper = found.layer == Layer::Upper;
        let merges = !found.upper.opaque && found.upper.beneath;
        if cookie.raw() & LOWER_COOKIE != 0 || !in_upper {
            if !merges {
                // The directory turned opaque since the listing began.
                return Ok(Vec::new());
            }
            let lower_cookie = DirCookie::new(cookie.raw() & !LOWER_COOKIE);
            return self.read_lower(path, lower_cookie, max_entries).await;
        }
        let mut entries = self.upper.read_dir(path, cookie, max_entries).await?;
        if merges && entries.len() < max_entries {
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
        self.creatable(path)?;
        self.upper.create_directory(path, now).await
    }

    async fn remove_directory(&self, path: FsPath<'_>) -> FsResult<()> {
        let Some((parent, _)) = path.split_last() else {
            return Err(FsError::NotPermitted);
        };
        let found = self.find(path)?;
        if found.metadata.kind != NodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        // The upper tree refuses a directory of its own that holds a name;
        // the boot entries still showing are this layer's to count.
        if !self.read_dir(path, DirCookie::START, 1).await?.is_empty() {
            return Err(FsError::NotEmpty);
        }
        let whiteout = self.lower_beneath(path, &found);
        if whiteout {
            self.copy_up_directories(parent)?;
        }
        self.upper.remove(path, true, whiteout)
    }

    async fn unlink(&self, path: FsPath<'_>) -> FsResult<()> {
        let Some((parent, _)) = path.split_last() else {
            return Err(FsError::IsDirectory);
        };
        let found = self.find(path)?;
        if found.metadata.kind == NodeKind::Directory {
            return Err(FsError::IsDirectory);
        }
        let whiteout = self.lower_beneath(path, &found);
        if whiteout {
            self.copy_up_directories(parent)?;
        }
        self.upper.remove(path, false, whiteout)
    }

    async fn rename(&self, source: FsPath<'_>, destination: FsPath<'_>, now: u64) -> FsResult<()> {
        let found = self.find(source)?;
        if source == destination {
            return Ok(());
        }
        if found.metadata.kind == NodeKind::Directory && self.merges_boot(source, &found) {
            return Err(FsError::CrossDevice);
        }
        self.creatable(destination)?;
        let found = self.copy_up(source)?;
        let whiteout = self.lower_beneath(source, &found);
        self.upper.rename_over(source, destination, now, whiteout)
    }

    async fn link(&self, existing: FsPath<'_>, link: FsPath<'_>, now: u64) -> FsResult<()> {
        if self.find(existing)?.metadata.kind == NodeKind::Directory {
            return Err(FsError::NotPermitted);
        }
        self.creatable(link)?;
        self.copy_up(existing)?;
        self.upper.link(existing, link, now).await
    }

    async fn symlink(&self, target: &str, link: FsPath<'_>, now: u64) -> FsResult<()> {
        self.creatable(link)?;
        self.upper.symlink(target, link, now).await
    }

    async fn read_link(&self, path: FsPath<'_>) -> FsResult<String> {
        match self.find(path)?.layer {
            Layer::Upper => self.upper.read_link(path).await,
            Layer::Lower => self.lower.read_link(path).await,
        }
    }

    async fn sync(&self, path: FsPath<'_>) -> FsResult<()> {
        match self.find(path)?.layer {
            Layer::Upper => self.upper.sync(path).await,
            Layer::Lower => self.lower.sync(path).await,
        }
    }

    async fn sync_data(&self, path: FsPath<'_>) -> FsResult<()> {
        match self.find(path)?.layer {
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
    use helios_hal::fs::{
        DeviceId, DirCookie, FileSystem, FsError, FsPath, OpenFlags, WritePosition,
    };

    use super::RootFs;
    use crate::vfs::{FileContents, MemoryFs, write_bytes};
    use crate::{BootFs, EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs};

    const IMAGE: EmbeddedBootFs = EmbeddedBootFs::new(
        &[EmbeddedBootDirectory::new("bin/empty", 43)],
        &[
            EmbeddedBootFile::new("bin/tool", b"tool", 42),
            EmbeddedBootFile::new("etc/motd", "日本語 한국어 中文".as_bytes(), 7),
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
                names.sort();
                return names;
            }
        }
    }

    fn write(fs: &RootFs, file: &str, position: WritePosition, data: &[u8], now: u64) -> u64 {
        block_on(write_bytes(fs, path(file), position, data, now)).expect("writes")
    }

    fn contents(fs: &RootFs, file: &str) -> Vec<u8> {
        block_on(fs.read_at(path(file), 0, 64))
            .expect("reads")
            .to_vec()
    }

    #[test]
    fn the_root_merges_both_layers_in_every_batch_size() {
        let fs = root();
        block_on(fs.create_directory(path("d"), 1)).expect("creates in the upper root");
        block_on(fs.open(path("d/f"), OpenFlags::CREATE, 1)).expect("creates");
        for batch in [1, 2, 64] {
            assert_eq!(
                listing(&fs, "", batch),
                ["bin", "d", "etc", "init.txt"],
                "batch of {batch}"
            );
        }
        assert_eq!(listing(&fs, "bin", 1), ["empty", "tool"]);
        assert_eq!(listing(&fs, "d", 8), ["f"]);
    }

    #[test]
    fn a_boot_file_is_copied_up_before_it_changes() {
        let lower = BootFs::new(IMAGE, DeviceId::new(1));
        let fs = RootFs::new(MemoryFs::new(DeviceId::new(4)), lower.clone());
        let boot_id = block_on(fs.stat(path("init.txt"))).expect("stats").id;

        write(&fs, "init.txt", WritePosition::At(0), b"J", 2);
        assert_eq!(contents(&fs, "init.txt"), b"Jello");
        assert_eq!(write(&fs, "init.txt", WritePosition::End, b"!", 3), 5);
        block_on(fs.set_size(path("etc/motd"), 9, 4)).expect("truncates a copy");
        assert_eq!(contents(&fs, "etc/motd"), "日本語".as_bytes());
        let copy = block_on(fs.stat(path("init.txt"))).expect("stats");
        assert_ne!(copy.id, boot_id, "the copy is the upper tree's object");
        assert!(matches!(
            block_on(fs.read_whole(path("init.txt"))),
            Ok(FileContents::Written(bytes)) if bytes.as_ref() == b"Jello!"
        ));

        let untouched = RootFs::new(MemoryFs::new(DeviceId::new(4)), lower);
        assert_eq!(contents(&untouched, "init.txt"), b"hello");
        assert!(matches!(
            block_on(untouched.read_whole(path("init.txt"))),
            Ok(FileContents::Boot(_))
        ));
        assert_eq!(listing(&fs, "etc", 8), ["motd"]);
    }

    #[test]
    fn a_new_name_goes_into_a_boot_directory() {
        let fs = root();
        block_on(fs.open(path("bin/new"), OpenFlags::CREATE, 1)).expect("creates");
        block_on(fs.create_directory(path("bin/empty/inner"), 1)).expect("creates");
        block_on(fs.symlink("tool", path("bin/alias"), 1)).expect("links");
        assert_eq!(listing(&fs, "bin", 1), ["alias", "empty", "new", "tool"]);
        assert_eq!(listing(&fs, "bin/empty", 8), ["inner"]);
        assert_eq!(
            block_on(fs.create_directory(path("bin"), 1)),
            Err(FsError::Exists)
        );
        assert_eq!(
            block_on(fs.open(path("init.txt/x"), OpenFlags::CREATE, 1)).map(drop),
            Err(FsError::NotDirectory)
        );
    }

    #[test]
    fn removing_a_boot_object_leaves_a_whiteout() {
        let fs = root();
        assert_eq!(
            block_on(fs.remove_directory(path("bin"))),
            Err(FsError::NotEmpty)
        );
        block_on(fs.unlink(path("bin/tool"))).expect("removes a boot file");
        assert_eq!(
            block_on(fs.stat(path("bin/tool"))).map(drop),
            Err(FsError::NoEntry)
        );
        assert_eq!(listing(&fs, "bin", 1), ["empty"]);
        block_on(fs.remove_directory(path("bin/empty"))).expect("removes a boot directory");
        block_on(fs.remove_directory(path("bin"))).expect("now empty");
        assert_eq!(listing(&fs, "", 8), ["etc", "init.txt"]);

        // A copied-up file removed is hidden too, not uncovered.
        write(&fs, "init.txt", WritePosition::At(0), b"J", 1);
        block_on(fs.unlink(path("init.txt"))).expect("removes the copy");
        assert_eq!(
            block_on(fs.stat(path("init.txt"))).map(drop),
            Err(FsError::NoEntry)
        );

        // A name made where a whiteout is starts afresh.
        block_on(fs.open(path("init.txt"), OpenFlags::CREATE, 2)).expect("creates");
        assert_eq!(contents(&fs, "init.txt"), b"");
    }

    #[test]
    fn a_directory_made_over_a_whiteout_shows_nothing_beneath() {
        let fs = root();
        block_on(fs.unlink(path("etc/motd"))).expect("removes");
        block_on(fs.remove_directory(path("etc"))).expect("removes");
        block_on(fs.create_directory(path("etc"), 1)).expect("makes it again");
        assert_eq!(listing(&fs, "etc", 8), Vec::<String>::new());
        assert_eq!(
            block_on(fs.stat(path("etc/motd"))).map(drop),
            Err(FsError::NoEntry)
        );
    }

    #[test]
    fn renaming_copies_a_boot_file_up_and_refuses_a_boot_directory() {
        let fs = root();
        block_on(fs.rename(path("init.txt"), path("moved.txt"), 1)).expect("renames");
        assert_eq!(contents(&fs, "moved.txt"), b"hello");
        assert_eq!(
            block_on(fs.stat(path("init.txt"))).map(drop),
            Err(FsError::NoEntry)
        );
        assert_eq!(
            block_on(fs.rename(path("bin"), path("sbin"), 1)),
            Err(FsError::CrossDevice)
        );
        // Copied up by the new name, `bin` still merges the image.
        block_on(fs.open(path("bin/new"), OpenFlags::CREATE, 1)).expect("creates");
        assert_eq!(
            block_on(fs.rename(path("bin"), path("sbin"), 1)),
            Err(FsError::CrossDevice)
        );
        block_on(fs.rename(path("bin/tool"), path("bin/empty/tool"), 1)).expect("renames");
        assert_eq!(listing(&fs, "bin", 8), ["empty", "new"]);
        assert_eq!(contents(&fs, "bin/empty/tool"), b"tool");

        block_on(fs.create_directory(path("d"), 1)).expect("creates");
        block_on(fs.rename(path("d"), path("bin/d"), 1)).expect("an upper directory moves");
    }

    #[test]
    fn a_hard_link_to_a_boot_file_copies_it_up() {
        let fs = root();
        block_on(fs.link(path("etc/motd"), path("motd"), 1)).expect("links");
        let original = block_on(fs.stat(path("etc/motd"))).expect("stats");
        let link = block_on(fs.stat(path("motd"))).expect("stats");
        assert_eq!((original.id, original.link_count), (link.id, 2));
        write(&fs, "motd", WritePosition::At(0), "中文".as_bytes(), 2);
        assert_eq!(&contents(&fs, "etc/motd")[..6], "中文".as_bytes());
        assert_eq!(
            block_on(fs.link(path("bin"), path("bin2"), 1)),
            Err(FsError::NotPermitted)
        );
    }

    #[test]
    fn two_roots_over_one_image_keep_their_writes_apart() {
        let lower = BootFs::new(IMAGE, DeviceId::new(1));
        let first = RootFs::new(MemoryFs::new(DeviceId::new(4)), lower.clone());
        let second = RootFs::new(MemoryFs::new(DeviceId::new(4)), lower);
        block_on(first.open(path("mine"), OpenFlags::CREATE, 1)).expect("creates");
        block_on(first.unlink(path("bin/tool"))).expect("removes");
        assert_eq!(
            block_on(second.stat(path("mine"))).map(drop),
            Err(FsError::NoEntry)
        );
        assert_eq!(contents(&second, "bin/tool"), b"tool");
    }
}

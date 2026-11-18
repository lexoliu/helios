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
    DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult, Landed, Metadata, NodeKind,
    OpenFlags, OwnedFsPath, PreparedWrite, RenameMode, WritePosition, WriteProgress,
};

use super::memory::UpperView;
use super::{FileContents, MemoryFs, UnixAttributes, read_all};
use crate::BootFs;

/// The Unix attributes the boot image's directories hold; its files and
/// links take [`UnixAttributes::inherited`] from this.
const BOOT_UNIX: UnixAttributes = UnixAttributes {
    permissions: 0o755,
    uid: 0,
    gid: 0,
};

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

    /// The Unix attributes of the object at `path`, from whichever layer
    /// serves it.
    pub fn unix_attributes(&self, path: FsPath<'_>) -> FsResult<UnixAttributes> {
        let found = self.find(path)?;
        match found.layer {
            Layer::Upper => Ok(self.upper.unix_metadata(path)?.attributes),
            Layer::Lower => Ok(BOOT_UNIX.inherited(found.metadata.kind)),
        }
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
    /// memory, and a [`RootWrite`]'s submit must copy up before it
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
                self.upper.copy_up(
                    prefix,
                    NodeKind::Directory,
                    BOOT_UNIX,
                    Bytes::new(),
                    &found.metadata,
                )?;
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
            // The boot image holds none of these kinds.
            NodeKind::CharacterDevice
            | NodeKind::BlockDevice
            | NodeKind::Fifo
            | NodeKind::Socket => return Err(FsError::Unsupported),
        };
        if let Some((parent, _)) = path.split_last() {
            self.copy_up_directories(parent)?;
        }
        self.upper.copy_up(
            path,
            found.metadata.kind,
            BOOT_UNIX.inherited(found.metadata.kind),
            contents,
            &found.metadata,
        )?;
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

/// A write to a namespace's root, staged at once like the tree it lands
/// in. A boot-image file is copied up when the write is filled, so a
/// write dropped before then leaves the boot layer serving the file.
pub struct RootWrite {
    root: RootFs,
    path: OwnedFsPath,
    position: WritePosition,
    len: usize,
    now: u64,
}

impl RootWrite {
    fn prepare(
        root: &RootFs,
        path: FsPath<'_>,
        position: WritePosition,
        len: usize,
        now: u64,
    ) -> FsResult<Self> {
        root.find(path)?.metadata.kind.require_regular()?;
        Ok(Self {
            root: root.clone(),
            path: path.into(),
            position,
            len,
            now,
        })
    }

    fn land(self, mut fill: impl FnMut(usize, &mut [u8])) -> FsResult<Landed> {
        self.root.copy_up(self.path.as_path())?;
        self.root.upper.write_now(
            self.path.as_path(),
            self.position,
            self.len,
            self.now,
            |destination| fill(0, destination),
        )
    }
}

impl PreparedWrite for RootWrite {
    type Filled = FsResult<WriteProgress<Self>>;

    fn staged_len(&self) -> usize {
        self.len
    }

    fn fill<F>(self, fill: F) -> Self::Filled
    where
        F: FnMut(usize, &mut [u8]),
    {
        // Copy-up and writing finish before the source borrow ends.
        self.land(fill).map(WriteProgress::Done)
    }
}

impl FileSystem for RootFs {
    type Write = RootWrite;

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

    fn prepare_write<'a>(
        &'a self,
        path: FsPath<'a>,
        position: WritePosition,
        len: usize,
        now: u64,
    ) -> impl Future<Output = FsResult<RootWrite>> + Send + 'a {
        core::future::ready(RootWrite::prepare(self, path, position, len, now))
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

    async fn remove_directory(&self, path: FsPath<'_>, now: u64) -> FsResult<()> {
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
        self.upper.remove(path, true, whiteout, now)
    }

    async fn unlink(&self, path: FsPath<'_>, now: u64) -> FsResult<()> {
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
        self.upper.remove(path, false, whiteout, now)
    }

    async fn rename(
        &self,
        source: FsPath<'_>,
        destination: FsPath<'_>,
        mode: RenameMode,
        now: u64,
    ) -> FsResult<()> {
        let found = self.find(source)?;
        if mode == RenameMode::Exchange {
            // Both names stay, so the swap needs no whiteout. A directory
            // merging the boot image's would carry that merge to its new
            // name, which an overlay without redirects cannot express.
            let other = self.find(destination)?;
            for (path, found) in [(source, &found), (destination, &other)] {
                if found.metadata.kind == NodeKind::Directory && self.merges_boot(path, found) {
                    return Err(FsError::CrossDevice);
                }
            }
            self.copy_up(source)?;
            self.copy_up(destination)?;
            return self
                .upper
                .rename_over(source, destination, mode, now, false);
        }
        if source == destination {
            return match mode {
                RenameMode::NoReplace => Err(FsError::Exists),
                RenameMode::Replace | RenameMode::Exchange => Ok(()),
            };
        }
        if found.metadata.kind == NodeKind::Directory && self.merges_boot(source, &found) {
            return Err(FsError::CrossDevice);
        }
        self.creatable(destination)?;
        let found = self.copy_up(source)?;
        let whiteout = self.lower_beneath(source, &found);
        self.upper
            .rename_over(source, destination, mode, now, whiteout)
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
        DeviceId, DirCookie, FileSystem, FsError, FsPath, OpenFlags, RenameMode, WritePosition,
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
            block_on(fs.remove_directory(path("bin"), 7_000_000_000)),
            Err(FsError::NotEmpty)
        );
        block_on(fs.unlink(path("bin/tool"), 7_000_000_000)).expect("removes a boot file");
        assert_eq!(
            block_on(fs.stat(path("bin/tool"))).map(drop),
            Err(FsError::NoEntry)
        );
        assert_eq!(listing(&fs, "bin", 1), ["empty"]);
        block_on(fs.remove_directory(path("bin/empty"), 7_000_000_000))
            .expect("removes a boot directory");
        block_on(fs.remove_directory(path("bin"), 7_000_000_000)).expect("now empty");
        assert_eq!(listing(&fs, "", 8), ["etc", "init.txt"]);

        // A copied-up file removed is hidden too, not uncovered.
        write(&fs, "init.txt", WritePosition::At(0), b"J", 1);
        block_on(fs.unlink(path("init.txt"), 7_000_000_000)).expect("removes the copy");
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
        block_on(fs.unlink(path("etc/motd"), 7_000_000_000)).expect("removes");
        block_on(fs.remove_directory(path("etc"), 7_000_000_000)).expect("removes");
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
        block_on(fs.rename(path("init.txt"), path("moved.txt"), RenameMode::Replace, 1))
            .expect("renames");
        assert_eq!(contents(&fs, "moved.txt"), b"hello");
        assert_eq!(
            block_on(fs.stat(path("init.txt"))).map(drop),
            Err(FsError::NoEntry)
        );
        assert_eq!(
            block_on(fs.rename(path("bin"), path("sbin"), RenameMode::Replace, 1)),
            Err(FsError::CrossDevice)
        );
        // Copied up by the new name, `bin` still merges the image.
        block_on(fs.open(path("bin/new"), OpenFlags::CREATE, 1)).expect("creates");
        assert_eq!(
            block_on(fs.rename(path("bin"), path("sbin"), RenameMode::Replace, 1)),
            Err(FsError::CrossDevice)
        );
        block_on(fs.rename(
            path("bin/tool"),
            path("bin/empty/tool"),
            RenameMode::Replace,
            1,
        ))
        .expect("renames");
        assert_eq!(listing(&fs, "bin", 8), ["empty", "new"]);
        assert_eq!(contents(&fs, "bin/empty/tool"), b"tool");

        block_on(fs.create_directory(path("d"), 1)).expect("creates");
        block_on(fs.rename(path("d"), path("bin/d"), RenameMode::Replace, 1))
            .expect("an upper directory moves");
    }

    #[test]
    fn noreplace_sees_a_boot_destination_and_exchange_copies_boot_files_up() {
        let fs = root();
        assert_eq!(
            block_on(fs.rename(path("init.txt"), path("etc/motd"), RenameMode::NoReplace, 1)),
            Err(FsError::Exists)
        );
        assert_eq!(
            block_on(fs.rename(path("init.txt"), path("init.txt"), RenameMode::NoReplace, 1)),
            Err(FsError::Exists)
        );
        block_on(fs.rename(path("init.txt"), path("fresh"), RenameMode::NoReplace, 1))
            .expect("renames");
        assert_eq!(contents(&fs, "fresh"), b"hello");
        // Two boot files swap contents between their names.
        block_on(fs.rename(path("fresh"), path("etc/motd"), RenameMode::Exchange, 2))
            .expect("exchanges");
        assert_eq!(contents(&fs, "etc/motd"), b"hello");
        assert_eq!(contents(&fs, "fresh"), "日本語 한국어 中文".as_bytes());
        block_on(fs.rename(path("bin/tool"), path("fresh"), RenameMode::Exchange, 3))
            .expect("exchanges");
        assert_eq!(contents(&fs, "bin/tool"), "日本語 한국어 中文".as_bytes());
        assert_eq!(contents(&fs, "fresh"), b"tool");
        // A directory merging the boot image's cannot change places, as for
        // any rename of one; a missing name is not found.
        assert_eq!(
            block_on(fs.rename(path("bin"), path("fresh"), RenameMode::Exchange, 4)),
            Err(FsError::CrossDevice)
        );
        assert_eq!(
            block_on(fs.rename(path("fresh"), path("none"), RenameMode::Exchange, 4)),
            Err(FsError::NoEntry)
        );
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
        block_on(first.unlink(path("bin/tool"), 7_000_000_000)).expect("removes");
        assert_eq!(
            block_on(second.stat(path("mine"))).map(drop),
            Err(FsError::NoEntry)
        );
        assert_eq!(contents(&second, "bin/tool"), b"tool");
    }
}

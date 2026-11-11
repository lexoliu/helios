//! The boot filesystem: the read-only tree the boot payload carries.
//!
//! [`EmbeddedBootFs`] is the image as the payload parse leaves it, two
//! slices of `'static` entries. [`BootFs`] is that image as a mountable
//! [`FileSystem`]: an index sorted by path, built once when the kernel
//! takes the payload and shared by every mount and every instance, so no
//! instance copies the tree.
//!
//! Concurrency contract: a [`BootFs`] is immutable after
//! [`BootFs::new`]; every operation reads it from any processor without a
//! lock, and every mutating operation fails with [`FsError::ReadOnly`].

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use bytes::Bytes;
use helios_hal::fs::{
    DeviceId, DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult, Metadata, NodeId,
    NodeKind, OpenFlags,
};
use triomphe::Arc;

use crate::vfs::cmp_to_children;

/// Single immutable file view into the payload the backend hands the kernel.
#[derive(Clone, Copy)]
pub struct EmbeddedBootFile {
    path: &'static str,
    contents: &'static [u8],
    modified_nanos: u64,
}

/// Single immutable directory view into the payload the backend hands the
/// kernel.
#[derive(Clone, Copy)]
pub struct EmbeddedBootDirectory {
    path: &'static str,
    modified_nanos: u64,
}

/// Immutable read-only boot filesystem image.
#[derive(Clone, Copy)]
pub struct EmbeddedBootFs {
    directories: &'static [EmbeddedBootDirectory],
    files: &'static [EmbeddedBootFile],
}

impl EmbeddedBootFile {
    pub const fn new(path: &'static str, contents: &'static [u8], modified_nanos: u64) -> Self {
        Self {
            path,
            contents,
            modified_nanos,
        }
    }

    pub const fn path(&self) -> &'static str {
        self.path
    }

    pub const fn contents(&self) -> &'static [u8] {
        self.contents
    }

    pub const fn modified_nanos(&self) -> u64 {
        self.modified_nanos
    }
}

impl EmbeddedBootDirectory {
    pub const fn new(path: &'static str, modified_nanos: u64) -> Self {
        Self {
            path,
            modified_nanos,
        }
    }

    pub const fn path(&self) -> &'static str {
        self.path
    }

    pub const fn modified_nanos(&self) -> u64 {
        self.modified_nanos
    }
}

impl EmbeddedBootFs {
    pub const fn new(
        directories: &'static [EmbeddedBootDirectory],
        files: &'static [EmbeddedBootFile],
    ) -> Self {
        Self { directories, files }
    }

    /// An image with nothing in it, for a kernel whose payload carries no
    /// boot filesystem.
    pub const fn empty() -> Self {
        Self::new(&[], &[])
    }

    pub const fn directories(&self) -> &'static [EmbeddedBootDirectory] {
        self.directories
    }

    pub const fn files(&self) -> &'static [EmbeddedBootFile] {
        self.files
    }

    pub const fn is_empty(&self) -> bool {
        self.directories.is_empty() && self.files.is_empty()
    }
}

/// Inode number of the root directory. Entry `i` of the index is inode
/// `i + FIRST_ENTRY_INODE`.
const ROOT_INODE: u64 = 1;
const FIRST_ENTRY_INODE: u64 = 2;

/// One object of the image: a file, a directory the image lists, or a
/// directory only implied by the paths below it.
struct BootNode {
    /// Path from the image root. An implied directory's path is a prefix
    /// of the `'static` path that implies it, so no node owns its name.
    path: &'static str,
    kind: NodeKind,
    contents: &'static [u8],
    modified_nanos: u64,
}

/// The image as a mountable read-only filesystem.
#[derive(Clone)]
pub struct BootFs {
    index: Arc<BootIndex>,
}

struct BootIndex {
    device: DeviceId,
    /// Every object but the root, sorted by path bytes, so the objects
    /// below one directory form one contiguous run.
    nodes: Vec<BootNode>,
}

impl BootFs {
    /// Indexes `image` as device `device`.
    ///
    /// Panics when the image names one path as both a file and a
    /// directory: the payload is kernel input the build produced, and an
    /// image like that is a build defect, not something to serve.
    pub fn new(image: EmbeddedBootFs, device: DeviceId) -> Self {
        let mut nodes: Vec<BootNode> = Vec::new();
        for file in image.files() {
            push_implied_directories(&mut nodes, file.path());
            nodes.push(BootNode {
                path: file.path(),
                kind: NodeKind::File,
                contents: file.contents(),
                modified_nanos: file.modified_nanos(),
            });
        }
        for directory in image.directories() {
            push_implied_directories(&mut nodes, directory.path());
            nodes.push(BootNode {
                path: directory.path(),
                kind: NodeKind::Directory,
                contents: &[],
                modified_nanos: directory.modified_nanos(),
            });
        }
        // A listed directory keeps its own timestamp over the zero an
        // implied one carries: sort the listed one first, then keep the
        // first of each path.
        nodes.sort_by(|left, right| {
            left.path
                .cmp(right.path)
                .then_with(|| right.modified_nanos.cmp(&left.modified_nanos))
        });
        nodes.dedup_by(|later, earlier| {
            if later.path != earlier.path {
                return false;
            }
            assert!(
                later.kind == NodeKind::Directory && earlier.kind == NodeKind::Directory,
                "bootfs path {} is both a file and a directory",
                later.path
            );
            true
        });
        Self {
            index: Arc::new(BootIndex { device, nodes }),
        }
    }

    pub fn device(&self) -> DeviceId {
        self.index.device
    }

    /// The bytes of the file at `path`, borrowed from the payload.
    pub fn file_contents(&self, path: FsPath<'_>) -> FsResult<&'static [u8]> {
        let node = self.index.lookup(path)?;
        match node.map(|(_, node)| node) {
            Some(node) if node.kind == NodeKind::File => Ok(node.contents),
            Some(_) | None => Err(FsError::IsDirectory),
        }
    }
}

/// Pushes a directory node for every proper ancestor of `path`.
fn push_implied_directories(nodes: &mut Vec<BootNode>, path: &'static str) {
    for (end, _) in path.match_indices('/') {
        nodes.push(BootNode {
            path: &path[..end],
            kind: NodeKind::Directory,
            contents: &[],
            modified_nanos: 0,
        });
    }
}

impl BootIndex {
    /// The node at `path`: `Ok(None)` is the root, which has no node.
    fn lookup(&self, path: FsPath<'_>) -> FsResult<Option<(usize, &BootNode)>> {
        if path.is_root() {
            return Ok(None);
        }
        let index = self
            .nodes
            .binary_search_by(|node| node.path.cmp(path.as_str()))
            .map_err(|_| FsError::NoEntry)?;
        Ok(Some((index, &self.nodes[index])))
    }

    fn metadata(&self, path: FsPath<'_>) -> FsResult<Metadata> {
        Ok(match self.lookup(path)? {
            None => Metadata {
                id: NodeId::new(self.device, ROOT_INODE),
                kind: NodeKind::Directory,
                link_count: 1,
                size: 0,
                access_nanos: 0,
                modified_nanos: 0,
                status_nanos: 0,
            },
            Some((index, node)) => Metadata {
                id: NodeId::new(self.device, entry_inode(index)),
                kind: node.kind,
                link_count: 1,
                size: node.contents.len() as u64,
                access_nanos: node.modified_nanos,
                modified_nanos: node.modified_nanos,
                status_nanos: node.modified_nanos,
            },
        })
    }

    /// The run of nodes below the directory at `path`.
    fn descendants(&self, path: FsPath<'_>) -> core::ops::Range<usize> {
        if path.is_root() {
            return 0..self.nodes.len();
        }
        let directory = path.as_str();
        let start = self
            .nodes
            .partition_point(|node| cmp_to_children(node.path, directory) == Ordering::Less);
        let end = start
            + self.nodes[start..]
                .partition_point(|node| cmp_to_children(node.path, directory) == Ordering::Equal);
        start..end
    }

    fn read_dir(
        &self,
        path: FsPath<'_>,
        cookie: DirCookie,
        max_entries: usize,
    ) -> FsResult<Vec<DirEntry>> {
        if let Some((_, node)) = self.lookup(path)?
            && node.kind != NodeKind::Directory
        {
            return Err(FsError::NotDirectory);
        }
        let prefix_len = if path.is_root() {
            0
        } else {
            path.as_str().len() + 1
        };
        let run = self.descendants(path);
        let resume = usize::try_from(cookie.raw()).map_err(|_| FsError::Invalid)?;
        let mut entries = Vec::new();
        for index in run.start.max(resume)..run.end {
            if entries.len() == max_entries {
                break;
            }
            let node = &self.nodes[index];
            let name = &node.path[prefix_len..];
            if name.contains('/') {
                continue;
            }
            entries.push(DirEntry {
                name: String::from(name),
                kind: node.kind,
                inode: entry_inode(index),
                next: DirCookie::new(index as u64 + 1),
            });
        }
        Ok(entries)
    }
}

const fn entry_inode(index: usize) -> u64 {
    index as u64 + FIRST_ENTRY_INODE
}

impl FileSystem for BootFs {
    async fn stat(&self, path: FsPath<'_>) -> FsResult<Metadata> {
        self.index.metadata(path)
    }

    async fn open(&self, path: FsPath<'_>, flags: OpenFlags, _now: u64) -> FsResult<Metadata> {
        let metadata = match self.index.metadata(path) {
            Ok(metadata) => metadata,
            Err(FsError::NoEntry) if flags.contains(OpenFlags::CREATE) => {
                return Err(FsError::ReadOnly);
            }
            Err(error) => return Err(error),
        };
        if flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUSIVE) {
            return Err(FsError::Exists);
        }
        if flags.contains(OpenFlags::DIRECTORY) && metadata.kind != NodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        if flags.contains(OpenFlags::TRUNCATE) {
            return Err(FsError::ReadOnly);
        }
        Ok(metadata)
    }

    async fn read_at(&self, path: FsPath<'_>, offset: u64, max_bytes: usize) -> FsResult<Bytes> {
        let contents = self.file_contents(path)?;
        let offset = usize::try_from(offset).map_err(|_| FsError::Overflow)?;
        if offset >= contents.len() {
            return Ok(Bytes::new());
        }
        let end = offset.saturating_add(max_bytes).min(contents.len());
        Ok(Bytes::from_static(&contents[offset..end]))
    }

    async fn write_at(&self, _: FsPath<'_>, _: u64, _: &[u8], _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn append(&self, _: FsPath<'_>, _: &[u8], _: u64) -> FsResult<u64> {
        Err(FsError::ReadOnly)
    }

    async fn set_size(&self, _: FsPath<'_>, _: u64, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn set_times(
        &self,
        _: FsPath<'_>,
        _: Option<u64>,
        _: Option<u64>,
        _: u64,
    ) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn read_dir(
        &self,
        path: FsPath<'_>,
        cookie: DirCookie,
        max_entries: usize,
    ) -> FsResult<Vec<DirEntry>> {
        self.index.read_dir(path, cookie, max_entries)
    }

    async fn create_directory(&self, _: FsPath<'_>, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn remove_directory(&self, _: FsPath<'_>) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn unlink(&self, _: FsPath<'_>) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn rename(&self, _: FsPath<'_>, _: FsPath<'_>, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn link(&self, _: FsPath<'_>, _: FsPath<'_>, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn symlink(&self, _: &str, _: FsPath<'_>, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn read_link(&self, path: FsPath<'_>) -> FsResult<String> {
        // The image format carries no symbolic links.
        self.index.metadata(path)?;
        Err(FsError::Invalid)
    }

    async fn sync(&self, path: FsPath<'_>) -> FsResult<()> {
        // Nothing here was ever written, so everything is already durable.
        self.index.metadata(path).map(drop)
    }

    async fn sync_data(&self, path: FsPath<'_>) -> FsResult<()> {
        self.index.metadata(path).map(drop)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;

    use futures_lite::future::block_on;
    use helios_artifact::bootfs::{EntryKind, WriteEntry, write_image};
    use helios_hal::fs::{DeviceId, DirCookie, FileSystem, FsError, FsPath, NodeKind, OpenFlags};

    use super::{BootFs, EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs};

    const IMAGE: EmbeddedBootFs = EmbeddedBootFs::new(
        &[EmbeddedBootDirectory::new("empty", 7)],
        &[
            EmbeddedBootFile::new("bin/init.program", b"init", 3),
            EmbeddedBootFile::new("lib/runtime.component", b"runtime", 0),
            EmbeddedBootFile::new("etc/config.txt", b"config", 0),
            EmbeddedBootFile::new("lib/nested/deep.txt", b"deep", 0),
            // Sorts between `lib` and `lib/…`: the run below `lib` must
            // still be found.
            EmbeddedBootFile::new("lib.txt", b"sibling", 0),
        ],
    );

    fn boot() -> BootFs {
        BootFs::new(IMAGE, DeviceId::new(1))
    }

    fn path(path: &str) -> FsPath<'_> {
        FsPath::new(path).expect("test paths are valid")
    }

    fn names(fs: &BootFs, directory: &str) -> Vec<(String, NodeKind)> {
        block_on(fs.read_dir(path(directory), DirCookie::START, usize::MAX))
            .expect("directory lists")
            .into_iter()
            .map(|entry| (entry.name, entry.kind))
            .collect()
    }

    /// A bootfs read back out of a real `helios-bootfs` image — the same
    /// parse the kernel applies to the payload a backend hands it.
    fn payload_image() -> EmbeddedBootFs {
        let bytes = write_image(&[
            WriteEntry {
                kind: EntryKind::InitComponent,
                path: "init",
                data: b"init",
                modified_nanos: 0,
            },
            WriteEntry {
                kind: EntryKind::InitArgv0,
                path: "init",
                data: b"",
                modified_nanos: 0,
            },
            WriteEntry {
                kind: EntryKind::File,
                path: "bin/init.txt",
                data: b"boot init\n",
                modified_nanos: 0,
            },
            WriteEntry {
                kind: EntryKind::File,
                path: "lib/module.txt",
                data: b"boot module\n",
                modified_nanos: 0,
            },
        ]);
        crate::BootPayload::parse(Box::leak(bytes.into_boxed_slice()))
            .expect("payload image must parse")
            .bootfs()
    }

    #[test]
    fn lists_listed_and_implied_directories_once() {
        let fs = boot();
        assert_eq!(
            names(&fs, ""),
            [
                (String::from("bin"), NodeKind::Directory),
                (String::from("empty"), NodeKind::Directory),
                (String::from("etc"), NodeKind::Directory),
                (String::from("lib"), NodeKind::Directory),
                (String::from("lib.txt"), NodeKind::File),
            ]
        );
        assert_eq!(
            names(&fs, "lib"),
            [
                (String::from("nested"), NodeKind::Directory),
                (String::from("runtime.component"), NodeKind::File),
            ]
        );
        assert_eq!(names(&fs, "empty"), []);
    }

    #[test]
    fn listing_resumes_from_a_cookie() {
        let fs = boot();
        let first = block_on(fs.read_dir(FsPath::ROOT, DirCookie::START, 2)).expect("lists");
        assert_eq!(first.len(), 2);
        let rest = block_on(fs.read_dir(FsPath::ROOT, first[1].next, usize::MAX)).expect("lists");
        let all: Vec<_> = first.iter().chain(&rest).map(|entry| &entry.name).collect();
        assert_eq!(all, ["bin", "empty", "etc", "lib", "lib.txt"]);
    }

    #[test]
    fn stat_reports_identity_size_and_listed_timestamps() {
        let fs = boot();
        let root = block_on(fs.stat(FsPath::ROOT)).expect("root stats");
        let file = block_on(fs.stat(path("bin/init.program"))).expect("file stats");
        let empty = block_on(fs.stat(path("empty"))).expect("listed directory stats");
        assert_eq!(root.kind, NodeKind::Directory);
        assert_eq!(
            (file.kind, file.size, file.modified_nanos),
            (NodeKind::File, 4, 3)
        );
        assert_eq!(empty.modified_nanos, 7);
        assert_eq!(file.id.device, DeviceId::new(1));
        assert_ne!(file.id, empty.id);
        assert_ne!(file.id, root.id);
        assert_eq!(
            block_on(fs.stat(path("bin/missing"))).map(drop),
            Err(FsError::NoEntry)
        );
    }

    #[test]
    fn reads_positionally_and_refuses_every_mutation() {
        let fs = boot();
        let read = block_on(fs.read_at(path("lib/runtime.component"), 2, 3)).expect("reads");
        assert_eq!(read.as_ref(), b"nti");
        let past_end = block_on(fs.read_at(path("lib/runtime.component"), 99, 3)).expect("reads");
        assert!(past_end.is_empty());
        assert_eq!(
            block_on(fs.write_at(path("etc/config.txt"), 0, b"x", 0)),
            Err(FsError::ReadOnly)
        );
        assert_eq!(
            block_on(fs.open(path("etc/new"), OpenFlags::CREATE, 0)).map(drop),
            Err(FsError::ReadOnly)
        );
        assert_eq!(
            block_on(fs.unlink(path("etc/config.txt"))),
            Err(FsError::ReadOnly)
        );
    }

    #[test]
    fn serves_a_parsed_payload() {
        let fs = BootFs::new(payload_image(), DeviceId::new(1));
        let read = block_on(fs.read_at(path("lib/module.txt"), 0, 64)).expect("reads");
        assert_eq!(read.as_ref(), b"boot module\n");
        assert_eq!(
            names(&fs, ""),
            [
                (String::from("bin"), NodeKind::Directory),
                (String::from("lib"), NodeKind::Directory),
            ]
        );
    }
}

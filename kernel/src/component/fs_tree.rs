//! The guest filesystem's node, and the boot payload's tree of them.
//!
//! The boot payload never changes after bring-up, so its tree is indexed
//! once, by [`BootfsTree::new`], and every guest filesystem reads the
//! same index as the base under its own changes instead of copying it.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use bytes::Bytes;
use hashbrown::HashMap;
use triomphe::Arc;

use crate::{
    AuthorityDomain, BootfsBytes, ComponentFsNodeKind, EmbeddedBootDirectory, EmbeddedBootFile,
    EmbeddedBootFs, ObjectIdentity,
};

/// What a node holds, and where it came from.
///
/// A file indexed from the boot payload holds [`BootfsBytes`]; every
/// write, truncate or creation stores [`FileContents::Written`], so a
/// node that anyone has modified can no longer claim the payload's trust.
/// Directories and symlinks hold `Written` bytes (empty, or the link
/// target).
#[derive(Clone, Debug)]
pub(crate) enum FileContents {
    Bootfs(BootfsBytes),
    Written(Bytes),
}

// Every node an instance creates or changes is its own copy, so carrying
// provenance must not make a node larger than the plain `Bytes` it
// replaced.
const _: () = assert!(
    core::mem::size_of::<FileContents>() == core::mem::size_of::<Bytes>(),
    "FileContents must stay the size of Bytes"
);

impl FileContents {
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Bootfs(bytes) => bytes.as_slice(),
            Self::Written(bytes) => bytes,
        }
    }

    pub(crate) fn slice(&self, range: core::ops::Range<usize>) -> Bytes {
        match self {
            Self::Bootfs(bytes) => Bytes::from_static(&bytes.as_slice()[range]),
            Self::Written(bytes) => bytes.slice(range),
        }
    }

    pub(crate) fn into_bytes(self) -> Bytes {
        match self {
            Self::Bootfs(bytes) => bytes.to_bytes(),
            Self::Written(bytes) => bytes,
        }
    }

    pub(crate) const fn empty() -> Self {
        Self::Written(Bytes::new())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FsNode {
    pub(crate) path: String,
    pub(crate) kind: ComponentFsNodeKind,
    pub(crate) contents: FileContents,
    pub(crate) identity: ObjectIdentity,
    pub(crate) access_nanos: u64,
    pub(crate) modified_nanos: u64,
    pub(crate) status_nanos: u64,
    pub(crate) readonly: bool,
    pub(crate) link_count: u64,
}

impl FsNode {
    pub(crate) fn new(
        path: String,
        kind: ComponentFsNodeKind,
        contents: FileContents,
        identity: ObjectIdentity,
        timestamp_nanos: u64,
        readonly: bool,
    ) -> Self {
        Self {
            path,
            kind,
            contents,
            identity,
            access_nanos: timestamp_nanos,
            modified_nanos: timestamp_nanos,
            status_nanos: timestamp_nanos,
            readonly,
            link_count: 1,
        }
    }

    pub(crate) fn touch_status(&mut self, now_nanos: u64) {
        self.status_nanos = now_nanos;
    }

    pub(crate) fn touch_modified(&mut self, now_nanos: u64) {
        self.modified_nanos = now_nanos;
        self.status_nanos = now_nanos;
    }

    pub(crate) fn set_times(
        &mut self,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
        status_nanos: u64,
    ) {
        if let Some(access_nanos) = access_nanos {
            self.access_nanos = access_nanos;
        }
        if let Some(modified_nanos) = modified_nanos {
            self.modified_nanos = modified_nanos;
        }
        if access_nanos.is_some() || modified_nanos.is_some() {
            self.status_nanos = status_nanos;
        }
    }
}

/// The boot payload's directory tree, indexed once at bring-up.
///
/// It holds the root, every bootfs directory and file, and the
/// directories their paths imply, in the order the payload lists them.
/// The node at index `i` has the [`AuthorityDomain::GUEST_BOOTFS`]
/// object identity `i + 1`, so the root is `1` and a guest filesystem
/// numbers what it creates from [`BootfsTree::next_inode`] on.
///
/// Concurrency: immutable once built. A clone is a reference-count
/// increment, and every processor reads the shared index without
/// synchronisation.
#[derive(Clone, Debug)]
pub struct BootfsTree {
    inner: Arc<BootfsTreeIndex>,
}

#[derive(Debug)]
struct BootfsTreeIndex {
    nodes: Vec<FsNode>,
    path_index: HashMap<String, usize>,
}

impl BootfsTree {
    pub fn new(image: EmbeddedBootFs) -> Self {
        let mut index = BootfsTreeIndex {
            nodes: Vec::new(),
            path_index: HashMap::new(),
        };
        index.insert(
            String::from("/"),
            ComponentFsNodeKind::Directory,
            FileContents::empty(),
            0,
            false,
        );
        for directory in image.directories() {
            index.insert_directory(directory);
        }
        for file in image.files() {
            index.insert_file(file);
        }
        index.nodes.shrink_to_fit();
        index.path_index.shrink_to_fit();
        Self {
            inner: Arc::new(index),
        }
    }

    /// Every node, in index order.
    pub(crate) fn nodes(&self) -> &[FsNode] {
        &self.inner.nodes
    }

    /// The index of the node at the absolute guest path `path`.
    pub(crate) fn index(&self, path: &str) -> Option<usize> {
        self.inner.path_index.get(path).copied()
    }

    /// The index of the node whose object identity is `identity`.
    pub(crate) fn index_of_identity(&self, identity: ObjectIdentity) -> Option<usize> {
        if identity.domain() != AuthorityDomain::GUEST_BOOTFS {
            return None;
        }
        let index = usize::try_from(identity.local()).ok()?.checked_sub(1)?;
        (index < self.inner.nodes.len()).then_some(index)
    }

    /// The first object identity no node of this tree holds.
    pub(crate) fn next_inode(&self) -> u64 {
        self.inner.nodes.len() as u64 + 1
    }

    /// Whether `self` and `other` are one shared index.
    #[cfg(test)]
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl BootfsTreeIndex {
    fn insert(
        &mut self,
        path: String,
        kind: ComponentFsNodeKind,
        contents: FileContents,
        timestamp_nanos: u64,
        readonly: bool,
    ) {
        let index = self.nodes.len();
        let identity = ObjectIdentity::new(AuthorityDomain::GUEST_BOOTFS, index as u64 + 1);
        assert!(
            self.path_index.insert(path.clone(), index).is_none(),
            "bootfs path {path} indexed twice"
        );
        self.nodes.push(FsNode::new(
            path,
            kind,
            contents,
            identity,
            timestamp_nanos,
            readonly,
        ));
    }

    fn insert_directory(&mut self, directory: &EmbeddedBootDirectory) {
        let absolute = embedded_absolute_path(directory.path());
        self.ensure_ancestors(directory.path(), &absolute);
        self.ensure_directory(&absolute, directory.modified_nanos());
    }

    fn insert_file(&mut self, file: &EmbeddedBootFile) {
        let absolute = embedded_absolute_path(file.path());
        self.ensure_ancestors(file.path(), &absolute);
        if self.path_index.contains_key(&absolute) {
            return;
        }
        self.insert(
            absolute,
            ComponentFsNodeKind::File,
            FileContents::Bootfs(file.bootfs_bytes()),
            file.modified_nanos(),
            true,
        );
    }

    /// Indexes every directory above `absolute`, the guest path of the
    /// payload entry `relative`.
    fn ensure_ancestors(&mut self, relative: &str, absolute: &str) {
        let mut current = String::from("/");
        for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
            let next = join_embedded_child(&current, segment);
            if next == absolute {
                break;
            }
            self.ensure_directory(&next, 0);
            current = next;
        }
    }

    /// Indexes the read-only directory `path`, or stamps an indexed one
    /// with `modified_nanos` when the payload names it with a time.
    fn ensure_directory(&mut self, path: &str, modified_nanos: u64) {
        if path == "/" {
            return;
        }
        if let Some(&index) = self.path_index.get(path) {
            let existing = &mut self.nodes[index];
            assert!(
                existing.kind == ComponentFsNodeKind::Directory,
                "bootfs directory path {path} collided with an existing file"
            );
            existing.readonly = true;
            if modified_nanos != 0 {
                existing.access_nanos = modified_nanos;
                existing.modified_nanos = modified_nanos;
                existing.status_nanos = modified_nanos;
            }
            return;
        }
        self.insert(
            path.to_owned(),
            ComponentFsNodeKind::Directory,
            FileContents::empty(),
            modified_nanos,
            true,
        );
    }
}

fn embedded_absolute_path(relative: &str) -> String {
    let mut path = String::with_capacity(relative.len() + 1);
    path.push('/');
    path.push_str(relative);
    path
}

fn join_embedded_child(parent: &str, child: &str) -> String {
    let slash = usize::from(parent != "/");
    let mut path = String::with_capacity(parent.len() + slash + child.len());
    path.push_str(parent);
    if parent != "/" {
        path.push('/');
    }
    path.push_str(child);
    path
}

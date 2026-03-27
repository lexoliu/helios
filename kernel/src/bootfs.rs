extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use helios_hal::fs::{
    Directory, DirectoryEntry, DirectoryHandle, DirectoryRights, File, FileHandle, FileRights,
    FileSystem, IoError, IoResult,
};
use helios_hal::resource::KernelResource;

/// Single immutable file embedded into the kernel image at compile time.
#[derive(Clone, Copy)]
pub struct EmbeddedBootFile {
    path: &'static str,
    contents: &'static [u8],
}

/// Immutable read-only boot filesystem image.
///
/// The image is intentionally simple: a sorted flat file list. Directories are
/// derived from path prefixes, which keeps the compiled representation compact
/// and architecture-neutral.
#[derive(Clone, Copy)]
pub struct EmbeddedBootFs {
    files: &'static [EmbeddedBootFile],
}

/// Read-only directory view into an [`EmbeddedBootFs`].
#[derive(Clone)]
pub struct BootDirectory {
    image: EmbeddedBootFs,
    path: String,
}

/// Read-only file view into an [`EmbeddedBootFs`].
#[derive(Clone)]
pub struct BootFile {
    path: &'static str,
    contents: &'static [u8],
    offset: usize,
}

/// Directory entry returned by bootfs enumeration.
pub struct BootDirectoryEntry {
    name: String,
    is_directory: bool,
}

/// Operations available on a capability-clamped bootfs directory handle.
pub trait BootDirectoryHandleExt {
    fn list(&self) -> Vec<BootDirectoryEntry>;
    fn open_directory(
        &self,
        path: &str,
        rights: DirectoryRights,
    ) -> Option<DirectoryHandle<BootDirectory>>;
    fn open_file(&self, path: &str, rights: FileRights) -> Option<FileHandle<BootFile>>;
}

impl EmbeddedBootFile {
    pub const fn new(path: &'static str, contents: &'static [u8]) -> Self {
        Self { path, contents }
    }

    pub const fn path(&self) -> &'static str {
        self.path
    }

    pub const fn contents(&self) -> &'static [u8] {
        self.contents
    }
}

impl EmbeddedBootFs {
    pub const fn new(files: &'static [EmbeddedBootFile]) -> Self {
        Self { files }
    }

    pub const fn files(&self) -> &'static [EmbeddedBootFile] {
        self.files
    }

    pub const fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Creates the root directory capability for this boot filesystem.
    ///
    /// Bootfs is immutable, so only read rights are valid here.
    pub fn root_directory(self, rights: DirectoryRights) -> DirectoryHandle<BootDirectory> {
        assert!(
            DirectoryRights::READ.contains(rights),
            "bootfs root directory only supports read rights"
        );
        KernelResource::new(BootDirectory::root(self), rights)
    }

    fn directory_exists(&self, path: &str) -> bool {
        if path.is_empty() {
            return true;
        }

        let prefix = with_trailing_slash(path);
        self.files.iter().any(|file| file.path.starts_with(&prefix))
    }

    fn file(&self, path: &str) -> Option<BootFile> {
        self.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| BootFile {
                path: file.path,
                contents: file.contents,
                offset: 0,
            })
    }
}

impl BootDirectory {
    fn root(image: EmbeddedBootFs) -> Self {
        Self {
            image,
            path: String::new(),
        }
    }

    pub fn path(&self) -> &str {
        if self.path.is_empty() {
            "/"
        } else {
            &self.path
        }
    }

    fn resolve_path(&self, path: &str) -> String {
        normalize_path(Some(&self.path), path)
    }
}

impl BootFile {
    pub const fn path(&self) -> &'static str {
        self.path
    }

    pub const fn contents(&self) -> &'static [u8] {
        self.contents
    }
}

impl BootDirectoryEntry {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_directory(&self) -> bool {
        self.is_directory
    }
}

impl BootDirectoryHandleExt for DirectoryHandle<BootDirectory> {
    fn list(&self) -> Vec<BootDirectoryEntry> {
        assert!(
            self.rights().contains(DirectoryRights::READ),
            "bootfs directory listing requires read rights"
        );

        let directory = self.object();
        let prefix = if directory.path.is_empty() {
            String::new()
        } else {
            with_trailing_slash(&directory.path)
        };
        let mut entries = Vec::new();

        for file in directory.image.files() {
            let Some(remainder) = file.path.strip_prefix(&prefix) else {
                continue;
            };
            if remainder.is_empty() {
                continue;
            }

            let (name, is_directory) = match remainder.split_once('/') {
                Some((name, _)) => (name, true),
                None => (remainder, false),
            };

            if entries
                .iter()
                .any(|entry: &BootDirectoryEntry| entry.name == name)
            {
                continue;
            }

            entries.push(BootDirectoryEntry {
                name: name.to_owned(),
                is_directory,
            });
        }

        entries
    }

    fn open_directory(
        &self,
        path: &str,
        rights: DirectoryRights,
    ) -> Option<DirectoryHandle<BootDirectory>> {
        assert!(
            self.rights().contains(rights),
            "requested bootfs directory rights exceed parent capability"
        );

        let path = self.object().resolve_path(path);
        self.object().image.directory_exists(&path).then(|| {
            KernelResource::new(
                BootDirectory {
                    image: self.object().image,
                    path,
                },
                rights,
            )
        })
    }

    fn open_file(&self, path: &str, rights: FileRights) -> Option<FileHandle<BootFile>> {
        assert!(
            self.rights().contains(DirectoryRights::READ),
            "bootfs file lookup requires read rights"
        );
        assert!(
            FileRights::READ.contains(rights),
            "bootfs files only support read rights"
        );

        let path = self.object().resolve_path(path);
        self.object()
            .image
            .file(&path)
            .map(|file| KernelResource::new(file, rights))
    }
}

impl FileSystem for EmbeddedBootFs {
    type Directory = BootDirectory;
    type File = BootFile;

    fn open(
        &self,
        path: &str,
    ) -> impl core::future::Future<
        Output = IoResult<Option<DirectoryEntry<Self::Directory, Self::File>>>,
    > + Send {
        let path = normalize_path(None, path);
        let image = *self;

        async move {
            if image.directory_exists(&path) {
                return Ok(Some(DirectoryEntry::Directory(BootDirectory {
                    image,
                    path,
                })));
            }

            Ok(image.file(&path).map(DirectoryEntry::File))
        }
    }
}

impl Directory for BootDirectory {
    type File = BootFile;

    fn list(
        &self,
    ) -> impl core::future::Future<Output = IoResult<Vec<DirectoryEntry<Self, Self::File>>>> + Send
    where
        Self: Sized,
    {
        let directory = self.clone();

        async move {
            let prefix = if directory.path.is_empty() {
                String::new()
            } else {
                with_trailing_slash(&directory.path)
            };
            let mut entries: Vec<DirectoryEntry<BootDirectory, BootFile>> = Vec::new();

            for file in directory.image.files() {
                let Some(remainder) = file.path.strip_prefix(&prefix) else {
                    continue;
                };
                if remainder.is_empty() {
                    continue;
                }

                match remainder.split_once('/') {
                    Some((name, _)) => {
                        let next_path = if directory.path.is_empty() {
                            name.to_owned()
                        } else {
                            format!("{}/{}", directory.path, name)
                        };
                        if entries.iter().any(|entry| match entry {
                            DirectoryEntry::Directory(existing) => existing.path == next_path,
                            DirectoryEntry::File(existing) => existing.path == next_path,
                        }) {
                            continue;
                        }

                        entries.push(DirectoryEntry::Directory(BootDirectory {
                            image: directory.image,
                            path: next_path,
                        }));
                    }
                    None => {
                        entries.push(DirectoryEntry::File(BootFile {
                            path: file.path,
                            contents: file.contents,
                            offset: 0,
                        }));
                    }
                }
            }

            Ok(entries)
        }
    }
}

impl File for BootFile {
    fn read(
        &mut self,
        buf: &mut [u8],
    ) -> impl core::future::Future<Output = IoResult<usize>> + Send {
        let remaining = &self.contents[self.offset..];
        let count = remaining.len().min(buf.len());
        buf[..count].copy_from_slice(&remaining[..count]);
        self.offset += count;

        async move { Ok(count) }
    }

    fn write(&mut self, _buf: &[u8]) -> impl core::future::Future<Output = IoResult<usize>> + Send {
        async move { Err(IoError::ReadOnly) }
    }
}

fn normalize_path(base: Option<&str>, path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();

    if !path.starts_with('/') {
        if let Some(base) = base {
            for segment in base.split('/') {
                if !segment.is_empty() {
                    segments.push(segment);
                }
            }
        }
    }

    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => panic!("bootfs paths must not contain parent traversal"),
            _ => segments.push(segment),
        }
    }

    segments.join("/")
}

fn with_trailing_slash(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }

    let mut prefixed = path.to_owned();
    prefixed.push('/');
    prefixed
}

#[cfg(test)]
mod tests {
    extern crate std;

    use futures_lite::future::block_on;
    use helios_kernel_macro::bootfs;

    use super::{BootDirectoryHandleExt, EmbeddedBootFile, EmbeddedBootFs};
    use helios_hal::fs::{
        Directory, DirectoryEntry, DirectoryRights, File, FileRights, FileSystem,
    };
    use helios_hal::io::IoError;

    const IMAGE: EmbeddedBootFs = EmbeddedBootFs::new(&[
        EmbeddedBootFile::new("bin/init.wasm", b"init"),
        EmbeddedBootFile::new("lib/runtime.wasm", b"runtime"),
        EmbeddedBootFile::new("etc/config.txt", b"config"),
    ]);
    const MACRO_IMAGE: EmbeddedBootFs = bootfs!("src/bootfs_test_data");

    #[test]
    fn lists_root_entries() {
        let root = IMAGE.root_directory(DirectoryRights::READ);
        let entries = root.list();

        assert_eq!(entries.len(), 3);
        assert!(
            entries
                .iter()
                .any(|entry| entry.name() == "bin" && entry.is_directory())
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.name() == "lib" && entry.is_directory())
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.name() == "etc" && entry.is_directory())
        );
    }

    #[test]
    fn opens_nested_files() {
        let root = IMAGE.root_directory(DirectoryRights::READ);
        let lib = root
            .open_directory("lib", DirectoryRights::READ)
            .expect("embedded lib directory must exist");
        let runtime = lib
            .open_file("runtime.wasm", FileRights::READ)
            .expect("embedded runtime.wasm must exist");

        assert_eq!(runtime.object().path(), "lib/runtime.wasm");
        assert_eq!(runtime.object().contents(), b"runtime");
    }

    #[test]
    fn packs_directory_tree_with_macro() {
        let root = MACRO_IMAGE.root_directory(DirectoryRights::READ);
        let bin = root
            .open_directory("bin", DirectoryRights::READ)
            .expect("macro-packed bin directory must exist");
        let init = bin
            .open_file("init.txt", FileRights::READ)
            .expect("macro-packed init.txt must exist");
        let lib = root
            .open_directory("lib", DirectoryRights::READ)
            .expect("macro-packed lib directory must exist");
        let module = lib
            .open_file("module.txt", FileRights::READ)
            .expect("macro-packed module.txt must exist");

        assert_eq!(init.object().contents(), b"boot init\n");
        assert_eq!(module.object().contents(), b"boot module\n");
    }

    #[test]
    fn exposes_read_only_filesystem_trait() {
        let entry = block_on(MACRO_IMAGE.open("/lib/module.txt"))
            .expect("bootfs open should succeed")
            .expect("module.txt must exist");
        let mut file = match entry {
            DirectoryEntry::File(file) => file,
            DirectoryEntry::Directory(_) => panic!("expected file entry"),
        };
        let mut buf = [0_u8; 32];
        let read = block_on(file.read(&mut buf)).expect("bootfs read should succeed");
        let write = block_on(file.write(b"mutate"));

        assert_eq!(&buf[..read], b"boot module\n");
        assert!(matches!(write, Err(IoError::ReadOnly)));
    }

    #[test]
    fn lists_directories_via_filesystem_trait() {
        let entry = block_on(MACRO_IMAGE.open("/"))
            .expect("bootfs open should succeed")
            .expect("bootfs root must exist");
        let directory = match entry {
            DirectoryEntry::Directory(directory) => directory,
            DirectoryEntry::File(_) => panic!("expected root directory"),
        };
        let entries = block_on(directory.list()).expect("bootfs list should succeed");

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| matches!(
            entry,
            DirectoryEntry::Directory(directory) if directory.path() == "bin"
        )));
        assert!(entries.iter().any(|entry| matches!(
            entry,
            DirectoryEntry::Directory(directory) if directory.path() == "lib"
        )));
    }
}

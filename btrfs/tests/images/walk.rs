//! A walk over every reachable inode: every directory listed and each of
//! its names looked up, every file read, every symlink and xattr read.

use core::ops::ControlFlow;

use helios_btrfs::{DirPosition, EntryTarget, Error, FileKind, Filesystem};

use crate::fixture::{SparseImage, block_on, read_all};

/// What a whole walk saw.
#[derive(Debug, Default)]
pub struct Census {
    pub directories: usize,
    pub files: usize,
    pub symlinks: usize,
    pub subvolumes: usize,
    pub file_bytes: u64,
}

/// Walks every inode reachable from the default subvolume's root,
/// crossing into other subvolumes, and stops at the first error.
pub fn walk(fs: &Filesystem<SparseImage>) -> Result<Census, Error> {
    let mut census = Census::default();
    let mut pending = vec![block_on(fs.root())?];
    while let Some(inode) = pending.pop() {
        let mut names = Vec::new();
        block_on(fs.list_xattrs(&inode, |name| {
            names.push(name.to_vec());
            ControlFlow::Continue(())
        }))?;
        for name in names {
            let mut value = [0; 256];
            let found = block_on(fs.xattr(&inode, &name, &mut value))?;
            assert!(found.is_some(), "a listed xattr is found by name");
        }
        match inode.kind() {
            FileKind::Directory => {
                census.directories += 1;
                let mut entries = Vec::new();
                block_on(fs.read_dir(&inode, DirPosition::START, |entry| {
                    entries.push((entry.name.to_vec(), entry.kind, entry.target));
                    ControlFlow::Continue(())
                }))?;
                for (name, kind, target) in entries {
                    let child = block_on(fs.lookup(&inode, &name))?
                        .expect("a listed name is found by lookup");
                    assert_eq!(child.kind(), kind, "entry type matches the inode");
                    match target {
                        EntryTarget::Inode(number) => assert_eq!(child.number(), number),
                        EntryTarget::Subvolume(tree) => {
                            census.subvolumes += 1;
                            assert_eq!(child.tree(), tree);
                        }
                    }
                    pending.push(child);
                }
            }
            FileKind::Regular => {
                census.files += 1;
                census.file_bytes += read_all(fs, &inode, 64 << 10)?.len() as u64;
            }
            FileKind::Symlink => {
                census.symlinks += 1;
                let mut target = [0; 4096];
                block_on(fs.read_link(&inode, &mut target))?;
            }
            _ => {}
        }
    }
    Ok(census)
}

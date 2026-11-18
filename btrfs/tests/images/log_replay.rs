//! Log replay of images Linux's own btrfs left with a log pending, held to
//! what Linux's replay of the same image produces. `tools/btrfs/fstests/
//! log-replay.sh` makes them as root on Linux, each case a directory with
//! `pending.img` (stopped after an fsync, before a commit) and `linux.img`
//! (that image mounted and unmounted by Linux); HELIOS_BTRFS_LOG_IMAGES
//! names the directory. A replay matches when every subvolume holds the same
//! names at the same directory indices, every inode the same attributes,
//! data, xattrs and link targets, and `btrfs check` passes the result.
//! Change times, and modification times of directories, are not compared:
//! both replays stamp their own clock on what they unlink.

use std::collections::BTreeSet;
use std::fs::File;
use std::ops::ControlFlow;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use helios_btrfs::write::Writer;
use helios_btrfs::{
    BlockSink, BlockSource, DirPosition, EntryTarget, Error, FileKind, Filesystem, Inode,
    READ_ALIGNMENT, Timestamp,
};
use helios_hal::io::{IoError, IoResult};

use super::fixture::block_on;

/// An image file, read and written in place.
struct ImageFile(File);

impl BlockSource for ImageFile {
    fn size_bytes(&self) -> u64 {
        self.0.metadata().unwrap().len()
    }

    async fn read_at(&self, offset: u64, output: &mut [u8]) -> IoResult<()> {
        self.0
            .read_exact_at(output, offset)
            .map_err(|_| IoError::DeviceFault)
    }
}

impl BlockSink for ImageFile {
    async fn stage(&self, block: u64, bytes: &[u8; READ_ALIGNMENT as usize]) -> IoResult<()> {
        self.0
            .write_all_at(bytes, block * READ_ALIGNMENT)
            .map_err(|_| IoError::DeviceFault)
    }

    async fn commit(&self, block: u64, bytes: &[u8; READ_ALIGNMENT as usize]) -> IoResult<()> {
        self.0.sync_all().map_err(|_| IoError::DeviceFault)?;
        self.stage(block, bytes).await?;
        self.0.sync_all().map_err(|_| IoError::DeviceFault)
    }
}

/// Linux writes every root item with a valid UUID, so no commit asks.
fn no_uuid() -> [u8; 16] {
    panic!("Linux's root items carry valid UUIDs")
}

/// When Helios mounts the images.
const MOUNTED: Timestamp = Timestamp {
    seconds: 1_800_000_000,
    nanoseconds: 0,
};

fn open_read(path: &Path) -> ImageFile {
    ImageFile(File::open(path).unwrap_or_else(|error| panic!("{}: {error}", path.display())))
}

fn open_rw(path: &Path) -> ImageFile {
    ImageFile(
        File::options()
            .read(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display())),
    )
}

#[test]
#[ignore = "needs the images tools/btrfs/fstests/log-replay.sh makes as root on Linux; the test-btrfs-fstests CI lane runs it with --ignored"]
fn helios_replays_linux_logs_as_linux_does() {
    let images = std::env::var_os("HELIOS_BTRFS_LOG_IMAGES")
        .expect("HELIOS_BTRFS_LOG_IMAGES names the directory log-replay.sh wrote");
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&images)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "{images:?} holds no case");
    for case in cases {
        block_on(replay_case(&case));
    }
}

async fn replay_case(case: &Path) {
    let name = case.display();
    // The read-only mount refuses what it cannot read as committed.
    assert!(
        matches!(
            Filesystem::mount(open_read(&case.join("pending.img"))).await,
            Err(Error::LogReplayRequired { .. })
        ),
        "{name}: pending.img has no pending log"
    );
    let replayed = tempfile::NamedTempFile::new().unwrap();
    std::fs::copy(case.join("pending.img"), replayed.path()).unwrap();
    let writer = Writer::mount(
        open_rw(replayed.path()),
        no_uuid as fn() -> [u8; 16],
        std::thread::available_parallelism().unwrap(),
        MOUNTED,
    )
    .await
    .unwrap_or_else(|error| panic!("{name}: replay failed: {error}"));
    drop(writer);

    let ours = Filesystem::mount(open_read(replayed.path())).await.unwrap();
    let theirs = Filesystem::mount(open_read(&case.join("linux.img")))
        .await
        .unwrap();
    let subvolumes = async |fs: &Filesystem<ImageFile>| {
        let mut found = Vec::new();
        fs.subvolumes(|tree, root| found.push((tree, root)))
            .await
            .unwrap();
        found
    };
    let trees = subvolumes(&ours).await;
    assert_eq!(trees, subvolumes(&theirs).await, "{name}: subvolumes");
    for (tree, _) in trees {
        let mut visited = BTreeSet::new();
        let mut pending = vec![(
            String::from("/"),
            ours.subvolume_root(tree).await.unwrap(),
            theirs.subvolume_root(tree).await.unwrap(),
        )];
        while let Some((path, mine, linux)) = pending.pop() {
            let at = format!("{name}: subvolume {tree}, {path}");
            assert_eq!(mine.number(), linux.number(), "{at}: inode number");
            if !visited.insert(mine.number()) {
                continue;
            }
            compare_inodes(&ours, &mine, &theirs, &linux, &at).await;
            if mine.kind() != FileKind::Directory {
                continue;
            }
            let (mine_entries, linux_entries) =
                (entries(&ours, &mine).await, entries(&theirs, &linux).await);
            assert_eq!(mine_entries, linux_entries, "{at}: entries");
            for (_, entry, _, target) in mine_entries {
                let EntryTarget::Inode(number) = target else {
                    continue;
                };
                pending.push((
                    format!("{path}{}/", String::from_utf8_lossy(&entry)),
                    ours.inode(tree, number).await.unwrap(),
                    theirs.inode(tree, number).await.unwrap(),
                ));
            }
        }
    }

    let check = std::process::Command::new("btrfs")
        .args(["check", "--readonly"])
        .arg(replayed.path())
        .output()
        .expect("btrfs-progs must be installed");
    assert!(
        check.status.success(),
        "{name}: btrfs check\n{}\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
}

/// A directory's entries, in index order.
async fn entries(
    fs: &Filesystem<ImageFile>,
    dir: &Inode,
) -> Vec<(DirPosition, Vec<u8>, FileKind, EntryTarget)> {
    let mut entries = Vec::new();
    fs.read_dir(dir, DirPosition::START, |entry| {
        entries.push((
            entry.position,
            entry.name.to_vec(),
            entry.kind,
            entry.target,
        ));
        ControlFlow::Continue(())
    })
    .await
    .unwrap();
    entries
}

async fn compare_inodes(
    ours: &Filesystem<ImageFile>,
    mine: &Inode,
    theirs: &Filesystem<ImageFile>,
    linux: &Inode,
    at: &str,
) {
    let (a, b) = (mine.metadata(), linux.metadata());
    assert_eq!(
        (
            a.kind,
            a.permissions,
            a.uid,
            a.gid,
            a.nlink,
            a.size,
            a.nbytes
        ),
        (
            b.kind,
            b.permissions,
            b.uid,
            b.gid,
            b.nlink,
            b.size,
            b.nbytes
        ),
        "{at}: attributes"
    );
    assert_eq!(
        (a.rdev, a.flags, a.generation, a.atime, a.otime),
        (b.rdev, b.flags, b.generation, b.atime, b.otime),
        "{at}: identity and times"
    );
    if a.kind != FileKind::Directory {
        assert_eq!(a.mtime, b.mtime, "{at}: modification time");
    }
    assert_eq!(
        xattrs(ours, mine).await,
        xattrs(theirs, linux).await,
        "{at}: xattrs"
    );
    match a.kind {
        FileKind::Regular => {
            let mut left = vec![0; a.size as usize];
            let mut right = vec![0; b.size as usize];
            ours.read(mine, 0, &mut left).await.unwrap();
            theirs.read(linux, 0, &mut right).await.unwrap();
            assert!(left == right, "{at}: data");
        }
        FileKind::Symlink => {
            let mut left = vec![0; 4096];
            let mut right = vec![0; 4096];
            let length = ours.read_link(mine, &mut left).await.unwrap();
            assert_eq!(theirs.read_link(linux, &mut right).await.unwrap(), length);
            assert_eq!(left[..length], right[..length], "{at}: target");
        }
        _ => {}
    }
}

async fn xattrs(fs: &Filesystem<ImageFile>, inode: &Inode) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut names = Vec::new();
    fs.list_xattrs(inode, |name| {
        names.push(name.to_vec());
        ControlFlow::Continue(())
    })
    .await
    .unwrap();
    let mut values = Vec::new();
    for name in names {
        let mut value = vec![0; 65536];
        let length = fs.xattr(inode, &name, &mut value).await.unwrap().unwrap();
        value.truncate(length);
        values.push((name, value));
    }
    values.sort();
    values
}

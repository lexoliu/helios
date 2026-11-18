//! Uses only the public writable API against the independent image fixture.

use std::sync::Mutex;

use helios_btrfs::write::{Attributes, CloneRange, Name, WriteError, WriteOptions, Writer, Zstd};
use helios_btrfs::{BlockSink, BlockSource, READ_ALIGNMENT, Timestamp};
use helios_hal::fs::RenameMode;
use helios_hal::io::IoResult;

use super::fixture::{SparseImage, base_image, block_on};

struct WritableImage(Mutex<SparseImage>);

impl BlockSource for WritableImage {
    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().size_bytes()
    }

    async fn read_at(&self, offset: u64, output: &mut [u8]) -> IoResult<()> {
        self.0.lock().unwrap().read_bytes(offset, output);
        Ok(())
    }
}

impl BlockSink for WritableImage {
    async fn stage(&self, block: u64, bytes: &[u8; READ_ALIGNMENT as usize]) -> IoResult<()> {
        self.0
            .lock()
            .unwrap()
            .write_bytes(block * READ_ALIGNMENT, bytes);
        Ok(())
    }

    async fn commit(&self, block: u64, bytes: &[u8; READ_ALIGNMENT as usize]) -> IoResult<()> {
        self.stage(block, bytes).await
    }
}

fn assert_send<T: Send>(_: &T) {}

/// The images' root records carry valid UUIDs, so no commit asks for one.
fn no_uuid() -> [u8; 16] {
    panic!("the image's root records carry valid UUIDs")
}

type ImageWriter = Writer<WritableImage, fn() -> [u8; 16]>;

async fn mount(image: SparseImage) -> Result<ImageWriter, WriteError> {
    Writer::mount(
        WritableImage(Mutex::new(image)),
        no_uuid as fn() -> [u8; 16],
        std::thread::available_parallelism().unwrap(),
    )
    .await
}

/// A writer over the image `mkfs.btrfs` produces for the guest smoke (128 MiB,
/// default features) commits a created file and reads it back.
#[test]
#[ignore = "needs mkfs.btrfs from btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn writer_commits_over_a_mkfs_image() {
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("hello.txt"), b"hello btrfs\n").unwrap();
    let image = super::mkfs::mkfs_sized(tree.path(), 128 << 20, &[]);
    block_on(async {
        let mut writer = mount(image).await.unwrap();
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let when = Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 0,
        };
        let concurrency = std::thread::available_parallelism().unwrap();
        let file = tx
            .create_file(
                Name {
                    parent: &root,
                    bytes: b"written",
                },
                Attributes {
                    permissions: 0o644,
                    uid: 0,
                    gid: 0,
                },
                when,
            )
            .await
            .unwrap();
        tx.write_file(
            &file,
            0,
            b"guest bytes",
            WriteOptions::plain(when, concurrency),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let snapshot = writer.snapshot().unwrap();
        let written = snapshot.resolve(b"/written").await.unwrap();
        let mut output = [0; 11];
        snapshot.read(&written, 0, &mut output).await.unwrap();
        assert_eq!(&output, b"guest bytes");

        // The guest smoke's sequence: a directory, a file in it, then a hard
        // link to that file once both are committed.
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let directory = tx
            .create_directory(
                Name {
                    parent: &root,
                    bytes: "目录-ディレクトリ-디렉터리".as_bytes(),
                },
                Attributes {
                    permissions: 0o755,
                    uid: 0,
                    gid: 0,
                },
                when,
            )
            .await
            .unwrap();
        let original = tx
            .create_file(
                Name {
                    parent: &directory,
                    bytes: b"original",
                },
                Attributes {
                    permissions: 0o644,
                    uid: 0,
                    gid: 0,
                },
                when,
            )
            .await
            .unwrap();
        tx.write_file(
            &original,
            0,
            &[0x47; 8192],
            WriteOptions::plain(when, concurrency),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let directory = tx
            .lookup(&root, "目录-ディレクトリ-디렉터리".as_bytes())
            .await
            .unwrap()
            .unwrap();
        let original = tx.lookup(&directory, b"original").await.unwrap().unwrap();
        assert_eq!(
            tx.metadata(&original).await.unwrap().kind,
            helios_btrfs::FileKind::Regular
        );
        tx.hard_link(
            Name {
                parent: &directory,
                bytes: b"hard",
            },
            &original,
            when,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            writer
                .snapshot()
                .unwrap()
                .resolve("/目录-ディレクトリ-디렉터리/hard".as_bytes())
                .await
                .unwrap()
                .metadata()
                .nlink,
            2
        );
    });
}

#[test]
fn public_writer_preserves_snapshots_and_committed_handles() {
    block_on(async {
        let mut writer = mount(base_image()).await.unwrap();
        let before = writer.snapshot().unwrap();
        let join = writer.join();
        assert_send(&join);
        let mut tx = join.await.unwrap();
        let root = tx.root().await.unwrap();
        let when = Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 123,
        };
        let concurrency = std::thread::available_parallelism().unwrap();
        let attributes = |permissions| Attributes {
            permissions,
            uid: 1000,
            gid: 1000,
        };
        let directory = tx
            .create_directory(
                Name {
                    parent: &root,
                    bytes: "中文-日本語-한국어".as_bytes(),
                },
                attributes(0o755),
                when,
            )
            .await
            .unwrap();
        let name = |bytes| Name {
            parent: &directory,
            bytes,
        };
        let source = tx
            .create_file(name(b"source"), attributes(0o640), when)
            .await
            .unwrap();
        let data = vec![0x53; 32768];
        let mut encoder = Zstd::new(3).unwrap();
        let write = tx.write_file(
            &source,
            0,
            &data,
            WriteOptions {
                when,
                concurrency,
                encoder: &mut encoder,
            },
        );
        assert_send(&write);
        let count = write.await.unwrap();
        assert_eq!(count, data.len());
        let destination = tx
            .create_file(name(b"clone"), attributes(0o640), when)
            .await
            .unwrap();
        tx.clone_range(
            CloneRange {
                source: &source,
                destination: &destination,
                source_offset: 0,
                destination_offset: 0,
                length: data.len() as u64,
            },
            when,
            concurrency,
        )
        .await
        .unwrap();
        let looked_up = tx.lookup(&directory, b"clone").await.unwrap().unwrap();
        assert_eq!(looked_up.number(), destination.number());
        assert_eq!(
            tx.metadata(&looked_up).await.unwrap().size,
            data.len() as u64
        );
        let mut output = vec![0; data.len()];
        assert_eq!(
            tx.read_file(&looked_up, 0, &mut output).await.unwrap(),
            data.len()
        );
        assert_eq!(output, data);
        let commit = tx.commit();
        assert_send(&commit);
        commit.await.unwrap();
        assert!(
            before
                .lookup(
                    &before.root().await.unwrap(),
                    "中文-日本語-한국어".as_bytes()
                )
                .await
                .unwrap()
                .is_none()
        );
        let shared = writer.snapshot().unwrap();
        let old_source = shared
            .resolve("/中文-日本語-한국어/source".as_bytes())
            .await
            .unwrap();
        let mut tx = writer.join().await.unwrap();
        let _ = tx
            .write_file(
                &source,
                0,
                b"changed",
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap();
        tx.rename(name(b"clone"), name(b"kept"), RenameMode::Replace, when)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let current = writer.snapshot().unwrap();
        let kept = current
            .resolve("/中文-日本語-한국어/kept".as_bytes())
            .await
            .unwrap();
        current.read(&kept, 0, &mut output).await.unwrap();
        assert_eq!(output, data);
        current.read(&old_source, 0, &mut output).await.unwrap();
        assert_eq!(output, data);
        let source = current
            .resolve("/中文-日本語-한국어/source".as_bytes())
            .await
            .unwrap();
        current.read(&source, 0, &mut output).await.unwrap();
        assert_eq!(&output[..7], b"changed");
        assert_eq!(&output[7..], &data[7..]);
    });
}

/// `mkfs.btrfs -O quota` creates the quota tree. A writable mount refuses it,
/// since the writer keeps no qgroup counts, and a read-only mount still opens
/// the image. `-O squota` sets the simple-quota incompat bit, which neither
/// mount supports.
#[test]
#[ignore = "needs mkfs.btrfs from btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn writer_refuses_mkfs_images_with_quotas_and_the_reader_mounts_them() {
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("hello.txt"), b"hello btrfs\n").unwrap();

    let image = super::mkfs::mkfs_sized(tree.path(), 128 << 20, &["-O", "quota"]);
    super::fixture::mount(image.clone()).expect("a read-only mount ignores quotas");
    let refused = block_on(mount(image));
    assert!(
        matches!(refused, Err(WriteError::QuotaTreePresent)),
        "-O quota: refused with {:?}",
        refused.err()
    );

    let image = super::mkfs::mkfs_sized(tree.path(), 128 << 20, &["-O", "squota"]);
    let squota = |error: &helios_btrfs::Error| {
        matches!(
            error,
            helios_btrfs::Error::UnsupportedIncompat { name: "squota", .. }
        )
    };
    let read = super::fixture::mount(image.clone()).err();
    assert!(
        read.as_ref().is_some_and(squota),
        "-O squota read-only: {read:?}"
    );
    let write = block_on(mount(image)).err();
    assert!(
        matches!(&write, Some(WriteError::Read(error)) if squota(error)),
        "-O squota writable: {write:?}"
    );
}

/// The image a test keeps a handle on while the writer owns the other.
struct SharedImage(std::sync::Arc<WritableImage>);

impl BlockSource for SharedImage {
    fn size_bytes(&self) -> u64 {
        self.0.size_bytes()
    }

    async fn read_at(&self, offset: u64, output: &mut [u8]) -> IoResult<()> {
        self.0.read_at(offset, output).await
    }
}

impl BlockSink for SharedImage {
    async fn stage(&self, block: u64, bytes: &[u8; READ_ALIGNMENT as usize]) -> IoResult<()> {
        self.0.stage(block, bytes).await
    }

    async fn commit(&self, block: u64, bytes: &[u8; READ_ALIGNMENT as usize]) -> IoResult<()> {
        self.0.commit(block, bytes).await
    }
}

/// Runs `sudo` without a password prompt; the node and the attribute below
/// need privileges the test user lacks.
fn sudo(args: &[&str]) {
    let output = std::process::Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("running sudo {args:?}: {error}"));
    assert!(
        output.status.success(),
        "sudo {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// What a file's metadata and security attribute say, for comparing views.
#[derive(Debug, PartialEq)]
struct Authority {
    kind: helios_btrfs::FileKind,
    permissions: u32,
    uid: u32,
    gid: u32,
    rdev: helios_btrfs::DeviceNumber,
    nlink: u32,
    size: u64,
    ctime: Timestamp,
    mtime: Timestamp,
    security: Option<Vec<u8>>,
}

/// A Linux image carries a setuid file, a character device node and a
/// `security.*` attribute (lexoliu/helios#1037). Mounted writable, the writer
/// keeps all three intact through unrelated writes, and `btrfs check` is clean.
#[test]
#[ignore = "needs mkfs.btrfs and btrfs check from btrfs-progs, passwordless sudo, and setfattr; the Linux CI lane runs it with --run-ignored"]
fn linux_authority_survives_unrelated_writes() {
    const NAMES: [&str; 3] = ["中文新文件", "日本語の新しいファイル", "한국어새파일"];
    const LABEL: &[u8] = b"system_u:object_r:helios_t:s0";

    let tree = tempfile::tempdir().unwrap();
    let path = |name: &str| tree.path().join(name);
    let utf8 = |name: &str| path(name).to_str().unwrap().to_owned();
    std::fs::write(path("suid"), b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(
        path("suid"),
        std::os::unix::fs::PermissionsExt::from_mode(0o4755),
    )
    .unwrap();
    // /dev/null's numbers; the node is never opened.
    sudo(&["mknod", &utf8("中文设备"), "c", "1", "3"]);
    std::fs::write(path("日本語のファイル"), "こんにちは\n").unwrap();
    std::fs::write(path("한국어파일"), "안녕하세요\n").unwrap();
    std::fs::write(path("doomed"), b"unlinked below").unwrap();
    std::fs::write(path("moved"), b"renamed below").unwrap();
    for name in ["suid", "中文设备"] {
        sudo(&[
            "setfattr",
            "-n",
            "security.helios",
            "-v",
            std::str::from_utf8(LABEL).unwrap(),
            &utf8(name),
        ]);
    }
    let image = super::mkfs::mkfs_sized(tree.path(), 128 << 20, &[]);
    let shared = std::sync::Arc::new(WritableImage(Mutex::new(image)));

    block_on(async {
        let mut writer = Writer::mount(
            SharedImage(shared.clone()),
            no_uuid as fn() -> [u8; 16],
            std::thread::available_parallelism().unwrap(),
        )
        .await
        .unwrap();
        let authority = async |writer: &Writer<SharedImage, fn() -> [u8; 16]>, name: &str| {
            let view = writer.snapshot().unwrap();
            let inode = view.resolve(format!("/{name}").as_bytes()).await.unwrap();
            let metadata = *inode.metadata();
            let mut value = [0; 64];
            let security = view
                .xattr(&inode, b"security.helios", &mut value)
                .await
                .unwrap()
                .map(|length| value[..length].to_vec());
            Authority {
                kind: metadata.kind,
                permissions: metadata.permissions,
                uid: metadata.uid,
                gid: metadata.gid,
                rdev: metadata.rdev,
                nlink: metadata.nlink,
                size: metadata.size,
                ctime: metadata.ctime,
                mtime: metadata.mtime,
                security,
            }
        };
        let suid = authority(&writer, "suid").await;
        let device = authority(&writer, "中文设备").await;
        assert_eq!(suid.permissions, 0o4755);
        assert_eq!(device.kind, helios_btrfs::FileKind::CharDevice);
        // `mkfs.btrfs --rootdir` stores every rdev as 0 (btrfs-progs
        // mkfs/rootdir.c), so only its preservation below is checked.
        assert_eq!(suid.security.as_deref(), Some(LABEL));
        assert_eq!(device.security.as_deref(), Some(LABEL));

        // Create, write, rename and unlink elsewhere.
        let when = Timestamp {
            seconds: 1_800_000_000,
            nanoseconds: 0,
        };
        let concurrency = std::thread::available_parallelism().unwrap();
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let at = |parent, bytes| Name { parent, bytes };
        let directory = tx
            .create_directory(
                at(&root, "目录-ディレクトリ-디렉터리".as_bytes()),
                Attributes {
                    permissions: 0o755,
                    uid: 0,
                    gid: 0,
                },
                when,
            )
            .await
            .unwrap();
        for name in NAMES {
            let file = tx
                .create_file(
                    at(&directory, name.as_bytes()),
                    Attributes {
                        permissions: 0o644,
                        uid: 0,
                        gid: 0,
                    },
                    when,
                )
                .await
                .unwrap();
            tx.write_file(
                &file,
                0,
                name.as_bytes(),
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap();
        }
        let (source, destination) = (at(&root, b"moved"), at(&directory, b"moved"));
        tx.rename(source, destination, RenameMode::Replace, when)
            .await
            .unwrap();
        tx.unlink_name(at(&root, b"doomed"), when).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(authority(&writer, "suid").await, suid);
        assert_eq!(authority(&writer, "中文设备").await, device);
        let view = writer.snapshot().unwrap();
        assert!(view.resolve(b"/doomed").await.is_err());
        view.resolve("/目录-ディレクトリ-디렉터리/moved".as_bytes())
            .await
            .unwrap();
    });

    btrfs_check(&shared);
}

/// Copies the image to a file, which `btrfs check` must find clean.
fn btrfs_check(image: &WritableImage) -> tempfile::NamedTempFile {
    let scratch = tempfile::NamedTempFile::new().unwrap();
    {
        use std::io::Seek;
        let mut image = image.0.lock().unwrap();
        image.seek(std::io::SeekFrom::Start(0)).unwrap();
        std::io::copy(&mut *image, &mut scratch.reopen().unwrap()).unwrap();
    }
    let check = std::process::Command::new("btrfs")
        .args(["check", "--readonly"])
        .arg(scratch.path())
        .output()
        .expect("btrfs-progs must be installed");
    assert!(
        check.status.success(),
        "btrfs check\n{}\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    scratch
}

/// Helios sets and removes the Unix authority Linux stores (lexoliu/helios#1037):
/// set-id and sticky bits by chmod and at creation, device nodes, FIFOs and
/// sockets by mknod, and the set-id bits a chown drops. `btrfs check` is
/// clean, and Linux's btrfs mounts the image and reads back what Helios
/// wrote.
#[test]
#[ignore = "needs mkfs.btrfs and btrfs check from btrfs-progs, passwordless sudo, and a Linux kernel with btrfs to loop-mount; the Linux CI lane runs it with --run-ignored"]
fn linux_reads_back_the_authority_helios_sets() {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    use helios_btrfs::DeviceNumber;
    use helios_btrfs::write::Special;

    const NAMES: [&str; 3] = ["中文", "日本語", "한국어"];
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("chmod-me"), b"#!/bin/sh\n").unwrap();
    std::fs::write(tree.path().join("chown-me"), b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(
        tree.path().join("chown-me"),
        std::os::unix::fs::PermissionsExt::from_mode(0o6755),
    )
    .unwrap();
    let image = super::mkfs::mkfs_sized(tree.path(), 128 << 20, &[]);
    let shared = std::sync::Arc::new(WritableImage(Mutex::new(image)));
    let null = DeviceNumber::new(1, 3).unwrap();
    let wide = DeviceNumber::new(259, 0x1_2345).unwrap();
    let when = Timestamp {
        seconds: 1_800_000_000,
        nanoseconds: 0,
    };
    // What Linux must read back: name, file type and permission bits of
    // `st_mode`, owner, group and `st_rdev`.
    let mut expected: Vec<(String, u32, u32, u32, u64)> = Vec::new();
    block_on(async {
        let mut writer = Writer::mount(
            SharedImage(shared.clone()),
            no_uuid as fn() -> [u8; 16],
            std::thread::available_parallelism().unwrap(),
        )
        .await
        .unwrap();
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        fn at<'a>(parent: &'a helios_btrfs::write::InodeHandle, name: &'a str) -> Name<'a> {
            Name {
                parent,
                bytes: name.as_bytes(),
            }
        }
        let chmod = tx.lookup(&root, b"chmod-me").await.unwrap().unwrap();
        tx.set_permissions(&chmod, 0o7755, when).await.unwrap();
        expected.push(("chmod-me".to_owned(), 0o107_755, 0, 0, 0));
        let chown = tx.lookup(&root, b"chown-me").await.unwrap().unwrap();
        tx.set_owner(&chown, Some(1000), Some(1000), when)
            .await
            .unwrap();
        expected.push(("chown-me".to_owned(), 0o100_755, 1000, 1000, 0));
        for name in NAMES {
            let owner = |permissions| Attributes {
                permissions,
                uid: 1000,
                gid: 100,
            };
            let file = format!("suid-{name}");
            tx.create_file(at(&root, &file), owner(0o4755), when)
                .await
                .unwrap();
            expected.push((file, 0o104_755, 1000, 100, 0));
            let directory = format!("sgid-{name}");
            tx.create_directory(at(&root, &directory), owner(0o3775), when)
                .await
                .unwrap();
            expected.push((directory, 0o043_775, 1000, 100, 0));
            for (prefix, special, mode, rdev) in [
                ("chr", Special::CharDevice(null), 0o020_660, null),
                ("blk", Special::BlockDevice(wide), 0o060_660, wide),
                ("fifo", Special::Fifo, 0o010_660, DeviceNumber::default()),
                ("sock", Special::Socket, 0o140_660, DeviceNumber::default()),
            ] {
                let node = format!("{prefix}-{name}");
                tx.create_special(at(&root, &node), special, owner(0o660), when)
                    .await
                    .unwrap();
                expected.push((node, mode, 1000, 100, u64::from(rdev.encode())));
            }
            // Made and removed again, as `rm` takes a node away.
            let gone = format!("gone-{name}");
            let device = Special::CharDevice(null);
            tx.create_special(at(&root, &gone), device, owner(0o600), when)
                .await
                .unwrap();
            tx.unlink_name(at(&root, &gone), when).await.unwrap();
        }
        tx.commit().await.unwrap();
    });

    let image = btrfs_check(&shared);
    let mount = tempfile::tempdir().unwrap();
    let point = mount.path().to_str().unwrap().to_owned();
    sudo(&[
        "mount",
        "-t",
        "btrfs",
        "-o",
        "loop,ro",
        image.path().to_str().unwrap(),
        &point,
    ]);
    // Read everything before asserting anything, so the image is always
    // unmounted.
    let read = (|| {
        let mut seen = Vec::new();
        for (name, ..) in &expected {
            let metadata = std::fs::symlink_metadata(mount.path().join(name))?;
            seen.push((
                name.clone(),
                metadata.mode(),
                metadata.uid(),
                metadata.gid(),
                metadata.rdev(),
            ));
        }
        let mut removed = Vec::new();
        for name in NAMES {
            removed.push(!mount.path().join(format!("gone-{name}")).exists());
            let fifo = std::fs::symlink_metadata(mount.path().join(format!("fifo-{name}")))?;
            removed.push(fifo.file_type().is_fifo());
        }
        std::io::Result::Ok((seen, removed))
    })();
    sudo(&["umount", &point]);
    let (seen, checks) = read.unwrap();
    assert_eq!(seen, expected);
    assert!(checks.into_iter().all(|held| held));
}

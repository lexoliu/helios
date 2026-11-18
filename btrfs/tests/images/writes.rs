//! Uses only the public writable API against the independent image fixture.

use std::sync::Mutex;

use helios_btrfs::write::{
    Attributes, CloneRange, DEFAULT_MAX_INLINE, Name, WriteError, WriteOptions, Writer, Zstd,
};
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
        MOUNTED,
    )
    .await
}

/// When the images are mounted; none has a log for it to stamp.
const MOUNTED: Timestamp = Timestamp {
    seconds: 1_700_000_000,
    nanoseconds: 0,
};

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
                max_inline: DEFAULT_MAX_INLINE,
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
            WriteOptions::plain(when, concurrency),
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
    rdev: u64,
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
            MOUNTED,
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

    assert_btrfs_check(&shared.0);
}

fn assert_btrfs_check(shared: &Mutex<SparseImage>) {
    let scratch = tempfile::NamedTempFile::new().unwrap();
    {
        use std::io::Seek;
        let mut image = shared.lock().unwrap();
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
}

fn inline_text(length: usize) -> Vec<u8> {
    let mut bytes = "内联数据 インラインデータ 인라인 데이터 "
        .repeat(length)
        .into_bytes();
    bytes.truncate(length);
    bytes
}

fn inline_noise(length: usize, seed: u32) -> Vec<u8> {
    let mut state = seed;
    (0..length)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

fn zstd_stored_len(content: &[u8]) -> u64 {
    use helios_btrfs::write::Encoder;
    Zstd::new(3)
        .unwrap()
        .encode(content, 4096)
        .unwrap()
        .expect("compressible content")
        .bytes
        .len() as u64
}

fn exercise_inline_limits(image: SparseImage) -> std::sync::Arc<WritableImage> {
    use helios_btrfs::write::Plain;
    use helios_btrfs::{Compression, ExtentKind};
    use std::collections::BTreeMap;

    const SECTOR: u64 = 4096;
    const WHEN: Timestamp = Timestamp {
        seconds: 1_700_000_200,
        nanoseconds: 0,
    };

    let shared = std::sync::Arc::new(WritableImage(Mutex::new(image)));
    block_on(async {
        let concurrency = std::thread::available_parallelism().unwrap();
        let mut writer = Writer::mount(
            SharedImage(shared.clone()),
            no_uuid as fn() -> [u8; 16],
            concurrency,
            MOUNTED,
        )
        .await
        .unwrap();
        let nodesize = u64::from(writer.snapshot().unwrap().superblock().nodesize());
        // `BTRFS_MAX_INLINE_DATA_SIZE` (`fs/btrfs/ctree.h`): `nodesize` minus
        // `btrfs_header` (101) + `btrfs_item` (25) + inline `btrfs_file_extent_item` header (21).
        let max_node_inline = (SECTOR - 1).min(nodesize - 147);
        let mut encoder = Zstd::new(3).unwrap();
        let mut expected: BTreeMap<&str, (Vec<u8>, Option<Option<Compression>>)> = BTreeMap::new();

        let plain = |max_inline| WriteOptions {
            when: WHEN,
            concurrency,
            encoder: Plain,
            max_inline,
        };
        let attrs = Attributes {
            permissions: 0o644,
            uid: 1000,
            gid: 1000,
        };

        // Write paths: uncompressed & compressed, at and across limits.
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let dir = tx
            .create_directory(
                Name {
                    parent: &root,
                    bytes: "内联-インライン-인라인".as_bytes(),
                },
                Attributes {
                    permissions: 0o755,
                    uid: 1000,
                    gid: 1000,
                },
                WHEN,
            )
            .await
            .unwrap();
        for (name, content, max_inline, layout) in [
            (
                "plain_at",
                inline_noise(2048, 1),
                DEFAULT_MAX_INLINE,
                Some(None),
            ),
            (
                "plain_over",
                inline_noise(2049, 2),
                DEFAULT_MAX_INLINE,
                None,
            ),
            ("plain_sector", inline_noise(4096, 3), 4096, None),
            (
                "plain_node_at",
                inline_noise(max_node_inline as usize, 4),
                4096,
                Some(None),
            ),
            (
                "plain_node_over",
                inline_noise(max_node_inline as usize + 1, 5),
                4096,
                None,
            ),
            ("plain_zero", inline_noise(16, 6), 0, None),
            (
                "append_plain_at",
                inline_noise(2000, 7),
                DEFAULT_MAX_INLINE,
                Some(None),
            ),
            (
                "append_plain_over",
                inline_noise(2000, 8),
                DEFAULT_MAX_INLINE,
                None,
            ),
            (
                "trunc_plain_at",
                inline_noise(100, 9),
                DEFAULT_MAX_INLINE,
                Some(None),
            ),
            (
                "trunc_plain_over",
                inline_noise(100, 10),
                DEFAULT_MAX_INLINE,
                None,
            ),
            ("trunc_plain_sector", inline_noise(100, 11), 4096, None),
            (
                "clone_dest_inline",
                inline_noise(100, 12),
                DEFAULT_MAX_INLINE,
                Some(None),
            ),
            (
                "clone_dest_off_sector",
                inline_noise(100, 13),
                DEFAULT_MAX_INLINE,
                None,
            ),
        ] {
            let file = tx
                .create_file(
                    Name {
                        parent: &dir,
                        bytes: name.as_bytes(),
                    },
                    attrs,
                    WHEN,
                )
                .await
                .unwrap();
            tx.write_file(&file, 0, &content, plain(max_inline))
                .await
                .unwrap();
            expected.insert(name, (content, layout));
        }

        let comp_3000 = inline_text(3000);
        let comp_3000_len = zstd_stored_len(&comp_3000);
        let mut padded_trunc = inline_text(2000);
        padded_trunc.resize(SECTOR as usize, 0);
        let padded_trunc_len = zstd_stored_len(&padded_trunc);
        for (name, content, max_inline, layout) in [
            (
                "comp_sector",
                inline_text(4096),
                DEFAULT_MAX_INLINE,
                Some(Some(Compression::Zstd)),
            ),
            (
                "comp_over_sector",
                inline_text(4097),
                DEFAULT_MAX_INLINE,
                None,
            ),
            (
                "comp_at",
                comp_3000.clone(),
                comp_3000_len,
                Some(Some(Compression::Zstd)),
            ),
            ("comp_over", comp_3000, comp_3000_len - 1, None),
            (
                "comp_noise",
                inline_noise(3000, 14),
                DEFAULT_MAX_INLINE,
                None,
            ),
            (
                "append_comp_at",
                inline_text(2000),
                DEFAULT_MAX_INLINE,
                Some(Some(Compression::Zstd)),
            ),
            (
                "append_comp_over",
                inline_text(2000),
                DEFAULT_MAX_INLINE,
                None,
            ),
            (
                "trunc_comp_at",
                inline_text(2000),
                DEFAULT_MAX_INLINE,
                Some(Some(Compression::Zstd)),
            ),
            (
                "trunc_comp_over",
                inline_text(2000),
                DEFAULT_MAX_INLINE,
                None,
            ),
            (
                "trunc_comp_sector_over",
                inline_text(2000),
                DEFAULT_MAX_INLINE,
                None,
            ),
        ] {
            let file = tx
                .create_file(
                    Name {
                        parent: &dir,
                        bytes: name.as_bytes(),
                    },
                    attrs,
                    WHEN,
                )
                .await
                .unwrap();
            tx.write_file(
                &file,
                0,
                &content,
                WriteOptions {
                    when: WHEN,
                    concurrency,
                    encoder: &mut encoder,
                    max_inline,
                },
            )
            .await
            .unwrap();
            expected.insert(name, (content, layout));
        }
        tx.commit().await.unwrap();

        // Append & truncate up paths.
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let dir = tx
            .lookup(&root, "内联-インライン-인라인".as_bytes())
            .await
            .unwrap()
            .unwrap();
        let open = async |tx: &helios_btrfs::write::WriteTransaction<'_, _, _>, name: &str| {
            tx.lookup(&dir, name.as_bytes()).await.unwrap().unwrap()
        };

        let f = open(&tx, "append_plain_at").await;
        let tail_48 = inline_noise(48, 15);
        tx.write_file(&f, 2000, &tail_48, plain(DEFAULT_MAX_INLINE))
            .await
            .unwrap();
        expected
            .get_mut("append_plain_at")
            .unwrap()
            .0
            .extend_from_slice(&tail_48);

        let f = open(&tx, "append_plain_over").await;
        let tail_49 = inline_noise(49, 16);
        tx.write_file(&f, 2000, &tail_49, plain(DEFAULT_MAX_INLINE))
            .await
            .unwrap();
        expected
            .get_mut("append_plain_over")
            .unwrap()
            .0
            .extend_from_slice(&tail_49);

        let comp_joined = inline_text(4096);
        let comp_joined_len = zstd_stored_len(&comp_joined);
        let comp_tail = &comp_joined[2000..];
        let f = open(&tx, "append_comp_at").await;
        tx.write_file(
            &f,
            2000,
            comp_tail,
            WriteOptions {
                when: WHEN,
                concurrency,
                encoder: &mut encoder,
                max_inline: comp_joined_len,
            },
        )
        .await
        .unwrap();
        expected
            .get_mut("append_comp_at")
            .unwrap()
            .0
            .extend_from_slice(comp_tail);

        let f = open(&tx, "append_comp_over").await;
        tx.write_file(
            &f,
            2000,
            comp_tail,
            WriteOptions {
                when: WHEN,
                concurrency,
                encoder: &mut encoder,
                max_inline: comp_joined_len - 1,
            },
        )
        .await
        .unwrap();
        expected
            .get_mut("append_comp_over")
            .unwrap()
            .0
            .extend_from_slice(comp_tail);

        for (name, size, max_inline) in [
            ("trunc_plain_at", 2048, DEFAULT_MAX_INLINE),
            ("trunc_plain_over", 2049, DEFAULT_MAX_INLINE),
            ("trunc_plain_sector", SECTOR, 4096),
        ] {
            let f = open(&tx, name).await;
            tx.truncate_file(&f, size, plain(max_inline)).await.unwrap();
            expected.get_mut(name).unwrap().0.resize(size as usize, 0);
        }
        for (name, size, max_inline) in [
            ("trunc_comp_at", SECTOR, padded_trunc_len),
            ("trunc_comp_over", SECTOR, padded_trunc_len - 1),
            ("trunc_comp_sector_over", SECTOR + 1, DEFAULT_MAX_INLINE),
        ] {
            let f = open(&tx, name).await;
            tx.truncate_file(
                &f,
                size,
                WriteOptions {
                    when: WHEN,
                    concurrency,
                    encoder: &mut encoder,
                    max_inline,
                },
            )
            .await
            .unwrap();
            expected.get_mut(name).unwrap().0.resize(size as usize, 0);
        }
        tx.commit().await.unwrap();

        // Reflink paths: inline source at/across `max_inline`, at offset 4096,
        // and regular source promoting an inline destination.
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let dir = tx
            .lookup(&root, "内联-インライン-인라인".as_bytes())
            .await
            .unwrap()
            .unwrap();
        let open = async |tx: &helios_btrfs::write::WriteTransaction<'_, _, _>, name: &str| {
            tx.lookup(&dir, name.as_bytes()).await.unwrap().unwrap()
        };

        let comp_sector_len = zstd_stored_len(&inline_text(4096));
        for (source_name, dest_name, dest_offset, max_inline, layout) in [
            (
                "plain_at",
                "clone_plain_at",
                0,
                DEFAULT_MAX_INLINE,
                Some(None),
            ),
            (
                "plain_at",
                "clone_plain_over",
                0,
                DEFAULT_MAX_INLINE - 1,
                None,
            ),
            (
                "plain_at",
                "clone_plain_off_sector",
                SECTOR,
                DEFAULT_MAX_INLINE,
                None,
            ),
            (
                "comp_sector",
                "clone_comp_at",
                0,
                comp_sector_len,
                Some(Some(Compression::Zstd)),
            ),
            (
                "comp_sector",
                "clone_comp_over",
                0,
                comp_sector_len - 1,
                None,
            ),
        ] {
            let from = open(&tx, source_name).await;
            let to = tx
                .create_file(
                    Name {
                        parent: &dir,
                        bytes: dest_name.as_bytes(),
                    },
                    attrs,
                    WHEN,
                )
                .await
                .unwrap();
            let src_bytes = expected[source_name].0.clone();
            tx.clone_range(
                CloneRange {
                    source: &from,
                    destination: &to,
                    source_offset: 0,
                    destination_offset: dest_offset,
                    length: src_bytes.len() as u64,
                },
                plain(max_inline),
            )
            .await
            .unwrap();
            let mut content = vec![0; dest_offset as usize];
            content.extend_from_slice(&src_bytes);
            expected.insert(dest_name, (content, layout));
        }

        let from_plain = open(&tx, "plain_at").await;
        let to_inline = open(&tx, "clone_dest_inline").await;
        let src_bytes = expected["plain_at"].0.clone();
        tx.clone_range(
            CloneRange {
                source: &from_plain,
                destination: &to_inline,
                source_offset: 0,
                destination_offset: 0,
                length: src_bytes.len() as u64,
            },
            plain(DEFAULT_MAX_INLINE),
        )
        .await
        .unwrap();
        expected.get_mut("clone_dest_inline").unwrap().0 = src_bytes;

        let from_reg = open(&tx, "plain_over").await;
        let to_off = open(&tx, "clone_dest_off_sector").await;
        let reg_bytes = expected["plain_over"].0.clone();
        tx.clone_range(
            CloneRange {
                source: &from_reg,
                destination: &to_off,
                source_offset: 0,
                destination_offset: SECTOR,
                length: reg_bytes.len() as u64,
            },
            plain(DEFAULT_MAX_INLINE),
        )
        .await
        .unwrap();
        let dest_bytes = &mut expected.get_mut("clone_dest_off_sector").unwrap().0;
        dest_bytes.resize(SECTOR as usize, 0);
        dest_bytes.extend_from_slice(&reg_bytes);

        tx.commit().await.unwrap();

        let view = writer.snapshot().unwrap();
        for (name, (bytes, layout)) in &expected {
            let path = format!("/内联-インライン-인라인/{name}");
            let inode = view.resolve(path.as_bytes()).await.unwrap();
            assert_eq!(inode.metadata().size, bytes.len() as u64, "{name}");
            let mut actual = vec![0; bytes.len()];
            assert_eq!(
                view.read(&inode, 0, &mut actual).await.unwrap(),
                bytes.len(),
                "{name}"
            );
            assert!(actual == *bytes, "{name}: content mismatch");
            let extents = view.file_extents(&inode).await.unwrap();
            for extent in extents.iter().filter(|e| e.kind == ExtentKind::Inline) {
                assert!(extent.stored <= max_node_inline, "{name}: {extent:?}");
                if extent.compression.is_none() {
                    assert_eq!(extent.stored, extent.len, "{name}: {extent:?}");
                } else {
                    assert!(extent.len <= SECTOR, "{name}: {extent:?}");
                }
            }
            match layout {
                Some(compression) => {
                    assert_eq!(extents.len(), 1, "{name}: {extents:?}");
                    assert_eq!(extents[0].kind, ExtentKind::Inline, "{name}: {extents:?}");
                    assert_eq!(extents[0].compression, *compression, "{name}: {extents:?}");
                }
                None => assert!(
                    extents.iter().all(|e| e.kind != ExtentKind::Inline),
                    "{name}: {extents:?}"
                ),
            }
        }
    });
    shared
}

#[test]
fn public_writer_enforces_inline_extent_limits() {
    let _ = exercise_inline_limits(base_image());
}

#[test]
#[ignore = "needs mkfs.btrfs and btrfs check from btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn inline_extent_limits_pass_btrfs_check() {
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("seed.txt"), b"seed\n").unwrap();
    for args in [&[][..], &["--nodesize", "4096"][..]] {
        let image = super::mkfs::mkfs_sized(tree.path(), 128 << 20, args);
        let shared = exercise_inline_limits(image);
        assert_btrfs_check(&shared.0);
    }
}

//! Extended attributes through the writer: the operations, Linux's limits and
//! errors, hash-sharing items, namespaces, removal with the inode, and what the
//! reader sees after a commit.

extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use ferrosys::{Metadata, Timestamp as ImageTime, TreeBuilder};
use futures_lite::future::block_on;
use zerocopy::{FromZeros, IntoBytes};

use super::{Disk, FixtureUuids, assert_btrfs_check, filesystem_image_tree_file, mount_writer};
use crate::csum::name_hash;
use crate::fs::writer::{Attributes, InodeHandle, Name, WriteTransaction, Writer};
use crate::ondisk::{RawDirItem, RawInodeItem, RawKey, item_type, view};
use crate::tree::write::WriteError;
use crate::write::XattrMode;
use crate::{Key, Timestamp};

/// Two names with one CRC32C name hash (`0x4e84071b`), found by search.
const COLLIDING: [&[u8]; 2] = [b"user.k1371838", b"user.k2000402"];

const CJK: [(&str, &str); 3] = [
    ("user.中文", "你好，世界"),
    ("user.日本語", "こんにちは世界"),
    ("user.한국어", "안녕하세요 세계"),
];

const ACL: &[u8] = b"system.posix_acl_access";

fn when(second: i64) -> Timestamp {
    Timestamp {
        seconds: 1_700_000_000 + second,
        nanoseconds: 7,
    }
}

type DiskWriter = Writer<Disk, FixtureUuids>;
type Transaction<'a> = WriteTransaction<'a, Disk, FixtureUuids>;

/// A writer over `tree`, formatted by the independent formatter the way
/// Linux stores it.
fn mounted(tree: TreeBuilder) -> (tempfile::NamedTempFile, DiskWriter) {
    let named = tempfile::NamedTempFile::new().unwrap();
    drop(filesystem_image_tree_file(
        true,
        true,
        tree,
        named.reopen().unwrap(),
    ));
    let writer = block_on(mount_writer(Disk(named.reopen().unwrap()))).unwrap();
    (named, writer)
}

/// An image whose `/file` carries attributes in every namespace.
fn image() -> (tempfile::NamedTempFile, DiskWriter) {
    let stamp = ImageTime::from_secs(1_700_000_000);
    let tree = TreeBuilder::new()
        .root(Metadata::new(0o755, stamp))
        .file("/file", b"data".to_vec(), Metadata::new(0o644, stamp))
        .xattr(
            "security.capability",
            vec![1, 0, 0, 2, 0xff, 0, 0, 0, 0, 0, 0, 0],
        )
        .xattr("trusted.zone", "中日韓")
        .xattr(ACL, vec![2, 0, 0, 0])
        .xattr("user.existing", "old")
        .directory("/dir", Metadata::new(0o755, stamp))
        .symlink("/link", "file", Metadata::new(0o777, stamp))
        .fifo("/pipe", Metadata::new(0o644, stamp));
    mounted(tree)
}

async fn lookup(tx: &Transaction<'_>, name: &str) -> InodeHandle {
    let root = tx.root().await.unwrap();
    tx.lookup(&root, name.as_bytes()).await.unwrap().unwrap()
}

async fn value(tx: &Transaction<'_>, file: &InodeHandle, name: &[u8]) -> Vec<u8> {
    let length = tx.xattr(file, name, &mut []).await.unwrap();
    let mut buffer = vec![0; length];
    // A zero-length value has no buffer to ask for.
    if length > 0 {
        assert_eq!(tx.xattr(file, name, &mut buffer).await.unwrap(), length);
    }
    buffer
}

async fn names(tx: &Transaction<'_>, file: &InodeHandle) -> Vec<Vec<u8>> {
    let length = tx.list_xattrs(file, &mut []).await.unwrap();
    let mut buffer = vec![0; length];
    if length > 0 {
        assert_eq!(tx.list_xattrs(file, &mut buffer).await.unwrap(), length);
    }
    buffer
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Everything the reader sees for `inode`, name and value, in listing order.
async fn read_back(
    snapshot: &crate::Filesystem<alloc::sync::Arc<Disk>>,
    path: &str,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let inode = snapshot.resolve(path.as_bytes()).await.unwrap();
    let mut listed = Vec::new();
    snapshot
        .list_xattrs(&inode, |name| {
            listed.push(name.to_vec());
            core::ops::ControlFlow::Continue(())
        })
        .await
        .unwrap();
    let mut all = Vec::new();
    for name in listed {
        let mut buffer = vec![0; 70_000];
        let length = snapshot
            .xattr(&inode, &name, &mut buffer)
            .await
            .unwrap()
            .expect("a listed attribute is found by name");
        buffer.truncate(length);
        all.push((name, buffer));
    }
    all
}

/// Writes an attribute of any namespace, as an image written by Linux has it.
async fn inject(tx: &mut Transaction<'_>, file: &InodeHandle, name: &[u8], value: &[u8]) {
    let edits = tx.edits();
    let root = edits.roots.get(file.tree()).unwrap();
    let key = Key::new(file.number(), item_type::XATTR_ITEM, name_hash(name));
    let mut item = edits
        .trees
        .get(&edits.volume, root, key)
        .await
        .unwrap()
        .unwrap_or_default();
    let record = RawDirItem {
        location: RawKey::new_zeroed(),
        transid: 1.into(),
        data_len: (value.len() as u16).into(),
        name_len: (name.len() as u16).into(),
        entry_type: 8,
    };
    item.extend_from_slice(record.as_bytes());
    item.extend_from_slice(name);
    item.extend_from_slice(value);
    let root = edits
        .trees
        .apply(&edits.volume, root, key, Some(&item), &mut edits.space)
        .await
        .unwrap();
    edits.roots.set(root).unwrap();
}

async fn raw_inode(tx: &mut Transaction<'_>, file: &InodeHandle) -> RawInodeItem {
    let edits = tx.edits();
    let root = edits.roots.get(file.tree()).unwrap();
    let bytes = edits
        .trees
        .get(
            &edits.volume,
            root,
            Key::new(file.number(), item_type::INODE_ITEM, 0),
        )
        .await
        .unwrap()
        .unwrap();
    *view::<RawInodeItem>(&bytes).unwrap()
}

/// The `XATTR_ITEM`s stored for `file`: key offset and record count.
async fn items(tx: &mut Transaction<'_>, file: &InodeHandle) -> Vec<(u64, usize)> {
    let edits = tx.edits();
    let root = edits.roots.get(file.tree()).unwrap();
    let mut found = Vec::new();
    let mut next = Key::new(file.number(), item_type::XATTR_ITEM, 0);
    while let Some((key, bytes)) = edits
        .trees
        .ceiling(&edits.volume, root, next)
        .await
        .unwrap()
    {
        if !key.is(file.number(), item_type::XATTR_ITEM) {
            break;
        }
        let records = crate::items::dir_records(&bytes, crate::items::RecordKind::Xattr)
            .inspect(|record| assert!(record.is_ok()))
            .count();
        found.push((key.offset, records));
        next = Key::new(file.number(), item_type::XATTR_ITEM, key.offset + 1);
    }
    found
}

#[test]
fn set_get_list_and_remove_round_trip_through_the_reader() {
    let (_named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        let directory = lookup(&tx, "dir").await;
        let before = raw_inode(&mut tx, &file).await;
        for (index, (name, text)) in CJK.iter().enumerate() {
            tx.set_xattr(
                &file,
                name.as_bytes(),
                text.as_bytes(),
                XattrMode::Create,
                when(10 + index as i64),
            )
            .await
            .unwrap();
        }
        tx.set_xattr(&file, b"user.ascii", b"plain", XattrMode::Set, when(20))
            .await
            .unwrap();
        tx.set_xattr(&file, b"user.empty", b"", XattrMode::Set, when(21))
            .await
            .unwrap();
        tx.set_xattr(
            &directory,
            "user.目录".as_bytes(),
            b"dir",
            XattrMode::Set,
            when(22),
        )
        .await
        .unwrap();
        for (name, text) in CJK {
            assert_eq!(value(&tx, &file, name.as_bytes()).await, text.as_bytes());
        }
        assert_eq!(value(&tx, &file, b"user.empty").await, b"");
        // A size query, an exact buffer and a short one.
        assert_eq!(tx.xattr(&file, b"user.ascii", &mut []).await.unwrap(), 5);
        let mut short = [0; 4];
        assert!(matches!(
            tx.xattr(&file, b"user.ascii", &mut short).await,
            Err(WriteError::XattrBuffer {
                needed: 5,
                provided: 4
            })
        ));
        let mut listed = names(&tx, &file).await;
        listed.sort();
        let mut expected: Vec<Vec<u8>> = vec![
            b"security.capability".to_vec(),
            b"system.posix_acl_access".to_vec(),
            b"trusted.zone".to_vec(),
            b"user.ascii".to_vec(),
            b"user.empty".to_vec(),
            b"user.existing".to_vec(),
        ];
        expected.extend(CJK.iter().map(|(name, _)| name.as_bytes().to_vec()));
        expected.sort();
        assert_eq!(listed, expected);
        let mut short = vec![0; 10];
        assert!(matches!(
            tx.list_xattrs(&file, &mut short).await,
            Err(WriteError::XattrBuffer { .. })
        ));
        // Setting changes the inode: ctime, transid and sequence.
        let after = raw_inode(&mut tx, &file).await;
        assert_eq!(after.ctime.sec.get(), when(21).seconds as u64);
        assert_eq!(after.ctime.nsec.get(), 7);
        assert_eq!(after.transid.get(), tx.edits().trees.generation());
        assert_eq!(
            after.sequence.get(),
            before.sequence.get().wrapping_add(5),
            "five sets on the file"
        );
        tx.remove_xattr(&file, "user.中文".as_bytes(), when(30))
            .await
            .unwrap();
        tx.remove_xattr(&file, b"user.existing", when(31))
            .await
            .unwrap();
        assert!(matches!(
            tx.xattr(&file, "user.中文".as_bytes(), &mut []).await,
            Err(WriteError::XattrMissing { .. })
        ));
        tx.commit().await.unwrap();
    });
    block_on(async {
        let snapshot = writer.snapshot().unwrap();
        let mut found = read_back(&snapshot, "/file").await;
        found.sort();
        let mut expected: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (
                b"security.capability".to_vec(),
                vec![1, 0, 0, 2, 0xff, 0, 0, 0, 0, 0, 0, 0],
            ),
            (ACL.to_vec(), vec![2, 0, 0, 0]),
            (b"trusted.zone".to_vec(), "中日韓".as_bytes().to_vec()),
            (b"user.ascii".to_vec(), b"plain".to_vec()),
            (b"user.empty".to_vec(), Vec::new()),
            (
                "user.日本語".as_bytes().to_vec(),
                "こんにちは世界".as_bytes().to_vec(),
            ),
            (
                "user.한국어".as_bytes().to_vec(),
                "안녕하세요 세계".as_bytes().to_vec(),
            ),
        ];
        expected.sort();
        assert_eq!(found, expected);
        assert_eq!(
            read_back(&snapshot, "/dir").await,
            vec![("user.目录".as_bytes().to_vec(), b"dir".to_vec())]
        );
    });
}

#[test]
fn flags_and_missing_attributes_fail_as_on_linux() {
    let (_named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        // XATTR_CREATE over an existing name is EEXIST.
        assert!(matches!(
            tx.set_xattr(&file, b"user.existing", b"new", XattrMode::Create, when(1))
                .await,
            Err(WriteError::XattrExists { .. })
        ));
        // XATTR_REPLACE of an absent name is ENODATA.
        assert!(matches!(
            tx.set_xattr(&file, b"user.absent", b"new", XattrMode::Replace, when(1))
                .await,
            Err(WriteError::XattrMissing { .. })
        ));
        // Failed calls leave the value and the inode alone.
        assert_eq!(value(&tx, &file, b"user.existing").await, b"old");
        assert!(matches!(
            tx.xattr(&file, b"user.absent", &mut []).await,
            Err(WriteError::XattrMissing { .. })
        ));
        // Removing an absent name is ENODATA, as XATTR_REPLACE with no value.
        assert!(matches!(
            tx.remove_xattr(&file, b"user.absent", when(1)).await,
            Err(WriteError::XattrMissing { .. })
        ));
        // Replace keeps the name and swaps the value, longer or shorter.
        tx.set_xattr(
            &file,
            b"user.existing",
            "新しい値".as_bytes(),
            XattrMode::Replace,
            when(2),
        )
        .await
        .unwrap();
        assert_eq!(
            value(&tx, &file, b"user.existing").await,
            "新しい値".as_bytes()
        );
        tx.set_xattr(&file, b"user.existing", b"x", XattrMode::Set, when(3))
            .await
            .unwrap();
        assert_eq!(value(&tx, &file, b"user.existing").await, b"x");
        tx.set_xattr(&file, b"user.fresh", b"v", XattrMode::Create, when(4))
            .await
            .unwrap();
        // Names: empty and over 255 bytes are ERANGE, a bare prefix EINVAL.
        let long = [b"user.".as_slice(), &[b'n'; 251]].concat();
        assert_eq!(long.len(), 256);
        for name in [&b""[..], &long] {
            assert!(matches!(
                tx.set_xattr(&file, name, b"v", XattrMode::Set, when(5))
                    .await,
                Err(WriteError::XattrNameLength { .. })
            ));
            assert!(matches!(
                tx.remove_xattr(&file, name, when(5)).await,
                Err(WriteError::XattrNameLength { .. })
            ));
            assert!(matches!(
                tx.xattr(&file, name, &mut []).await,
                Err(WriteError::XattrNameLength { .. })
            ));
        }
        for prefix in [&b"user."[..], b"security.", b"trusted.", b"btrfs."] {
            assert!(matches!(
                tx.set_xattr(&file, prefix, b"v", XattrMode::Set, when(5))
                    .await,
                Err(WriteError::XattrName)
            ));
            assert!(matches!(
                tx.remove_xattr(&file, prefix, when(5)).await,
                Err(WriteError::XattrName)
            ));
            assert!(matches!(
                tx.xattr(&file, prefix, &mut []).await,
                Err(WriteError::XattrName)
            ));
        }
        let longest = [b"user.".as_slice(), &[b'n'; 250]].concat();
        tx.set_xattr(&file, &longest, b"v", XattrMode::Set, when(5))
            .await
            .unwrap();
        assert_eq!(value(&tx, &file, &longest).await, b"v");
    });
}

#[test]
fn colliding_names_share_one_item() {
    assert_eq!(name_hash(COLLIDING[0]), name_hash(COLLIDING[1]));
    assert_eq!(name_hash(COLLIDING[0]), 0x4e84_071b);
    assert_ne!(COLLIDING[0], COLLIDING[1]);
    let (named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        let hash = name_hash(COLLIDING[0]);
        tx.set_xattr(&file, COLLIDING[0], b"first", XattrMode::Create, when(1))
            .await
            .unwrap();
        tx.set_xattr(&file, COLLIDING[1], b"second", XattrMode::Create, when(2))
            .await
            .unwrap();
        let stored = items(&mut tx, &file).await;
        assert!(stored.contains(&(hash, 2)), "{stored:?}");
        // Each is found by name, and replacing one leaves the other.
        assert_eq!(value(&tx, &file, COLLIDING[0]).await, b"first");
        assert_eq!(value(&tx, &file, COLLIDING[1]).await, b"second");
        tx.set_xattr(
            &file,
            COLLIDING[0],
            "長い値になる".as_bytes(),
            XattrMode::Replace,
            when(3),
        )
        .await
        .unwrap();
        assert_eq!(value(&tx, &file, COLLIDING[1]).await, b"second");
        assert_eq!(
            value(&tx, &file, COLLIDING[0]).await,
            "長い値になる".as_bytes()
        );
        assert!(items(&mut tx, &file).await.contains(&(hash, 2)));
        // Removing one keeps the item, removing the second deletes it.
        tx.remove_xattr(&file, COLLIDING[0], when(4)).await.unwrap();
        assert!(items(&mut tx, &file).await.contains(&(hash, 1)));
        assert_eq!(value(&tx, &file, COLLIDING[1]).await, b"second");
        assert!(matches!(
            tx.xattr(&file, COLLIDING[0], &mut []).await,
            Err(WriteError::XattrMissing { .. })
        ));
        tx.set_xattr(&file, COLLIDING[0], b"again", XattrMode::Create, when(5))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    });
    block_on(async {
        let snapshot = writer.snapshot().unwrap();
        let found = read_back(&snapshot, "/file").await;
        for (name, expected) in [(COLLIDING[0], &b"again"[..]), (COLLIDING[1], b"second")] {
            assert!(found.contains(&(name.to_vec(), expected.to_vec())));
        }
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        for name in COLLIDING {
            tx.remove_xattr(&file, name, when(6)).await.unwrap();
        }
        let hash = name_hash(COLLIDING[0]);
        assert!(
            items(&mut tx, &file)
                .await
                .iter()
                .all(|(key, _)| *key != hash)
        );
    });
    drop(named);
}

#[test]
fn sizes_are_limited_as_linux_limits_them() {
    let (_named, mut writer) = image();
    block_on(async {
        let nodesize = writer.snapshot().unwrap().superblock().nodesize() as usize;
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        // BTRFS_MAX_XATTR_SIZE: a leaf's item capacity less one dir item.
        let maximum = nodesize
            - size_of::<crate::ondisk::RawHeader>()
            - size_of::<crate::ondisk::RawItem>()
            - size_of::<RawDirItem>();
        let name = b"user.big";
        let fits = vec![0xAB; maximum - name.len()];
        tx.set_xattr(&file, name, &fits, XattrMode::Create, when(1))
            .await
            .unwrap();
        assert_eq!(tx.xattr(&file, name, &mut []).await.unwrap(), fits.len());
        let mut buffer = vec![0; fits.len()];
        tx.xattr(&file, name, &mut buffer).await.unwrap();
        assert_eq!(buffer, fits);
        tx.remove_xattr(&file, name, when(2)).await.unwrap();
        // One byte more is ENOSPC, and more than XATTR_SIZE_MAX is E2BIG.
        assert!(matches!(
            tx.set_xattr(
                &file,
                name,
                &vec![0; fits.len() + 1],
                XattrMode::Set,
                when(3)
            )
            .await,
            Err(WriteError::XattrNoSpace { .. })
        ));
        assert!(matches!(
            tx.set_xattr(&file, name, &vec![0; 65_537], XattrMode::Set, when(3))
                .await,
            Err(WriteError::XattrValueTooBig {
                length: 65_537,
                maximum: 65_536
            })
        ));
        // A name sharing an item with a full one overflows the leaf.
        let half = vec![1; maximum / 2];
        tx.set_xattr(&file, COLLIDING[0], &half, XattrMode::Set, when(4))
            .await
            .unwrap();
        assert!(matches!(
            tx.set_xattr(&file, COLLIDING[1], &half, XattrMode::Set, when(5))
                .await,
            Err(WriteError::XattrNoSpace { .. })
        ));
        assert_eq!(value(&tx, &file, COLLIDING[0]).await, half);
        assert!(matches!(
            tx.xattr(&file, COLLIDING[1], &mut []).await,
            Err(WriteError::XattrMissing { .. })
        ));
    });
}

/// The tags and permission bits of `include/uapi/linux/posix_acl.h`.
const USER_OBJ: u16 = 0x01;
const USER: u16 = 0x02;
const GROUP_OBJ: u16 = 0x04;
const GROUP: u16 = 0x08;
const MASK: u16 = 0x10;
const OTHER: u16 = 0x20;
/// `ACL_UNDEFINED_ID`.
const NO_ID: u32 = u32::MAX;

/// An ACL in the uapi encoding `setfacl` hands to `setxattr(2)`.
fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut bytes = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in entries {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&perm.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    bytes
}

/// `u::rwx,u:1000:rw-,g::r-x,g:1001:r--,m::rw-,o::r--`.
fn named_acl() -> Vec<u8> {
    acl(&[
        (USER_OBJ, 7, NO_ID),
        (USER, 6, 1000),
        (GROUP_OBJ, 5, NO_ID),
        (GROUP, 4, 1001),
        (MASK, 6, NO_ID),
        (OTHER, 4, NO_ID),
    ])
}

fn permission_bits(raw: &RawInodeItem) -> u32 {
    raw.mode.get() & 0o7777
}

/// `security.*`, `trusted.*` and `user.*` are stored, replaced, listed and
/// removed alike, with the create and replace flags, and read back through
/// the reader after a commit; names no handler takes stay `EOPNOTSUPP`.
#[test]
fn every_stored_namespace_is_written_and_reads_back() {
    const WRITTEN: [(&str, &str); 6] = [
        ("security.中文", "安全标签"),
        ("security.日本語", "セキュリティ"),
        ("trusted.한국어", "신뢰할 수 있는 값"),
        ("trusted.日本語", "信頼された値"),
        ("user.中文", "用户的值"),
        ("security.capability", "能力"),
    ];
    let (named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        let directory = lookup(&tx, "dir").await;
        for (index, (name, text)) in WRITTEN.iter().enumerate() {
            let mode = if *name == "security.capability" {
                XattrMode::Replace
            } else {
                XattrMode::Create
            };
            tx.set_xattr(
                &file,
                name.as_bytes(),
                text.as_bytes(),
                mode,
                when(index as i64),
            )
            .await
            .unwrap();
            assert_eq!(value(&tx, &file, name.as_bytes()).await, text.as_bytes());
        }
        // The flags hold in every namespace.
        for name in [&b"trusted.zone"[..], b"security.capability"] {
            assert!(
                matches!(
                    tx.set_xattr(&file, name, b"x", XattrMode::Create, when(10))
                        .await,
                    Err(WriteError::XattrExists { .. })
                ),
                "{}",
                String::from_utf8_lossy(name)
            );
        }
        assert!(matches!(
            tx.set_xattr(
                &file,
                "trusted.缺席".as_bytes(),
                b"x",
                XattrMode::Replace,
                when(10)
            )
            .await,
            Err(WriteError::XattrMissing { .. })
        ));
        tx.set_xattr(
            &file,
            b"trusted.zone",
            "중일한".as_bytes(),
            XattrMode::Replace,
            when(11),
        )
        .await
        .unwrap();
        tx.remove_xattr(&file, "security.日本語".as_bytes(), when(12))
            .await
            .unwrap();
        assert!(matches!(
            tx.remove_xattr(&file, "security.日本語".as_bytes(), when(13))
                .await,
            Err(WriteError::XattrMissing { .. })
        ));
        // A directory takes them too.
        tx.set_xattr(
            &directory,
            "trusted.目录".as_bytes(),
            "ディレクトリ".as_bytes(),
            XattrMode::Set,
            when(14),
        )
        .await
        .unwrap();
        let after = raw_inode(&mut tx, &file).await;
        assert_eq!(after.ctime.sec.get(), when(12).seconds as u64);
        // No handler: EOPNOTSUPP.
        for name in [
            &b"system.other"[..],
            b"btrfs.compression",
            b"os2.x",
            b"plain",
        ] {
            assert!(matches!(
                tx.set_xattr(&file, name, b"x", XattrMode::Set, when(1))
                    .await,
                Err(WriteError::XattrNamespace)
            ));
            assert!(matches!(
                tx.remove_xattr(&file, name, when(1)).await,
                Err(WriteError::XattrNamespace)
            ));
            assert!(matches!(
                tx.xattr(&file, name, &mut []).await,
                Err(WriteError::XattrNamespace)
            ));
        }
        tx.commit().await.unwrap();
    });
    block_on(async {
        let snapshot = writer.snapshot().unwrap();
        let mut found = read_back(&snapshot, "/file").await;
        found.sort();
        let mut expected: Vec<(Vec<u8>, Vec<u8>)> = WRITTEN
            .iter()
            .filter(|(name, _)| *name != "security.日本語")
            .map(|(name, text)| (name.as_bytes().to_vec(), text.as_bytes().to_vec()))
            .collect();
        expected.extend([
            (b"trusted.zone".to_vec(), "중일한".as_bytes().to_vec()),
            (ACL.to_vec(), vec![2, 0, 0, 0]),
            (b"user.existing".to_vec(), b"old".to_vec()),
        ]);
        expected.sort();
        assert_eq!(found, expected);
        assert_eq!(
            read_back(&snapshot, "/dir").await,
            vec![(
                "trusted.目录".as_bytes().to_vec(),
                "ディレクトリ".as_bytes().to_vec()
            )]
        );
    });
    drop(named);
}

/// POSIX ACLs are stored as `btrfs_set_acl` stores them: validated, in the
/// encoding `posix_acl_to_xattr` produces, with the access ACL setting the
/// mode's permission bits and not stored when the mode says all it does.
#[test]
fn posix_acls_are_stored_as_btrfs_set_acl_stores_them() {
    const DEFAULT: &[u8] = b"system.posix_acl_default";
    let (named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        let directory = lookup(&tx, "dir").await;
        let before = raw_inode(&mut tx, &file).await;
        assert_eq!(permission_bits(&before), 0o644);

        // A base entry's id is written as ACL_UNDEFINED_ID, and the flags do
        // not apply to an ACL (`do_setxattr` hands it to `do_set_acl`).
        let mut sent = named_acl();
        sent[8..12].copy_from_slice(&5u32.to_le_bytes());
        tx.set_xattr(&file, ACL, &sent, XattrMode::Create, when(1))
            .await
            .unwrap();
        assert_eq!(value(&tx, &file, ACL).await, named_acl());
        let after = raw_inode(&mut tx, &file).await;
        // u::rwx, the mask rw-, o::r--.
        assert_eq!(permission_bits(&after), 0o764);
        assert_eq!(after.ctime.sec.get(), when(1).seconds as u64);
        assert_eq!(after.sequence.get(), before.sequence.get().wrapping_add(1));

        // An access ACL the mode represents sets the mode and is not kept.
        tx.set_xattr(
            &file,
            ACL,
            &acl(&[
                (USER_OBJ, 6, NO_ID),
                (GROUP_OBJ, 0, NO_ID),
                (OTHER, 0, NO_ID),
            ]),
            XattrMode::Replace,
            when(2),
        )
        .await
        .unwrap();
        assert_eq!(permission_bits(&raw_inode(&mut tx, &file).await), 0o600);
        assert!(matches!(
            tx.xattr(&file, ACL, &mut []).await,
            Err(WriteError::XattrMissing { .. })
        ));
        // Removing an absent ACL succeeds, as `vfs_remove_acl` does.
        tx.remove_xattr(&file, ACL, when(3)).await.unwrap();

        // A default ACL belongs to a directory.
        tx.set_xattr(&directory, DEFAULT, &named_acl(), XattrMode::Set, when(4))
            .await
            .unwrap();
        assert_eq!(value(&tx, &directory, DEFAULT).await, named_acl());
        assert_eq!(
            permission_bits(&raw_inode(&mut tx, &directory).await),
            0o755,
            "a default ACL leaves the mode alone"
        );
        assert!(matches!(
            tx.set_xattr(&file, DEFAULT, &named_acl(), XattrMode::Set, when(5))
                .await,
            Err(WriteError::AclDefaultNotDirectory { .. })
        ));
        tx.remove_xattr(&file, DEFAULT, when(5)).await.unwrap();

        // `posix_acl_from_xattr` and `posix_acl_valid` refusals.
        for (sent, refusal) in [
            (vec![2, 0], "short"),
            (
                acl(&[
                    (USER_OBJ, 7, NO_ID),
                    (USER, 6, 1000),
                    (GROUP_OBJ, 5, NO_ID),
                    (OTHER, 0, NO_ID),
                ]),
                "named entry with no mask",
            ),
            (
                acl(&[
                    (USER_OBJ, 7, NO_ID),
                    (USER, 6, NO_ID),
                    (GROUP_OBJ, 5, NO_ID),
                    (MASK, 7, NO_ID),
                    (OTHER, 0, NO_ID),
                ]),
                "undefined id",
            ),
            (
                acl(&[
                    (USER_OBJ, 8, NO_ID),
                    (GROUP_OBJ, 5, NO_ID),
                    (OTHER, 0, NO_ID),
                ]),
                "permission bit",
            ),
        ] {
            assert!(
                matches!(
                    tx.set_xattr(&file, ACL, &sent, XattrMode::Set, when(6))
                        .await,
                    Err(WriteError::AclInvalid)
                ),
                "{refusal}"
            );
        }
        let mut version = named_acl();
        version[0] = 1;
        assert!(matches!(
            tx.set_xattr(&file, ACL, &version, XattrMode::Set, when(6))
                .await,
            Err(WriteError::AclVersion { version: 1 })
        ));
        // A FIFO takes an ACL (`btrfs_special_inode_operations`), a symlink
        // none.
        let pipe = lookup(&tx, "pipe").await;
        tx.set_xattr(&pipe, ACL, &named_acl(), XattrMode::Set, when(7))
            .await
            .unwrap();
        let link = lookup(&tx, "link").await;
        for result in [
            tx.set_xattr(&link, ACL, &named_acl(), XattrMode::Set, when(7))
                .await,
            tx.remove_xattr(&link, ACL, when(7)).await,
            tx.xattr(&link, ACL, &mut []).await.map(drop),
        ] {
            assert!(matches!(result, Err(WriteError::AclNotSupported { .. })));
        }

        // chmod carries the mode into the access ACL (`posix_acl_chmod`).
        tx.set_xattr(&file, ACL, &named_acl(), XattrMode::Set, when(8))
            .await
            .unwrap();
        tx.set_permissions(&file, 0o751, when(9)).await.unwrap();
        assert_eq!(
            value(&tx, &file, ACL).await,
            acl(&[
                (USER_OBJ, 7, NO_ID),
                (USER, 6, 1000),
                (GROUP_OBJ, 5, NO_ID),
                (GROUP, 4, 1001),
                (MASK, 5, NO_ID),
                (OTHER, 1, NO_ID),
            ])
        );
        assert_eq!(permission_bits(&raw_inode(&mut tx, &file).await), 0o751);
        tx.commit().await.unwrap();
    });
    block_on(async {
        let snapshot = writer.snapshot().unwrap();
        let file = snapshot.resolve(b"/file").await.unwrap();
        assert_eq!(file.metadata().permissions, 0o751);
        let mut buffer = [0; 64];
        let length = snapshot
            .xattr(&file, ACL, &mut buffer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(buffer[..length][4..12], acl(&[(USER_OBJ, 7, NO_ID)])[4..]);
    });
    drop(named);
}

/// A creation under a directory with a default ACL inherits it as
/// `posix_acl_create` has it: the umask does not apply, the access ACL and
/// the mode narrow each other, and a directory keeps the default ACL.
#[test]
fn creations_inherit_a_default_acl() {
    const DEFAULT: &[u8] = b"system.posix_acl_default";
    let (named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let directory = lookup(&tx, "dir").await;
        let root = tx.root().await.unwrap();
        tx.set_xattr(&directory, DEFAULT, &named_acl(), XattrMode::Set, when(1))
            .await
            .unwrap();
        let attributes = |permissions| Attributes {
            permissions,
            uid: 1000,
            gid: 1000,
            umask: 0o077,
        };
        let file = tx
            .create_file(
                Name {
                    parent: &directory,
                    bytes: "文件-ファイル-파일".as_bytes(),
                },
                attributes(0o666),
                when(2),
            )
            .await
            .unwrap();
        // 0666 against u::rwx, m::rw-, o::r--.
        assert_eq!(permission_bits(&raw_inode(&mut tx, &file).await), 0o664);
        assert_eq!(
            value(&tx, &file, ACL).await,
            acl(&[
                (USER_OBJ, 6, NO_ID),
                (USER, 6, 1000),
                (GROUP_OBJ, 5, NO_ID),
                (GROUP, 4, 1001),
                (MASK, 6, NO_ID),
                (OTHER, 4, NO_ID),
            ])
        );
        assert!(matches!(
            tx.xattr(&file, DEFAULT, &mut []).await,
            Err(WriteError::XattrMissing { .. })
        ));
        let subdirectory = tx
            .create_directory(
                Name {
                    parent: &directory,
                    bytes: "目录-ディレクトリ-디렉터리".as_bytes(),
                },
                attributes(0o777),
                when(3),
            )
            .await
            .unwrap();
        assert_eq!(
            permission_bits(&raw_inode(&mut tx, &subdirectory).await),
            0o764
        );
        assert_eq!(value(&tx, &subdirectory, DEFAULT).await, named_acl());
        assert_eq!(value(&tx, &subdirectory, ACL).await, named_acl());
        // Without a default ACL the umask applies and nothing is inherited.
        let plain = tx
            .create_file(
                Name {
                    parent: &root,
                    bytes: "普通".as_bytes(),
                },
                attributes(0o666),
                when(4),
            )
            .await
            .unwrap();
        assert_eq!(permission_bits(&raw_inode(&mut tx, &plain).await), 0o600);
        assert!(names(&tx, &plain).await.is_empty());
        tx.commit().await.unwrap();
    });
    block_on(async {
        let snapshot = writer.snapshot().unwrap();
        let inherited = read_back(&snapshot, "/dir/目录-ディレクトリ-디렉터리").await;
        assert_eq!(inherited.len(), 2);
    });
    drop(named);
}

/// Under a directory with `S_ISGID`, creations inherit the parent's group and
/// subdirectories inherit `S_ISGID` (`inode_init_owner` in `fs/inode.c`), both
/// under umask narrowing and under a default ACL (`posix_acl_create`).
#[test]
fn creations_under_setgid_directory_inherit_group_and_directory_setgid() {
    const DEFAULT: &[u8] = b"system.posix_acl_default";
    let (named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let sgid = tx
            .create_directory(
                Name {
                    parent: &root,
                    bytes: b"sgid",
                },
                Attributes {
                    permissions: 0o755,
                    uid: 100,
                    gid: 200,
                    umask: 0,
                },
                when(1),
            )
            .await
            .unwrap();
        tx.set_permissions(&sgid, 0o2755, when(2)).await.unwrap();

        let file = tx
            .create_file(
                Name {
                    parent: &sgid,
                    bytes: "文件-ファイル-파일".as_bytes(),
                },
                Attributes {
                    permissions: 0o666,
                    uid: 100,
                    gid: 300,
                    umask: 0o022,
                },
                when(3),
            )
            .await
            .unwrap();
        let file_meta = tx.metadata(&file).await.unwrap();
        assert_eq!(file_meta.uid, 100);
        assert_eq!(file_meta.gid, 200);
        assert_eq!(file_meta.permissions, 0o644);

        let subdir = tx
            .create_directory(
                Name {
                    parent: &sgid,
                    bytes: "目录-ディレクトリ-디렉터리".as_bytes(),
                },
                Attributes {
                    permissions: 0o777,
                    uid: 100,
                    gid: 300,
                    umask: 0o022,
                },
                when(4),
            )
            .await
            .unwrap();
        let subdir_meta = tx.metadata(&subdir).await.unwrap();
        assert_eq!(subdir_meta.uid, 100);
        assert_eq!(subdir_meta.gid, 200);
        assert_eq!(subdir_meta.permissions, 0o2755);

        // `inode_init_owner` sets `S_ISGID` after `posix_acl_create` applies
        // the umask, and the inherited `S_ISGID` propagates to grandchildren.
        let nested = tx
            .create_directory(
                Name {
                    parent: &subdir,
                    bytes: b"nested",
                },
                Attributes {
                    permissions: 0o777,
                    uid: 101,
                    gid: 400,
                    umask: 0o2027,
                },
                when(5),
            )
            .await
            .unwrap();
        let nested_meta = tx.metadata(&nested).await.unwrap();
        assert_eq!(nested_meta.uid, 101);
        assert_eq!(nested_meta.gid, 200);
        assert_eq!(nested_meta.permissions, 0o2750);

        let link = tx
            .symlink(
                Name {
                    parent: &sgid,
                    bytes: "链接-リンク-링크".as_bytes(),
                },
                b"nested",
                Attributes {
                    permissions: 0o777,
                    uid: 100,
                    gid: 300,
                    umask: 0,
                },
                when(6),
            )
            .await
            .unwrap();
        let link_meta = tx.metadata(&link).await.unwrap();
        assert_eq!(link_meta.gid, 200);
        assert_eq!(link_meta.permissions, 0o777);

        // With a default ACL (`u::rwx,u:101:rwx,g::r-x,m::rwx,o::r-x`, as in
        // fstests generic/444), subdirectories keep `S_ISGID` (`0o2775`).
        let default_acl = acl(&[
            (USER_OBJ, 7, NO_ID),
            (USER, 7, 101),
            (GROUP_OBJ, 5, NO_ID),
            (MASK, 7, NO_ID),
            (OTHER, 5, NO_ID),
        ]);
        tx.set_xattr(&sgid, DEFAULT, &default_acl, XattrMode::Set, when(7))
            .await
            .unwrap();
        let acl_subdir = tx
            .create_directory(
                Name {
                    parent: &sgid,
                    bytes: b"acl-sub",
                },
                Attributes {
                    permissions: 0o777,
                    uid: 100,
                    gid: 101,
                    umask: 0o022,
                },
                when(8),
            )
            .await
            .unwrap();
        let acl_subdir_meta = tx.metadata(&acl_subdir).await.unwrap();
        assert_eq!(acl_subdir_meta.gid, 200);
        assert_eq!(acl_subdir_meta.permissions, 0o2775);
        assert_eq!(value(&tx, &acl_subdir, DEFAULT).await, default_acl);
        assert_eq!(value(&tx, &acl_subdir, ACL).await, default_acl);

        let acl_file = tx
            .create_file(
                Name {
                    parent: &sgid,
                    bytes: b"acl-file",
                },
                Attributes {
                    permissions: 0o666,
                    uid: 100,
                    gid: 101,
                    umask: 0o022,
                },
                when(9),
            )
            .await
            .unwrap();
        let acl_file_meta = tx.metadata(&acl_file).await.unwrap();
        assert_eq!(acl_file_meta.gid, 200);
        assert_eq!(acl_file_meta.permissions, 0o664);

        tx.commit().await.unwrap();
    });
    block_on(async {
        let snapshot = writer.snapshot().unwrap();
        let subdir = snapshot.resolve(b"/sgid/acl-sub").await.unwrap();
        assert_eq!(subdir.metadata().gid, 200);
        assert_eq!(subdir.metadata().permissions, 0o2775);
        let file = snapshot
            .resolve("/sgid/文件-ファイル-파일".as_bytes())
            .await
            .unwrap();
        assert_eq!(file.metadata().gid, 200);
        assert_eq!(file.metadata().permissions, 0o644);
    });
    drop(named);
}

#[test]
fn user_attributes_apply_to_regular_files_directories_and_sockets() {
    use crate::transaction::namespace::{Attributes as RawAttributes, Name as RawName};
    let (named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        for path in ["link", "pipe"] {
            let node = lookup(&tx, path).await;
            // Writing is EPERM, reading ENODATA (`xattr_permission` runs
            // before `xattr_resolve_name`, bare `"user."` included).
            for user_name in [&b"user.x"[..], b"user."] {
                assert!(matches!(
                    tx.set_xattr(&node, user_name, b"v", XattrMode::Set, when(1))
                        .await,
                    Err(WriteError::XattrNotPermitted { .. })
                ));
                assert!(matches!(
                    tx.remove_xattr(&node, user_name, when(1)).await,
                    Err(WriteError::XattrNotPermitted { .. })
                ));
                assert!(matches!(
                    tx.xattr(&node, user_name, &mut []).await,
                    Err(WriteError::XattrMissing { .. })
                ));
            }
            for prefix in [&b"security."[..], b"trusted.", b"btrfs."] {
                assert!(matches!(
                    tx.xattr(&node, prefix, &mut []).await,
                    Err(WriteError::XattrName)
                ));
            }
            assert!(matches!(
                tx.xattr(&node, b"os2.x", &mut []).await,
                Err(WriteError::XattrNamespace)
            ));
        }
        // A symlink created by the writer is the same.
        let link = tx
            .symlink(
                Name {
                    parent: &root,
                    bytes: "链接-リンク-링크".as_bytes(),
                },
                b"file",
                Attributes {
                    permissions: 0o777,
                    uid: 0,
                    gid: 0,
                    umask: 0,
                },
                when(1),
            )
            .await
            .unwrap();
        assert!(matches!(
            tx.set_xattr(&link, b"user.x", b"v", XattrMode::Set, when(2))
                .await,
            Err(WriteError::XattrNotPermitted { .. })
        ));

        // Sockets (`S_IFSOCK`, `fs/xattr.c:159-175`) accept `user.*`.
        tx.edits()
            .create_node(
                RawName {
                    tree: root.tree(),
                    parent: root.number(),
                    bytes: "套接字-ソケット-소켓".as_bytes(),
                },
                RawAttributes {
                    mode: 0o140_755,
                    uid: 1000,
                    gid: 1000,
                    umask: 0,
                },
                when(3),
            )
            .await
            .unwrap();
        let socket = tx
            .lookup(&root, "套接字-ソケット-소켓".as_bytes())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tx.metadata(&socket).await.unwrap().kind,
            crate::FileKind::Socket
        );
        assert!(matches!(
            tx.xattr(&socket, b"user.", &mut []).await,
            Err(WriteError::XattrName)
        ));
        for (index, (name, text)) in CJK.iter().enumerate() {
            tx.set_xattr(
                &socket,
                name.as_bytes(),
                text.as_bytes(),
                XattrMode::Create,
                when(4 + index as i64),
            )
            .await
            .unwrap();
            assert_eq!(value(&tx, &socket, name.as_bytes()).await, text.as_bytes());
        }
        tx.remove_xattr(&socket, CJK[0].0.as_bytes(), when(10))
            .await
            .unwrap();
        assert!(matches!(
            tx.xattr(&socket, CJK[0].0.as_bytes(), &mut []).await,
            Err(WriteError::XattrMissing { .. })
        ));
        tx.commit().await.unwrap();
    });
    block_on(async {
        let snapshot = writer.snapshot().unwrap();
        let mut found = read_back(&snapshot, "/套接字-ソケット-소켓").await;
        found.sort();
        let mut expected: Vec<(Vec<u8>, Vec<u8>)> = CJK[1..]
            .iter()
            .map(|(name, text)| (name.as_bytes().to_vec(), text.as_bytes().to_vec()))
            .collect();
        expected.sort();
        assert_eq!(found, expected);
    });
    drop(named);
}

#[test]
fn immutable_and_append_only_inodes_refuse_changes() {
    use crate::items::{INODE_APPEND, INODE_IMMUTABLE};
    let (_named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        tx.set_xattr(&file, b"user.kept", b"v", XattrMode::Set, when(1))
            .await
            .unwrap();
        for flag in [INODE_IMMUTABLE, INODE_APPEND] {
            let mut raw = raw_inode(&mut tx, &file).await;
            raw.flags.set(flag);
            let edits = tx.edits();
            let root = edits.roots.get(file.tree()).unwrap();
            let root = edits
                .trees
                .apply(
                    &edits.volume,
                    root,
                    Key::new(file.number(), item_type::INODE_ITEM, 0),
                    Some(raw.as_bytes()),
                    &mut edits.space,
                )
                .await
                .unwrap();
            edits.roots.set(root).unwrap();
            assert!(matches!(
                tx.set_xattr(&file, b"user.kept", b"w", XattrMode::Set, when(2))
                    .await,
                Err(WriteError::InodeFlags { .. })
            ));
            assert!(matches!(
                tx.remove_xattr(&file, b"user.kept", when(2)).await,
                Err(WriteError::InodeFlags { .. })
            ));
            assert_eq!(value(&tx, &file, b"user.kept").await, b"v");
        }
    });
}

#[test]
fn removing_the_inode_removes_its_attributes() {
    let (named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let attributes = Attributes {
            permissions: 0o644,
            uid: 1000,
            gid: 1000,
            umask: 0,
        };
        let doomed = tx
            .create_file(
                Name {
                    parent: &root,
                    bytes: b"doomed",
                },
                attributes,
                when(1),
            )
            .await
            .unwrap();
        let folder = tx
            .create_directory(
                Name {
                    parent: &root,
                    bytes: b"folder",
                },
                attributes,
                when(1),
            )
            .await
            .unwrap();
        for node in [&doomed, &folder] {
            for (name, text) in CJK {
                tx.set_xattr(
                    node,
                    name.as_bytes(),
                    text.as_bytes(),
                    XattrMode::Set,
                    when(2),
                )
                .await
                .unwrap();
            }
            for name in COLLIDING {
                tx.set_xattr(node, name, b"v", XattrMode::Set, when(2))
                    .await
                    .unwrap();
            }
            inject(&mut tx, node, b"security.capability", &[1; 20]).await;
            inject(&mut tx, node, b"trusted.x", b"t").await;
            assert!(!items(&mut tx, node).await.is_empty());
        }
        tx.commit().await.unwrap();
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let doomed = tx.lookup(&root, b"doomed").await.unwrap().unwrap();
        let folder = tx.lookup(&root, b"folder").await.unwrap().unwrap();
        tx.unlink_name(
            Name {
                parent: &root,
                bytes: b"doomed",
            },
            when(3),
        )
        .await
        .unwrap();
        tx.unlink_name(
            Name {
                parent: &root,
                bytes: b"folder",
            },
            when(3),
        )
        .await
        .unwrap();
        for node in [&doomed, &folder] {
            let edits = tx.edits();
            let tree = edits.roots.get(node.tree()).unwrap();
            let left = edits
                .trees
                .ceiling(&edits.volume, tree, Key::new(node.number(), 0, 0))
                .await
                .unwrap();
            assert!(
                left.is_none_or(|(key, _)| key.objectid != node.number()),
                "every item of the removed inode is gone"
            );
        }
        tx.commit().await.unwrap();
    });
    drop(named);
}

#[test]
fn a_hard_linked_inode_keeps_its_attributes_until_the_last_link() {
    let (_named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let file = lookup(&tx, "file").await;
        tx.hard_link(
            Name {
                parent: &root,
                bytes: b"alias",
            },
            &file,
            when(1),
        )
        .await
        .unwrap();
        tx.unlink_name(
            Name {
                parent: &root,
                bytes: b"file",
            },
            when(2),
        )
        .await
        .unwrap();
        let alias = lookup(&tx, "alias").await;
        assert_eq!(value(&tx, &alias, b"user.existing").await, b"old");
        assert_eq!(
            value(&tx, &alias, b"trusted.zone").await,
            "中日韓".as_bytes()
        );
    });
}

#[test]
fn an_owner_change_kills_the_file_capability_of_a_non_directory() {
    let (_named, mut writer) = image();
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        let directory = lookup(&tx, "dir").await;
        inject(&mut tx, &directory, b"security.capability", &[1; 12]).await;
        let edits = tx.edits();
        edits
            .kill_privileges(file.tree(), file.number())
            .await
            .unwrap();
        // A directory keeps it (`cap_inode_killpriv` is for files), and a file
        // without one is not an error.
        edits
            .kill_privileges(directory.tree(), directory.number())
            .await
            .unwrap();
        edits
            .kill_privileges(file.tree(), file.number())
            .await
            .unwrap();
        assert!(matches!(
            tx.xattr(&file, b"security.capability", &mut []).await,
            Err(WriteError::XattrMissing { .. })
        ));
        // The rest of the file's attributes stay.
        assert_eq!(value(&tx, &file, b"user.existing").await, b"old");
        assert_eq!(value(&tx, &file, ACL).await, [2, 0, 0, 0]);
        assert_eq!(
            value(&tx, &directory, b"security.capability").await,
            [1; 12]
        );
    });
}

/// `chown` of a file carrying a capability Linux wrote removes the
/// capability, as `notify_change` does for `ATTR_KILL_PRIV`, whichever of the
/// owner and the group changes; a directory keeps its own, and every other
/// attribute stays.
#[test]
fn chown_removes_the_capability_linux_wrote() {
    const FILES: [&str; 3] = ["/中文文件", "/日本語のファイル", "/한국어파일"];
    const DIRECTORY: &str = "/目录-ディレクトリ-디렉터리";
    /// A version 2 `security.capability` granting `CAP_NET_RAW` (bit 13).
    const CAPABILITY: [u8; 20] = [
        1, 0, 0, 2, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    let stamp = ImageTime::from_secs(1_700_000_000);
    let mut tree = TreeBuilder::new().root(Metadata::new(0o755, stamp));
    for (path, (name, text)) in FILES.iter().zip(CJK) {
        tree = tree
            .file(*path, path.as_bytes().to_vec(), Metadata::new(0o755, stamp))
            .xattr("security.capability", CAPABILITY.to_vec())
            .xattr(name, text);
    }
    let tree = tree
        .directory(DIRECTORY, Metadata::new(0o755, stamp))
        .xattr("security.capability", CAPABILITY.to_vec());
    let (_named, mut writer) = mounted(tree);
    let owners = [
        (Some(1000), None),
        (None, Some(1000)),
        (Some(1001), Some(1001)),
    ];
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        for (path, (uid, gid)) in FILES.iter().zip(owners) {
            let file = lookup(&tx, &path[1..]).await;
            assert_eq!(value(&tx, &file, b"security.capability").await, CAPABILITY);
            tx.set_owner(&file, uid, gid, when(1)).await.unwrap();
            assert!(matches!(
                tx.xattr(&file, b"security.capability", &mut []).await,
                Err(WriteError::XattrMissing { .. })
            ));
        }
        let directory = lookup(&tx, &DIRECTORY[1..]).await;
        tx.set_owner(&directory, Some(1000), Some(1000), when(1))
            .await
            .unwrap();
        assert_eq!(
            value(&tx, &directory, b"security.capability").await,
            CAPABILITY
        );
        tx.commit().await.unwrap();
    });
    block_on(async {
        let snapshot = writer.snapshot().unwrap();
        for ((path, (name, text)), (uid, gid)) in FILES.iter().zip(CJK).zip(owners) {
            assert_eq!(
                read_back(&snapshot, path).await,
                vec![(name.as_bytes().to_vec(), text.as_bytes().to_vec())],
                "{path}"
            );
            let metadata = *snapshot.resolve(path.as_bytes()).await.unwrap().metadata();
            assert_eq!(metadata.uid, uid.unwrap_or(0), "{path}");
            assert_eq!(metadata.gid, gid.unwrap_or(0), "{path}");
            assert_eq!(metadata.ctime, when(1), "{path}");
        }
        assert_eq!(
            read_back(&snapshot, DIRECTORY).await,
            vec![(b"security.capability".to_vec(), CAPABILITY.to_vec())]
        );
    });
}

#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn attribute_edits_pass_btrfs_check() {
    let (named, mut writer) = image();
    assert_btrfs_check(named.path());
    block_on(async {
        let mut tx = writer.join().await.unwrap();
        let file = lookup(&tx, "file").await;
        for (name, text) in CJK {
            tx.set_xattr(
                &file,
                name.as_bytes(),
                text.as_bytes(),
                XattrMode::Set,
                when(1),
            )
            .await
            .unwrap();
        }
        for name in COLLIDING {
            tx.set_xattr(&file, name, b"v", XattrMode::Set, when(2))
                .await
                .unwrap();
        }
        tx.remove_xattr(&file, COLLIDING[0], when(3)).await.unwrap();
        tx.commit().await.unwrap();
    });
    assert_btrfs_check(named.path());
}

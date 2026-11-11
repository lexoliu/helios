//! Test images, generated at test time.
//!
//! Nothing here writes btrfs structures by hand. [`ferrosys`] formats the
//! base filesystem the way `mkfs.btrfs --rootdir` lays one out (its own
//! test suite holds every image it writes to `btrfs check`), and
//! [`btrfs_transaction`] then commits a second transaction onto that image
//! through the format's copy-on-write path, adding what a fresh format
//! never contains: compressed extents of all three algorithms, explicit
//! sparse files and preallocated extents. Both are independent clean-room
//! implementations of the format, so a test here never reads back only
//! what this crate itself wrote.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom, Write};

use btrfs_disk::items::{CompressionType, FileExtentItem, Timespec};
use btrfs_transaction::filesystem::Filesystem as TransactionFs;
use btrfs_transaction::inode::InodeArgs;
use btrfs_transaction::transaction::Transaction;
use ferrosys::btrfs::{FormatOptions, SubvolumeRequest, format_to};
use ferrosys::{Metadata, Timestamp, TreeBuilder};
use helios_btrfs::{BlockSource, Filesystem};
use helios_hal::io::{IoError, IoResult};

/// The smallest volume ferrosys formats with its default geometry is
/// about 109 MiB, and the first backup superblock sits at 64 MiB.
pub const VOLUME_BYTES: u64 = 128 << 20;

const BLOCK: u64 = 4096;

/// A disk image holding only the blocks written to it; the rest reads as
/// zeroes. A formatted 128 MiB volume costs only the few megabytes of
/// blocks the formatter and the transaction wrote.
#[derive(Clone, Default)]
pub struct SparseImage {
    blocks: BTreeMap<u64, Box<[u8; BLOCK as usize]>>,
    len: u64,
    cursor: u64,
}

impl SparseImage {
    pub fn new(len: u64) -> Self {
        Self {
            blocks: BTreeMap::new(),
            len,
            cursor: 0,
        }
    }

    /// Copies `buf.len()` bytes at `offset` out of the image.
    pub fn read_bytes(&self, offset: u64, buf: &mut [u8]) {
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let index = at / BLOCK;
            let within = (at % BLOCK) as usize;
            let count = (BLOCK as usize - within).min(buf.len() - done);
            match self.blocks.get(&index) {
                Some(block) => {
                    buf[done..done + count].copy_from_slice(&block[within..within + count])
                }
                None => buf[done..done + count].fill(0),
            }
            done += count;
        }
    }

    /// Copies `data` into the image at `offset`, growing it if needed.
    pub fn write_bytes(&mut self, offset: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let at = offset + done as u64;
            let index = at / BLOCK;
            let within = (at % BLOCK) as usize;
            let count = (BLOCK as usize - within).min(data.len() - done);
            let block = self
                .blocks
                .entry(index)
                .or_insert_with(|| Box::new([0; BLOCK as usize]));
            block[within..within + count].copy_from_slice(&data[done..done + count]);
            done += count;
        }
        self.len = self.len.max(offset + data.len() as u64);
    }

    /// Flips one bit at `offset`.
    pub fn flip_bit(&mut self, offset: u64, bit: u8) {
        let mut byte = [0];
        self.read_bytes(offset, &mut byte);
        byte[0] ^= 1 << bit;
        self.write_bytes(offset, &byte);
    }

    /// The image cut to its first `len` bytes.
    pub fn truncated(&self, len: u64) -> Self {
        let mut image = self.clone();
        image.blocks.retain(|index, _| index * BLOCK < len);
        if let Some((index, block)) = image.blocks.iter_mut().next_back() {
            let keep = len - index * BLOCK;
            if keep < BLOCK {
                block[keep as usize..].fill(0);
            }
        }
        image.len = len;
        image
    }

    /// Every written block offset, ascending.
    pub fn written_blocks(&self) -> impl Iterator<Item = u64> + '_ {
        self.blocks.keys().map(|index| index * BLOCK)
    }

    /// The offset of the first written byte run equal to `needle`, which
    /// must start a block.
    pub fn find_block_starting_with(&self, needle: &[u8]) -> Option<u64> {
        self.blocks
            .iter()
            .find(|(_, block)| block.starts_with(needle))
            .map(|(index, _)| index * BLOCK)
    }
}

impl Read for SparseImage {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.len.saturating_sub(self.cursor);
        let count = (buf.len() as u64).min(available) as usize;
        self.read_bytes(self.cursor, &mut buf[..count]);
        self.cursor += count as u64;
        Ok(count)
    }
}

impl Write for SparseImage {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.write_bytes(self.cursor, data);
        self.cursor += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for SparseImage {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.cursor.checked_add_signed(delta),
        };
        self.cursor = target.ok_or_else(|| io::Error::other("seek before the image start"))?;
        Ok(self.cursor)
    }
}

impl BlockSource for SparseImage {
    fn size_bytes(&self) -> u64 {
        self.len
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
        assert!(
            offset.is_multiple_of(helios_btrfs::READ_ALIGNMENT)
                && (buf.len() as u64).is_multiple_of(helios_btrfs::READ_ALIGNMENT),
            "read of {} bytes at {offset:#x} breaks the BlockSource alignment contract",
            buf.len()
        );
        let in_bounds = offset
            .checked_add(buf.len() as u64)
            .is_some_and(|end| end <= self.len);
        if !in_bounds {
            return Err(IoError::OutOfBounds);
        }
        self.read_bytes(offset, buf);
        Ok(())
    }
}

/// Mounts `image`, driving the future to completion on this thread.
pub fn mount(image: SparseImage) -> Result<Filesystem<SparseImage>, helios_btrfs::Error> {
    futures_lite::future::block_on(Filesystem::mount(image))
}

pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    futures_lite::future::block_on(future)
}

/// A fixed instant, so every generated image is the same bytes.
fn time() -> Timestamp {
    Timestamp::from_secs(1_700_000_000)
}

fn meta(mode: u16) -> Metadata {
    Metadata::new(mode, time()).owned_by(1000, 1000)
}

/// Deterministic, non-repeating file content: a counter run through a
/// multiplicative hash, so a misplaced block never matches by accident.
pub fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|index| {
            let mixed = (index as u32 ^ seed).wrapping_mul(0x9E37_79B1);
            (mixed >> 24) as u8
        })
        .collect()
}

/// Text that compresses well, as source code and logs do.
pub fn compressible(len: usize) -> Vec<u8> {
    let line = "helios btrfs compressed extent — 圧縮 압축 压缩 — line ";
    let mut out = Vec::with_capacity(len);
    let mut index = 0_u32;
    while out.len() < len {
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(index.to_string().as_bytes());
        out.push(b'\n');
        index += 1;
    }
    out.truncate(len);
    out
}

pub const HELLO: &[u8] = b"hello, btrfs\n";
pub const HOSTNAME: &[u8] = b"helios\n";
pub const BIG_LEN: usize = (1 << 20) + 123;
/// Enough entries that `/many` spans tens of leaves under an internal
/// node, so listings and lookups cross leaf boundaries.
pub const MANY_FILES: usize = 1200;
pub const CJK_FILES: [(&str, &str); 3] = [
    ("中文.txt", "你好，世界。这是一个文件系统测试。\n"),
    (
        "日本語.txt",
        "こんにちは、世界。これはファイルシステムのテストです。\n",
    ),
    (
        "한국어.txt",
        "안녕하세요, 세계. 이것은 파일 시스템 테스트입니다.\n",
    ),
];

pub fn many_name(index: usize) -> String {
    format!("entry-{index:05}")
}

pub fn many_content(index: usize) -> Vec<u8> {
    format!("file number {index}\n").into_bytes()
}

/// The tree the base image is formatted from.
pub fn base_tree() -> TreeBuilder {
    let mut tree = TreeBuilder::new()
        .root(meta(0o755))
        .file("/hello.txt", HELLO, meta(0o644))
        .xattr("user.comment", "a greeting")
        .xattr("user.lang", "en")
        .file("/empty", Vec::new(), meta(0o600))
        .file("/big.bin", pattern(BIG_LEN, 7), meta(0o644))
        .directory("/etc", meta(0o755))
        .file("/etc/hostname", HOSTNAME, meta(0o644))
        .hardlink("/etc/hostname-link", "/etc/hostname", meta(0o644))
        .symlink("/etc/hostname-abs", "/etc/hostname", meta(0o777))
        .symlink("/etc/up", "../hello.txt", meta(0o777))
        .symlink("/etc/dangling", "missing/file", meta(0o777))
        .symlink("/etc/loop-a", "loop-b", meta(0o777))
        .symlink("/etc/loop-b", "loop-a", meta(0o777))
        .directory("/docs", meta(0o755))
        .directory("/docs/nested", meta(0o700))
        .directory("/docs/nested/deeper", meta(0o700))
        .file("/docs/nested/deeper/leaf.txt", b"leaf\n", meta(0o644))
        .symlink("/docs/big", "../big.bin", meta(0o777))
        .directory("/many", meta(0o755));
    for (name, content) in CJK_FILES {
        tree = tree.file(format!("/docs/{name}"), content.as_bytes(), meta(0o644));
    }
    for index in 0..MANY_FILES {
        tree = tree.file(
            format!("/many/{}", many_name(index)),
            many_content(index),
            meta(0o644),
        );
    }
    tree.directory("/@home", meta(0o755)).file(
        "/@home/notes.txt",
        b"inside a subvolume\n",
        meta(0o644),
    )
}

fn options() -> FormatOptions {
    FormatOptions::new([0x5a; 16], time())
        .chunk_tree_uuid([0x22; 16])
        .device_uuid([0x33; 16])
        .subvolume_uuid([0x44; 16])
        .subvolume(SubvolumeRequest::new(b"/@home".to_vec(), [0x55; 16]))
}

/// The base image: [`base_tree`] formatted by ferrosys.
pub fn base_image() -> SparseImage {
    let mut image = SparseImage::new(VOLUME_BYTES);
    format_to(&mut image, base_tree(), VOLUME_BYTES, options()).expect("ferrosys formats the tree");
    image.cursor = 0;
    image
}

/// The base image with `/@home` as the default subvolume.
pub fn default_subvolume_image() -> SparseImage {
    let mut image = SparseImage::new(VOLUME_BYTES);
    let options = options().default_subvolume(b"/@home".to_vec());
    format_to(&mut image, base_tree(), VOLUME_BYTES, options).expect("ferrosys formats the tree");
    image.cursor = 0;
    image
}

pub const ZLIB_INODE: u64 = 100_001;
pub const LZO_INODE: u64 = 100_002;
pub const ZSTD_INODE: u64 = 100_003;
pub const ZSTD_INLINE_INODE: u64 = 100_004;
pub const SPARSE_INODE: u64 = 100_005;
pub const PREALLOC_INODE: u64 = 100_006;
/// Where the sparse file's second data run starts.
pub const SPARSE_SECOND_RUN: u64 = 3 << 20;
pub const SPARSE_SIZE: u64 = 5 << 20;
pub const PREALLOC_SIZE: u64 = 64 << 10;

const FS_TREE: u64 = 5;
const TOP_DIR: u64 = 256;
const FILE_TYPE_REGULAR: u8 = 1;

/// The base image plus a committed transaction adding compressed,
/// sparse and preallocated files.
pub fn extents_image() -> SparseImage {
    let mut fs = TransactionFs::open(base_image()).expect("btrfs-transaction opens the image");
    let mut tx = Transaction::start(&mut fs).expect("transaction starts");
    let when = Timespec {
        sec: 1_700_000_100,
        nsec: 0,
    };
    let mut index = 1_000_000;
    let mut add = |tx: &mut Transaction<SparseImage>,
                   fs: &mut TransactionFs<SparseImage>,
                   inode: u64,
                   name: &[u8],
                   size: u64| {
        let mut args = InodeArgs::new(tx.transid, 0o100_644).with_uniform_time(when);
        args.size = size;
        tx.create_inode(fs, FS_TREE, inode, &args)
            .expect("inode created");
        tx.link_dir_entry(
            fs,
            FS_TREE,
            TOP_DIR,
            inode,
            name,
            FILE_TYPE_REGULAR,
            index,
            when,
        )
        .expect("entry linked");
        index += 1;
    };

    for (inode, name, compression) in [
        (ZLIB_INODE, "zlib.txt", CompressionType::Zlib),
        (LZO_INODE, "lzo.txt", CompressionType::Lzo),
        (ZSTD_INODE, "zstd.txt", CompressionType::Zstd),
    ] {
        let content = compressible(300 * 1024 + 77);
        add(
            &mut tx,
            &mut fs,
            inode,
            name.as_bytes(),
            content.len() as u64,
        );
        // The kernel compresses at most 128 KiB into one extent, and a
        // reader refuses more; write the file the way the kernel would.
        for (index, piece) in content.chunks(128 << 10).enumerate() {
            let offset = (index as u64) << 17;
            tx.write_file_data(
                &mut fs,
                FS_TREE,
                inode,
                offset,
                piece,
                false,
                Some(compression),
            )
            .expect("compressed data written");
        }
    }

    let inline = compressible(3000);
    add(
        &mut tx,
        &mut fs,
        ZSTD_INLINE_INODE,
        b"zstd-inline.txt",
        inline.len() as u64,
    );
    tx.write_file_data(
        &mut fs,
        FS_TREE,
        ZSTD_INLINE_INODE,
        0,
        &inline,
        false,
        Some(CompressionType::Zstd),
    )
    .expect("inline compressed data written");

    add(&mut tx, &mut fs, SPARSE_INODE, b"sparse.bin", SPARSE_SIZE);
    tx.write_file_data(
        &mut fs,
        FS_TREE,
        SPARSE_INODE,
        0,
        &pattern(8192, 11),
        false,
        None,
    )
    .expect("first run written");
    tx.write_file_data(
        &mut fs,
        FS_TREE,
        SPARSE_INODE,
        SPARSE_SECOND_RUN,
        &pattern(12288, 13),
        false,
        None,
    )
    .expect("second run written");

    add(
        &mut tx,
        &mut fs,
        PREALLOC_INODE,
        b"prealloc.bin",
        PREALLOC_SIZE,
    );
    let logical = tx
        .reserve_data_extent(&mut fs, PREALLOC_SIZE, FS_TREE, PREALLOC_INODE, 0)
        .expect("extent reserved");
    let item = FileExtentItem::to_bytes_regular(
        tx.transid,
        PREALLOC_SIZE,
        CompressionType::None,
        true,
        logical,
        PREALLOC_SIZE,
        0,
        PREALLOC_SIZE,
    );
    tx.insert_file_extent(&mut fs, FS_TREE, PREALLOC_INODE, 0, &item)
        .expect("prealloc extent inserted");

    tx.commit(&mut fs).expect("transaction commits");
    let devices = fs.reader_mut().devices_mut();
    let device = devices.values_mut().next().expect("one device");
    let mut image = std::mem::take(device);
    image.cursor = 0;
    image
}

/// The content of a file [`extents_image`] added, by inode.
pub fn added_content(inode: u64) -> Vec<u8> {
    match inode {
        ZLIB_INODE | LZO_INODE | ZSTD_INODE => compressible(300 * 1024 + 77),
        ZSTD_INLINE_INODE => compressible(3000),
        SPARSE_INODE => {
            let mut content = vec![0; SPARSE_SIZE as usize];
            content[..8192].copy_from_slice(&pattern(8192, 11));
            let second = SPARSE_SECOND_RUN as usize;
            content[second..second + 12288].copy_from_slice(&pattern(12288, 13));
            content
        }
        PREALLOC_INODE => vec![0; PREALLOC_SIZE as usize],
        _ => panic!("inode {inode} was not added by the fixture"),
    }
}

/// Reads a whole file through `fs`, in reads of `chunk` bytes.
pub fn read_all(
    fs: &Filesystem<SparseImage>,
    inode: &helios_btrfs::Inode,
    chunk: usize,
) -> Result<Vec<u8>, helios_btrfs::Error> {
    let mut out = Vec::new();
    let mut buf = vec![0; chunk];
    loop {
        let count = block_on(fs.read(inode, out.len() as u64, &mut buf))?;
        if count == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..count]);
    }
}

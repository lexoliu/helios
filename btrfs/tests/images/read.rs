//! An intact image reads back exactly what it was formatted from.

use core::ops::ControlFlow;

use helios_btrfs::{ChecksumType, DirPosition, EntryTarget, Error, FileKind};

use crate::fixture::{
    BIG_LEN, CJK_FILES, HELLO, HOSTNAME, MANY_FILES, base_image, block_on, default_subvolume_image,
    many_content, many_name, mount, pattern, read_all,
};
use crate::walk::walk;

#[test]
fn mounts_and_reports_the_superblock() {
    let fs = mount(base_image()).expect("mounts");
    let superblock = fs.superblock();
    assert_eq!(superblock.fsid(), [0x5a; 16]);
    assert_eq!(superblock.csum_type(), ChecksumType::Crc32c);
    assert_eq!(superblock.sectorsize(), 4096);
    assert_eq!(superblock.nodesize(), 16384);
    assert_eq!(superblock.mirror(), 0);
    assert_eq!(fs.default_subvolume(), 5);
}

#[test]
fn reads_small_and_empty_files() {
    let fs = mount(base_image()).expect("mounts");
    let hello = block_on(fs.resolve(b"/hello.txt")).expect("resolves");
    assert_eq!(hello.kind(), FileKind::Regular);
    assert_eq!(hello.metadata().permissions, 0o644);
    assert_eq!(hello.metadata().uid, 1000);
    assert_eq!(hello.metadata().size, HELLO.len() as u64);
    assert_eq!(read_all(&fs, &hello, 4096).expect("reads"), HELLO);

    let empty = block_on(fs.resolve(b"empty")).expect("resolves");
    assert_eq!(empty.metadata().permissions, 0o600);
    assert_eq!(read_all(&fs, &empty, 4096).expect("reads"), b"");
}

#[test]
fn reads_a_regular_file_in_any_chunking() {
    let fs = mount(base_image()).expect("mounts");
    let big = block_on(fs.resolve(b"/big.bin")).expect("resolves");
    let expected = pattern(BIG_LEN, 7);
    for chunk in [4096, 1000, 131_072 + 17, 1 << 21] {
        assert_eq!(
            read_all(&fs, &big, chunk).expect("reads"),
            expected,
            "chunk size {chunk}"
        );
    }

    // Unaligned reads straddling sector and window boundaries.
    for (offset, len) in [(1, 10), (4095, 2), (131_071, 3), (BIG_LEN as u64 - 5, 100)] {
        let mut buf = vec![0; len];
        let count = block_on(fs.read(&big, offset, &mut buf)).expect("reads");
        let start = offset as usize;
        let end = (start + len).min(BIG_LEN);
        assert_eq!(count, end - start);
        assert_eq!(&buf[..count], &expected[start..end]);
    }
    let mut buf = [0; 8];
    assert_eq!(
        block_on(fs.read(&big, BIG_LEN as u64, &mut buf)).expect("reads"),
        0
    );
}

#[test]
fn lists_the_root_directory() {
    let fs = mount(base_image()).expect("mounts");
    let root = block_on(fs.root()).expect("root");
    let mut entries = Vec::new();
    let end = block_on(fs.read_dir(&root, DirPosition::START, |entry| {
        entries.push((
            String::from_utf8(entry.name.to_vec()).unwrap(),
            entry.kind,
            entry.target,
        ));
        ControlFlow::Continue(())
    }))
    .expect("lists");
    assert_eq!(end, None);
    let mut names: Vec<_> = entries.iter().map(|(name, ..)| name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "@home",
            "big.bin",
            "docs",
            "empty",
            "etc",
            "hello.txt",
            "many"
        ]
    );
    let home = entries.iter().find(|(name, ..)| name == "@home").unwrap();
    assert_eq!(home.1, FileKind::Directory);
    assert!(matches!(home.2, EntryTarget::Subvolume(tree) if tree >= 256));
}

#[test]
fn lists_a_large_directory_in_pages() {
    let fs = mount(base_image()).expect("mounts");
    let many = block_on(fs.resolve(b"/many")).expect("resolves");
    let mut names = Vec::new();
    let mut position = Some(DirPosition::START);
    let mut pages = 0;
    while let Some(from) = position {
        let mut taken = 0;
        position = block_on(fs.read_dir(&many, from, |entry| {
            if taken == 97 {
                return ControlFlow::Break(());
            }
            taken += 1;
            names.push(String::from_utf8(entry.name.to_vec()).unwrap());
            ControlFlow::Continue(())
        }))
        .expect("lists");
        pages += 1;
    }
    assert_eq!(pages, MANY_FILES.div_ceil(97));
    names.sort_unstable();
    let expected: Vec<_> = (0..MANY_FILES).map(many_name).collect();
    assert_eq!(names, expected);
}

#[test]
fn looks_up_names_in_a_large_directory() {
    let fs = mount(base_image()).expect("mounts");
    let many = block_on(fs.resolve(b"/many")).expect("resolves");
    for index in (0..MANY_FILES).step_by(37) {
        let file = block_on(fs.lookup(&many, many_name(index).as_bytes()))
            .expect("looks up")
            .expect("exists");
        assert_eq!(
            read_all(&fs, &file, 4096).expect("reads"),
            many_content(index)
        );
    }
    assert!(
        block_on(fs.lookup(&many, b"entry-99999"))
            .expect("looks up")
            .is_none()
    );
    assert!(
        block_on(fs.lookup(&many, b"."))
            .expect("looks up")
            .is_none()
    );
}

#[test]
fn reads_files_with_chinese_japanese_and_korean_names() {
    let fs = mount(base_image()).expect("mounts");
    for (name, content) in CJK_FILES {
        let file = block_on(fs.resolve(format!("/docs/{name}").as_bytes())).expect("resolves");
        assert_eq!(
            read_all(&fs, &file, 4096).expect("reads"),
            content.as_bytes()
        );
    }
}

#[test]
fn follows_symlinks() {
    let fs = mount(base_image()).expect("mounts");
    let link = block_on(fs.resolve(b"/etc")).expect("resolves");
    let abs = block_on(fs.lookup(&link, b"hostname-abs"))
        .expect("looks up")
        .expect("exists");
    assert_eq!(abs.kind(), FileKind::Symlink);
    let mut target = [0; 64];
    let len = block_on(fs.read_link(&abs, &mut target)).expect("reads the link");
    assert_eq!(&target[..len], b"/etc/hostname");

    for (path, expected) in [
        (&b"/etc/hostname-abs"[..], HOSTNAME),
        (b"/etc/up", HELLO),
        (b"etc/./up", HELLO),
        (b"/docs/nested/deeper/../../../hello.txt", HELLO),
        (b"/../../hello.txt", HELLO),
    ] {
        let file = block_on(fs.resolve(path)).expect("resolves");
        assert_eq!(read_all(&fs, &file, 4096).expect("reads"), expected);
    }
    let big = block_on(fs.resolve(b"/docs/big")).expect("resolves");
    assert_eq!(big.metadata().size, BIG_LEN as u64);

    assert!(matches!(
        block_on(fs.resolve(b"/etc/dangling")),
        Err(Error::NotFound { .. })
    ));
    assert_eq!(
        block_on(fs.resolve(b"/etc/loop-a")),
        Err(Error::SymlinkLimit { limit: 40 })
    );
}

#[test]
fn a_hard_link_is_the_same_inode() {
    let fs = mount(base_image()).expect("mounts");
    let original = block_on(fs.resolve(b"/etc/hostname")).expect("resolves");
    let link = block_on(fs.resolve(b"/etc/hostname-link")).expect("resolves");
    assert_eq!(original.number(), link.number());
    assert_eq!(original.metadata().nlink, 2);
}

#[test]
fn reads_extended_attributes() {
    let fs = mount(base_image()).expect("mounts");
    let hello = block_on(fs.resolve(b"/hello.txt")).expect("resolves");
    let mut value = [0; 64];
    let len = block_on(fs.xattr(&hello, b"user.comment", &mut value))
        .expect("reads")
        .expect("exists");
    assert_eq!(&value[..len], b"a greeting");
    let mut short = [0; 3];
    assert_eq!(
        block_on(fs.xattr(&hello, b"user.comment", &mut short)).expect("reads"),
        Some(10)
    );
    assert_eq!(&short, b"a g");
    assert_eq!(
        block_on(fs.xattr(&hello, b"user.missing", &mut value)).expect("reads"),
        None
    );

    let mut names = Vec::new();
    block_on(fs.list_xattrs(&hello, |name| {
        names.push(name.to_vec());
        ControlFlow::Continue(())
    }))
    .expect("lists");
    names.sort_unstable();
    assert_eq!(names, [b"user.comment".to_vec(), b"user.lang".to_vec()]);
}

#[test]
fn crosses_into_a_subvolume() {
    let fs = mount(base_image()).expect("mounts");
    let notes = block_on(fs.resolve(b"/@home/notes.txt")).expect("resolves");
    assert!(notes.tree() >= 256);
    assert_eq!(
        read_all(&fs, &notes, 4096).expect("reads"),
        b"inside a subvolume\n"
    );
}

#[test]
fn mounts_the_default_subvolume() {
    let fs = mount(default_subvolume_image()).expect("mounts");
    assert!(fs.default_subvolume() >= 256);
    let notes = block_on(fs.resolve(b"/notes.txt")).expect("resolves");
    assert_eq!(
        read_all(&fs, &notes, 4096).expect("reads"),
        b"inside a subvolume\n"
    );
    assert!(matches!(
        block_on(fs.resolve(b"/hello.txt")),
        Err(Error::NotFound { .. })
    ));
    let top = block_on(fs.subvolume_root(5)).expect("top level");
    let hello = block_on(fs.lookup(&top, b"hello.txt"))
        .expect("looks up")
        .expect("exists");
    assert_eq!(read_all(&fs, &hello, 4096).expect("reads"), HELLO);
}

#[test]
fn refuses_the_wrong_kind_of_inode() {
    let fs = mount(base_image()).expect("mounts");
    let root = block_on(fs.root()).expect("root");
    let hello = block_on(fs.resolve(b"/hello.txt")).expect("resolves");
    let mut buf = [0; 16];
    assert!(matches!(
        block_on(fs.read(&root, 0, &mut buf)),
        Err(Error::WrongKind {
            expected: FileKind::Regular,
            found: FileKind::Directory,
            ..
        })
    ));
    assert!(matches!(
        block_on(fs.lookup(&hello, b"x")),
        Err(Error::WrongKind {
            expected: FileKind::Directory,
            ..
        })
    ));
    assert!(matches!(
        block_on(fs.read_link(&hello, &mut buf)),
        Err(Error::WrongKind {
            expected: FileKind::Symlink,
            ..
        })
    ));
}

#[test]
fn walks_every_inode() {
    let fs = mount(base_image()).expect("mounts");
    let census = walk(&fs).expect("walks");
    // The root, /etc, /docs, two nested, /many and the subvolume root.
    assert_eq!(census.directories, 7);
    assert_eq!(census.subvolumes, 1);
    assert_eq!(census.symlinks, 6);
    // Hello, empty, big, hostname and its link, leaf, CJK, many, notes.
    assert_eq!(census.files, 6 + CJK_FILES.len() + MANY_FILES + 1);
}

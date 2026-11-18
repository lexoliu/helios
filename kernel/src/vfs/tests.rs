use alloc::string::String;
use alloc::vec::Vec;

use futures_lite::future::block_on;
use helios_hal::cpu::ProcessorId;
use helios_hal::fs::{
    FileSystem, FileSystemRights, FsError, FsPath, OpenFlags, RenameMode, WritePosition,
};

use super::{
    BOOT_DEVICE, Follow, MAX_MOUNTS, MemoryFs, MountError, MountGrant, MountSharing, MountTable,
    MountedFs, Namespace,
};
use crate::test_support::UnsupportedStorage;
use crate::{
    BootFs, ComponentFsPathError, EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs,
    MountCap, ProcessAuthority, resolve_child_path,
};

type TestNamespace = Namespace<UnsupportedStorage>;

const IMAGE: EmbeddedBootFs = EmbeddedBootFs::new(
    &[EmbeddedBootDirectory::new("mnt", 0)],
    &[EmbeddedBootFile::new("bin/tool", b"tool", 42)],
);

const READ_WRITE: MountGrant = MountGrant {
    rights: FileSystemRights::READ.union(FileSystemRights::WRITE),
    sharing: MountSharing::Shared,
};

/// A request for `source` at `/mnt`, shared, with no btrfs options.
fn request(source: crate::MountSource, read_only: bool) -> crate::MountRequest<'static> {
    crate::MountRequest {
        source,
        point: "/mnt",
        read_only,
        sharing: MountSharing::Shared,
        write: crate::vfs::BtrfsWriteOptions {
            entropy: crate::test_support::test_root_entropy(),
            concurrency: core::num::NonZeroUsize::new(1).unwrap(),
            compress: None,
            commit: None,
        },
    }
}

fn namespace() -> TestNamespace {
    Namespace::new(MountTable::new(BootFs::new(IMAGE, BOOT_DEVICE), 1))
}

fn memory(namespace: &TestNamespace) -> MountedFs<UnsupportedStorage> {
    MountedFs::Memory(MemoryFs::new(namespace.table().allocate_device()))
}

/// A machine with neither a host share nor a disk.
fn nothing_attached() -> crate::AttachedStorage<UnsupportedStorage> {
    crate::AttachedStorage {
        host_share: None,
        disks: None,
    }
}

fn cap(point: &str) -> MountCap {
    ProcessAuthority::root()
        .derive_mount_cap(point)
        .expect("root may mount anywhere")
}

fn mount_memory(namespace: &TestNamespace, point: &str) {
    block_on(namespace.mount(&cap(point), memory(namespace), READ_WRITE)).expect("mounts");
}

fn owner(namespace: &TestNamespace, absolute: &str) -> (String, String) {
    let resolved = namespace.resolve(absolute).expect("resolves");
    (
        String::from(resolved.mount_point()),
        String::from(resolved.path().as_str()),
    )
}

fn create(namespace: &TestNamespace, absolute: &str, bytes: &[u8]) {
    let resolved = namespace.resolve(absolute).expect("resolves");
    let fs = resolved.writable().expect("writable mount");
    block_on(fs.open(resolved.path(), OpenFlags::CREATE, 1)).expect("creates");
    block_on(super::write_bytes(
        fs,
        resolved.path(),
        WritePosition::At(0),
        bytes,
        1,
    ))
    .expect("writes");
}

fn read(namespace: &TestNamespace, absolute: &str) -> Vec<u8> {
    let resolved = namespace.resolve(absolute).expect("resolves");
    block_on(resolved.fs().read_at(resolved.path(), 0, 64))
        .expect("reads")
        .to_vec()
}

#[test]
fn the_longest_mount_point_owns_a_path() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    let mnt = resolve_child_path("/", "mnt").expect("normalizes");
    block_on(
        namespace
            .resolve(&mnt)
            .expect("resolves")
            .writable()
            .expect("writable")
            .create_directory(FsPath::new("data").expect("valid"), 1),
    )
    .expect("creates the inner mount point");
    mount_memory(&namespace, "/mnt/data");

    assert_eq!(
        owner(&namespace, "/mnt/data/x/y"),
        ("/mnt/data".into(), "x/y".into())
    );
    assert_eq!(
        owner(&namespace, "/mnt/data"),
        ("/mnt/data".into(), "".into())
    );
    assert_eq!(
        owner(&namespace, "/mnt/datax"),
        ("/mnt".into(), "datax".into())
    );
    assert_eq!(owner(&namespace, "/mnt"), ("/mnt".into(), "".into()));
    assert_eq!(
        owner(&namespace, "/bin/tool"),
        ("/".into(), "bin/tool".into())
    );
    assert_eq!(owner(&namespace, "/"), ("/".into(), "".into()));
}

#[test]
fn dot_dot_climbs_out_of_a_mount_into_the_one_beneath() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    create(&namespace, "/mnt/inside", b"in the mount");

    // From the root, `..` inside the mounted tree climbs back to the
    // root mount, which owns `/bin/tool`.
    let climbed = resolve_child_path("/", "mnt/../bin/tool").expect("normalizes");
    assert_eq!(climbed, "/bin/tool");
    assert_eq!(owner(&namespace, &climbed), ("/".into(), "bin/tool".into()));
    assert_eq!(read(&namespace, &climbed), b"tool");

    // And from the root into the mount and back into it.
    let reentered = resolve_child_path("/", "bin/../mnt/./inside").expect("normalizes");
    assert_eq!(
        owner(&namespace, &reentered),
        ("/mnt".into(), "inside".into())
    );
    assert_eq!(read(&namespace, &reentered), b"in the mount");

    // A descriptor opened on the mount's root cannot climb out of it.
    assert_eq!(
        resolve_child_path("/mnt", "../bin/tool"),
        Err(ComponentFsPathError::NotPermitted)
    );
}

#[test]
fn rename_and_link_across_mounts_are_refused_with_exdev() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    create(&namespace, "/mnt/file", b"payload");

    assert_eq!(
        block_on(namespace.rename("/mnt/file", "/file", RenameMode::Replace, 2)),
        Err(FsError::CrossDevice)
    );
    assert_eq!(
        block_on(namespace.link("/mnt/file", "/file", 2)),
        Err(FsError::CrossDevice)
    );
    // Nothing moved.
    assert_eq!(read(&namespace, "/mnt/file"), b"payload");
    assert_eq!(
        block_on(namespace.stat("/file", Follow::No)).map(drop),
        Err(FsError::NoEntry)
    );
    // Within the mount, both work.
    block_on(namespace.rename("/mnt/file", "/mnt/moved", RenameMode::Replace, 2)).expect("renames");
    block_on(namespace.link("/mnt/moved", "/mnt/linked", 2)).expect("links");
    assert_eq!(read(&namespace, "/mnt/linked"), b"payload");
}

#[test]
fn a_mount_point_is_listed_in_its_parent_and_unmount_uncovers_it() {
    let namespace = namespace();
    mount_memory(&namespace, "/scratch");
    create(&namespace, "/scratch/f", b"x");
    let mut names: Vec<String> = block_on(namespace.list("/"))
        .expect("lists")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    assert_eq!(names, ["bin", "mnt", "scratch"]);

    block_on(namespace.table().unmount("/scratch")).expect("unmounts");
    assert_eq!(
        owner(&namespace, "/scratch/f"),
        ("/".into(), "scratch/f".into())
    );
    assert_eq!(
        block_on(namespace.stat("/scratch/f", Follow::No)).map(drop),
        Err(FsError::NoEntry)
    );
}

#[test]
fn a_cancelled_unmount_leaves_the_mount_live() {
    let namespace = namespace();
    mount_memory(&namespace, "/scratch");
    create(&namespace, "/scratch/f", b"kept");

    // A resolution that has not let go keeps the unmount waiting; the
    // unmount is dropped while it waits.
    let looking = namespace.resolve("/scratch/f").expect("resolves");
    {
        let mut unmount = core::pin::pin!(namespace.table().unmount("/scratch"));
        assert_eq!(
            block_on(futures_lite::future::poll_once(&mut unmount)),
            None
        );
        assert_eq!(owner(&namespace, "/scratch/f").0, "/");
    }
    drop(looking);

    assert_eq!(
        owner(&namespace, "/scratch/f"),
        ("/scratch".into(), "f".into())
    );
    assert_eq!(read(&namespace, "/scratch/f"), b"kept");
    block_on(namespace.table().unmount("/scratch")).expect("a later unmount completes");
}

/// A resolution writes one count: its own processor's, for the slot it
/// resolves through. A task that moves to another processor before it
/// lets go releases its pin there, and the unmount it held up completes.
#[test]
fn a_resolution_pins_on_its_own_processor_and_may_let_go_on_another() {
    let namespace: TestNamespace =
        Namespace::new(MountTable::new(BootFs::new(IMAGE, BOOT_DEVICE), 2));
    mount_memory(&namespace, "/scratch");
    mount_memory(&namespace, "/other");
    let pinned = |processor: usize| {
        namespace.table().inner.pins.blocks[processor]
            .iter()
            .filter(|count| count.load(core::sync::atomic::Ordering::SeqCst) != 0)
            .count()
    };

    crate::test_processor_identity::set(ProcessorId::new(1));
    let looking = namespace.resolve("/scratch/f").expect("resolves");
    assert_eq!((pinned(0), pinned(1)), (0, 1));

    let mut unmount = core::pin::pin!(namespace.table().unmount("/scratch"));
    crate::test_processor_identity::set(ProcessorId::new(0));
    assert_eq!(
        block_on(futures_lite::future::poll_once(&mut unmount)),
        None
    );
    drop(looking);
    assert_eq!(block_on(unmount), Ok(()));
    assert_eq!(owner(&namespace, "/scratch/f").0, "/");
    assert_eq!(owner(&namespace, "/other/f").0, "/other");
}

/// A scan picks a mount by its point's published length and hash; a
/// candidate whose point turns out to differ (a hash that collided) is
/// ruled out under its pin, and the path resolves past it.
#[test]
fn a_colliding_point_hash_is_ruled_out() {
    let namespace = namespace();
    mount_memory(&namespace, "/aa");
    let table = &namespace.table().inner;
    let index = table
        .slots
        .iter()
        .position(|slot| slot.state.load(core::sync::atomic::Ordering::SeqCst) == super::SLOT_LIVE)
        .expect("the mount has a slot");
    table.slots[index].point_hash.store(
        super::point_hash(b"/bb"),
        core::sync::atomic::Ordering::SeqCst,
    );

    assert_eq!(owner(&namespace, "/bb/x"), ("/".into(), "bb/x".into()));
    assert_eq!(table.pins.total(index), 0, "the ruled-out pin was released");
}

#[test]
fn unmount_refuses_a_mount_with_mounts_below_it() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    block_on(
        namespace
            .resolve("/mnt")
            .expect("resolves")
            .writable()
            .expect("writable")
            .create_directory(FsPath::new("inner").expect("valid"), 1),
    )
    .expect("creates");
    mount_memory(&namespace, "/mnt/inner");
    assert_eq!(
        block_on(namespace.table().unmount("/mnt")),
        Err(MountError::Busy("/mnt".into()))
    );
    block_on(namespace.table().unmount("/mnt/inner")).expect("unmounts the inner one");
    block_on(namespace.table().unmount("/mnt")).expect("then the outer one");
    assert_eq!(
        block_on(namespace.table().unmount("/mnt")),
        Err(MountError::NotMounted("/mnt".into()))
    );
}

#[test]
fn a_read_only_mount_refuses_changes() {
    let namespace = namespace();
    block_on(namespace.mount(
        &cap("/mnt"),
        MountedFs::Boot(namespace.table().boot().clone()),
        MountGrant {
            rights: FileSystemRights::READ,
            sharing: MountSharing::Private,
        },
    ))
    .expect("mounts");
    assert_eq!(read(&namespace, "/mnt/bin/tool"), b"tool");
    assert_eq!(
        namespace
            .resolve("/mnt/bin/tool")
            .expect("resolves")
            .writable()
            .map(drop),
        Err(FsError::ReadOnly)
    );
}

#[test]
fn a_mount_point_must_be_a_directory_under_an_existing_one() {
    let namespace = namespace();
    let refused =
        |point: &str| block_on(namespace.mount(&cap(point), memory(&namespace), READ_WRITE));
    assert_eq!(refused("/"), Err(MountError::InvalidPoint("/".into())));
    assert_eq!(
        refused("/missing/point"),
        Err(MountError::Filesystem {
            point: "/missing/point".into(),
            error: FsError::NoEntry,
        })
    );
    assert_eq!(
        refused("/bin/tool"),
        Err(MountError::Filesystem {
            point: "/bin/tool".into(),
            error: FsError::NotDirectory,
        })
    );
    mount_memory(&namespace, "/mnt");
    assert_eq!(
        refused("/mnt"),
        Err(MountError::AlreadyMounted("/mnt".into()))
    );
    let root = namespace.root.clone();
    assert_eq!(
        namespace.table().mount("/other", root, READ_WRITE),
        Err(MountError::RootKind)
    );
}

#[test]
fn the_table_refuses_a_mount_past_its_capacity() {
    let namespace = namespace();
    for index in 0..MAX_MOUNTS {
        let point = alloc::format!("/m{index}");
        namespace
            .table()
            .mount(&point, memory(&namespace), READ_WRITE)
            .expect("fits");
    }
    assert_eq!(
        namespace
            .table()
            .mount("/one-more", memory(&namespace), READ_WRITE),
        Err(MountError::TableFull)
    );
}

fn unmount_cap(point: &str) -> crate::UnmountCap {
    ProcessAuthority::root()
        .derive_unmount_cap(point)
        .expect("root may unmount anywhere")
}

#[test]
fn a_mount_belongs_to_its_tree_and_the_trees_below_it() {
    let root = namespace();
    let tree = root.descend();
    let same_tree = tree.clone();
    let below = tree.descend();
    let other = root.descend();
    mount_memory(&root, "/mnt");
    create(&root, "/mnt/kernel", "日本語".as_bytes());
    mount_memory(&tree, "/scratch");
    create(&tree, "/scratch/tree", "한국어".as_bytes());

    // The root namespace's mount reaches every tree below it.
    for namespace in [&tree, &same_tree, &below, &other] {
        assert_eq!(read(namespace, "/mnt/kernel"), "日本語".as_bytes());
    }
    // A tree's own mount reaches the tree and its descendants only.
    for namespace in [&same_tree, &below] {
        assert_eq!(read(namespace, "/scratch/tree"), "한국어".as_bytes());
    }
    for namespace in [&root, &other] {
        assert_eq!(owner(namespace, "/scratch/tree").0, "/");
        assert_eq!(
            block_on(namespace.stat("/scratch/tree", Follow::No)).map(drop),
            Err(FsError::NoEntry)
        );
    }
    // Only the tree that made a mount removes it.
    for namespace in [&root, &other, &below] {
        assert_eq!(
            block_on(namespace.unmount(&unmount_cap("/scratch"))),
            Err(MountError::NotMounted("/scratch".into()))
        );
    }
    assert_eq!(
        block_on(tree.unmount(&unmount_cap("/mnt"))),
        Err(MountError::NotMounted("/mnt".into()))
    );
    block_on(same_tree.unmount(&unmount_cap("/scratch"))).expect("the tree unmounts its own");
    assert_eq!(owner(&below, "/scratch/tree").0, "/");
}

#[test]
fn a_tree_shares_its_root_layer_and_a_descendant_starts_its_own() {
    let root = namespace();
    let tree = root.descend();
    let same_tree = tree.clone();
    let below = tree.descend();
    create(&tree, "/private", b"one tree");
    assert_eq!(read(&same_tree, "/private"), b"one tree");
    for namespace in [&root, &below] {
        assert_eq!(
            block_on(namespace.stat("/private", Follow::No)).map(drop),
            Err(FsError::NoEntry)
        );
    }
}

#[test]
fn a_symbolic_link_is_followed_across_mounts_only_when_asked() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    create(&namespace, "/mnt/target", b"followed");
    let resolved = namespace.resolve("/mnt/link").expect("resolves");
    block_on(
        resolved
            .writable()
            .expect("writable")
            .symlink("target", resolved.path(), 1),
    )
    .expect("links");
    let followed =
        block_on(namespace.resolve_following("/mnt/link", Follow::Within("/"))).expect("follows");
    assert_eq!(followed.absolute(), "/mnt/target");
    let unfollowed =
        block_on(namespace.resolve_following("/mnt/link", Follow::No)).expect("resolves");
    assert_eq!(unfollowed.absolute(), "/mnt/link");
}

#[test]
fn a_mount_request_without_mount_authority_is_refused() {
    let namespace = namespace();
    let mut authority = ProcessAuthority::empty();
    authority.insert_directory_preopen(
        crate::DirectoryPreopen::new(
            "/",
            "/",
            crate::DirectoryAuthorityRights::READ
                | crate::DirectoryAuthorityRights::MUTATE_DIRECTORY,
        )
        .expect("valid preopen"),
    );

    assert!(matches!(
        block_on(crate::request_mount(
            &authority,
            &namespace,
            nothing_attached(),
            request(crate::MountSource::Memory, false),
        )),
        Err(crate::MountRequestError::Authority(
            crate::ProcessAuthorityError::MountGrantExceedsAuthority(
                crate::MountAuthorityRights::MOUNT
            )
        ))
    ));
    assert_eq!(owner(&namespace, "/mnt/x").0, "/");
}

#[test]
fn a_mount_request_with_mount_authority_attaches_the_source() {
    let namespace = namespace();
    let authority = ProcessAuthority::root();

    block_on(crate::request_mount(
        &authority,
        &namespace,
        nothing_attached(),
        crate::MountRequest {
            sharing: MountSharing::Private,
            ..request(crate::MountSource::BootImage, false)
        },
    ))
    .expect("root may mount the boot image");
    // The grant the request asked for is the one the mount holds, and the
    // boot image is read-only whatever the request said.
    assert_eq!(
        namespace
            .resolve("/mnt/bin/tool")
            .expect("resolves")
            .grant(),
        MountGrant {
            rights: FileSystemRights::READ,
            sharing: MountSharing::Private,
        }
    );
    assert_eq!(read(&namespace, "/mnt/bin/tool"), b"tool");
    assert_eq!(
        namespace
            .resolve("/mnt/bin/tool")
            .expect("resolves")
            .writable()
            .map(drop),
        Err(FsError::ReadOnly)
    );

    assert!(matches!(
        block_on(crate::request_mount(
            &authority,
            &namespace,
            nothing_attached(),
            request(crate::MountSource::HostShare, false),
        )),
        Err(crate::MountRequestError::HostShareUnavailable)
    ));
    let serial = helios_hal::fs::BlockSerial::new(b"absent").expect("serial fits");
    assert!(matches!(
        block_on(crate::request_mount(
            &authority,
            &namespace,
            nothing_attached(),
            request(crate::MountSource::Disk { serial }, true),
        )),
        Err(crate::MountRequestError::DiskUnavailable { serial: refused }) if refused == serial
    ));

    block_on(crate::request_unmount(&authority, &namespace, "/mnt")).expect("unmounts");
    assert_eq!(owner(&namespace, "/mnt/bin/tool").0, "/");
}

/// A mount is private or shared from the request that made it until it is
/// gone, through every resolution, a held one included; the root of a
/// namespace is shared.
#[test]
fn a_mount_keeps_the_sharing_it_was_granted() {
    let namespace = namespace();
    let authority = ProcessAuthority::root();
    for sharing in [MountSharing::Shared, MountSharing::Private] {
        block_on(crate::request_mount(
            &authority,
            &namespace,
            nothing_attached(),
            crate::MountRequest {
                sharing,
                ..request(crate::MountSource::Memory, false)
            },
        ))
        .expect("root may mount memory");
        let resolved = namespace
            .resolve("/mnt/中文/日本語/한국어")
            .expect("resolves");
        let granted = MountGrant {
            rights: FileSystemRights::READ | FileSystemRights::WRITE,
            sharing,
        };
        assert_eq!(resolved.grant(), granted);
        let held = resolved.held();
        drop(resolved);
        assert_eq!(held.resolved().grant(), granted);
        drop(held);
        block_on(crate::request_unmount(&authority, &namespace, "/mnt")).expect("unmounts");
    }
    assert_eq!(
        namespace.resolve("/mnt").expect("resolves").grant(),
        super::ROOT_GRANT
    );
    assert_eq!(super::ROOT_GRANT.sharing, MountSharing::Shared);
}

/// End-to-end check of private and shared mounts over stored Unix metadata:
/// a private mount exposes stored owner, mode, set-id bits, device nodes and
/// file capabilities; a shared mount exposes the container's configured owner,
/// mount-derived modes, `nosuid`, `nodev` and ignored file capabilities.
#[test]
fn private_and_shared_mounts_distinguish_container_views_and_capabilities() {
    use bytes::Bytes;
    use helios_btrfs::DeviceNumber;
    use helios_btrfs::write::Special;
    use helios_hal::fs::FsPath;

    use super::{MountFlags, MountedFs, SharedOwner, UnixAttributes, XattrSetMode};

    const CAPABILITY: &[u8] = &[
        0x01, 0x00, 0x00, 0x02, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const CONTAINER: SharedOwner = SharedOwner {
        uid: 1000,
        gid: 1000,
    };

    let namespace = namespace();
    let tree = MemoryFs::new(namespace.table().allocate_device());
    let root_owned = |permissions| UnixAttributes {
        permissions,
        uid: 0,
        gid: 0,
    };
    tree.create_file_with(
        FsPath::new("bin-suid").unwrap(),
        root_owned(0o4755),
        1_000_000_000,
    )
    .unwrap();
    tree.set_xattr(
        FsPath::new("bin-suid").unwrap(),
        "security.capability",
        Bytes::from_static(CAPABILITY),
        XattrSetMode::Create,
        1_000_000_000,
    )
    .unwrap();
    tree.create_directory_with(
        FsPath::new("dir-sgid").unwrap(),
        UnixAttributes {
            permissions: 0o3770,
            uid: 42,
            gid: 84,
        },
        1_000_000_000,
    )
    .unwrap();
    tree.create_special(
        FsPath::new("dev-null").unwrap(),
        Special::CharDevice(DeviceNumber::new(1, 3).unwrap()),
        root_owned(0o666),
        1_000_000_000,
    )
    .unwrap();
    tree.symlink_with(
        "bin-suid",
        FsPath::new("link").unwrap(),
        root_owned(0o777),
        1_000_000_000,
    )
    .unwrap();

    // Mounted private: the container sees the stored owner, mode and file
    // capability, and neither `nosuid` nor `nodev` is set.
    block_on(namespace.mount(
        &cap("/mnt"),
        MountedFs::Memory(tree.clone()),
        MountGrant {
            rights: FileSystemRights::READ | FileSystemRights::WRITE,
            sharing: MountSharing::Private,
        },
    ))
    .expect("private mount");
    {
        let resolved = namespace.resolve("/mnt/bin-suid").expect("resolves");
        let grant = resolved.grant();
        let stored = tree.unix_metadata(resolved.path()).unwrap();
        let cap_bytes = tree.xattr(resolved.path(), "security.capability").unwrap();
        assert_eq!(
            grant.container_view(stored.kind, stored.attributes, CONTAINER),
            root_owned(0o4755)
        );
        assert!(grant.flags().honours_exec_privileges());
        assert!(grant.flags().opens_devices());
        assert_eq!(
            grant.file_capability(cap_bytes.as_deref()),
            Some(CAPABILITY)
        );
    }
    block_on(namespace.unmount(&cap("/mnt"))).expect("unmounts");

    // Mounted shared (read-write and read-only): every node appears owned by
    // `CONTAINER`, modes follow the mount's rights with set-id and sticky
    // cleared, `nosuid` and `nodev` are set, and file capabilities are ignored.
    for (rights, file_mode, device_mode) in [
        (
            FileSystemRights::READ | FileSystemRights::WRITE,
            0o777,
            0o666,
        ),
        (FileSystemRights::READ, 0o555, 0o444),
    ] {
        block_on(namespace.mount(
            &cap("/mnt"),
            MountedFs::Memory(tree.clone()),
            MountGrant {
                rights,
                sharing: MountSharing::Shared,
            },
        ))
        .expect("shared mount");
        for (name, expected_mode) in [
            ("bin-suid", file_mode),
            ("dir-sgid", file_mode),
            ("dev-null", device_mode),
            ("link", 0o777),
        ] {
            let full = alloc::format!("/mnt/{name}");
            let resolved = namespace
                .resolve_entry(&full, Follow::No)
                .expect("resolves");
            let grant = resolved.grant();
            let stored = tree.unix_metadata(resolved.path()).unwrap();
            assert_eq!(
                grant.container_view(stored.kind, stored.attributes, CONTAINER),
                UnixAttributes {
                    permissions: expected_mode,
                    uid: CONTAINER.uid,
                    gid: CONTAINER.gid,
                },
                "{name} {rights:?}"
            );
            assert!(
                grant
                    .flags()
                    .contains(MountFlags::NO_SUID | MountFlags::NO_DEV)
            );
            assert!(!grant.flags().honours_exec_privileges());
            assert!(!grant.flags().opens_devices());
        }
        let resolved = namespace.resolve("/mnt/bin-suid").expect("resolves");
        let cap_bytes = tree.xattr(resolved.path(), "security.capability").unwrap();
        assert_eq!(cap_bytes.as_deref(), Some(CAPABILITY));
        assert_eq!(resolved.grant().file_capability(cap_bytes.as_deref()), None);
        block_on(namespace.unmount(&cap("/mnt"))).expect("unmounts");
    }
}

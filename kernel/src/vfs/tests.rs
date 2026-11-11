use alloc::string::String;
use alloc::vec::Vec;

use futures_lite::future::block_on;
use helios_hal::fs::{FileSystem, FileSystemRights, FsError, FsPath, OpenFlags};

use super::{
    BOOT_DEVICE, Follow, MAX_MOUNTS, MemoryFs, MountError, MountTable, MountedFs, Namespace,
};
use crate::{
    BootFs, ComponentFsPathError, EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs,
    MountCap, ProcessAuthority, UnsupportedHostFileSystem, resolve_child_path,
};

type TestNamespace = Namespace<UnsupportedHostFileSystem>;

const IMAGE: EmbeddedBootFs = EmbeddedBootFs::new(
    &[EmbeddedBootDirectory::new("mnt", 0)],
    &[EmbeddedBootFile::new("bin/tool", b"tool", 42)],
);

const READ_WRITE: FileSystemRights = FileSystemRights::READ.union(FileSystemRights::WRITE);

fn namespace() -> TestNamespace {
    Namespace::new(MountTable::new(BootFs::new(IMAGE, BOOT_DEVICE)))
}

fn memory(namespace: &TestNamespace) -> MountedFs<UnsupportedHostFileSystem> {
    MountedFs::Memory(MemoryFs::new(namespace.table().allocate_device()))
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
    block_on(fs.write_at(resolved.path(), 0, bytes, 1)).expect("writes");
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
    block_on(
        resolve_child_path("/", "mnt")
            .map(|mnt| namespace.resolve(&mnt).expect("resolves"))
            .expect("normalizes")
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
        block_on(namespace.rename("/mnt/file", "/file", 2)),
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
    block_on(namespace.rename("/mnt/file", "/mnt/moved", 2)).expect("renames");
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
fn unmount_refuses_a_mount_with_mounts_below_it() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    let resolved = namespace.resolve("/mnt").expect("resolves");
    block_on(
        resolved
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
        FileSystemRights::READ,
    ))
    .expect("mounts");
    assert_eq!(read(&namespace, "/mnt/bin/tool"), b"tool");
    let resolved = namespace.resolve("/mnt/bin/tool").expect("resolves");
    assert_eq!(resolved.writable().map(drop), Err(FsError::ReadOnly));
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
    let root = MountedFs::Root(namespace.root.clone());
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

#[test]
fn namespaces_share_mounts_but_not_root_layers() {
    let first = namespace();
    let second = Namespace::new(first.table().clone());
    let child = Namespace::sharing(first.table().clone(), first.root_layer());
    mount_memory(&first, "/mnt");
    create(&first, "/mnt/shared", b"everyone");
    create(&first, "/private", b"one tree");

    assert_eq!(read(&second, "/mnt/shared"), b"everyone");
    assert_eq!(
        block_on(second.stat("/private", Follow::No)).map(drop),
        Err(FsError::NoEntry)
    );
    assert_eq!(read(&child, "/private"), b"one tree");
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

    assert_eq!(
        block_on(crate::request_mount(
            &authority,
            &namespace,
            None,
            crate::MountSource::Memory,
            "/mnt",
            false,
        )),
        Err(crate::MountRequestError::Authority(
            crate::ProcessAuthorityError::MountGrantExceedsAuthority(
                crate::MountAuthorityRights::MOUNT
            )
        ))
    );
    assert_eq!(owner(&namespace, "/mnt/x").0, "/");
}

#[test]
fn a_mount_request_with_mount_authority_attaches_the_source() {
    let namespace = namespace();
    let authority = ProcessAuthority::root();

    block_on(crate::request_mount(
        &authority,
        &namespace,
        None,
        crate::MountSource::BootImage,
        "/mnt",
        false,
    ))
    .expect("root may mount the boot image");
    assert_eq!(read(&namespace, "/mnt/bin/tool"), b"tool");
    let resolved = namespace.resolve("/mnt/bin/tool").expect("resolves");
    assert_eq!(resolved.writable().map(drop), Err(FsError::ReadOnly));

    assert_eq!(
        block_on(crate::request_mount(
            &authority,
            &namespace,
            None,
            crate::MountSource::HostShare,
            "/mnt",
            false,
        )),
        Err(crate::MountRequestError::HostShareUnavailable)
    );

    block_on(crate::request_unmount(&authority, &namespace, "/mnt")).expect("unmounts");
    assert_eq!(owner(&namespace, "/mnt/bin/tool").0, "/");
}

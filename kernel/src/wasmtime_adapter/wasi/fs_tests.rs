//! The WASI filesystem operations, run against a namespace over a test
//! boot image.

use alloc::string::String;

use futures_lite::future::block_on;
use helios_hal::fs::{FileSystemRights, NodeKind, WritePosition};

use super::{
    FsDescriptor, FsNodeKind, Granted, Kind, RegularFile, Right, Write, create_directory_at,
    fs_types, link_at, metadata_hash, open_at, read, read_program_file, readlink_at,
    remove_directory_at, rename_at, set_size, set_times, set_times_at, stat_at, stat_path,
    symlink_at, unlink_file_at, write_program_file,
};
use crate::wasmtime_adapter::component_host::ProgramSource;
use crate::wasmtime_adapter::cwasm::{self, UntrustedCwasm};
use crate::{
    AuthorityDomain, BOOT_DEVICE, BootFs, EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs,
    FileContents, Follow, HostShare, MemoryFs, MountTable, MountedFs, Namespace,
    UnsupportedHostFileSystem, object_identity,
};

type TestNamespace = Namespace<UnsupportedHostFileSystem>;

const IMAGE: EmbeddedBootFs = EmbeddedBootFs::new(
    &[
        EmbeddedBootDirectory::new("bin/empty", 43),
        EmbeddedBootDirectory::new("mnt", 0),
    ],
    &[EmbeddedBootFile::new("bin/tool", b"tool", 42)],
);

fn namespace() -> TestNamespace {
    Namespace::new(MountTable::new(BootFs::new(IMAGE, BOOT_DEVICE), 1))
}

fn root() -> FsDescriptor {
    FsDescriptor {
        path: String::from("/"),
        kind: FsNodeKind::Directory,
        flags: fs_types::DescriptorFlags::READ
            | fs_types::DescriptorFlags::WRITE
            | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
        identity: None,
        mount: None,
    }
}

fn readonly_root() -> FsDescriptor {
    FsDescriptor {
        flags: fs_types::DescriptorFlags::READ,
        ..root()
    }
}

/// The proof of a right the test's descriptor is known to hold.
fn held<K: Kind, R: Right>(descriptor: &FsDescriptor) -> Granted<'_, K, R> {
    descriptor
        .grant()
        .expect("the descriptor holds the right the operation demands")
}

fn open(
    namespace: &TestNamespace,
    base: &FsDescriptor,
    path: &str,
    open_flags: fs_types::OpenFlags,
    flags: fs_types::DescriptorFlags,
) -> Result<FsDescriptor, fs_types::ErrorCode> {
    block_on(open_at(
        namespace,
        base,
        fs_types::PathFlags::empty(),
        path,
        open_flags,
        flags,
        1,
    ))
}

fn create_file(namespace: &TestNamespace, path: &str, bytes: &[u8]) -> FsDescriptor {
    let descriptor = open(
        namespace,
        &root(),
        path,
        fs_types::OpenFlags::CREATE,
        fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
    )
    .expect("file creation must succeed");
    block_on(super::write(
        namespace,
        held(&descriptor),
        WritePosition::At(0),
        bytes,
        1,
    ))
    .expect("file write must succeed");
    descriptor
}

fn read_all(namespace: &TestNamespace, descriptor: &FsDescriptor) -> alloc::vec::Vec<u8> {
    block_on(read(namespace, held(descriptor), 0, 64))
        .expect("read must succeed")
        .to_vec()
}

fn mount_memory(namespace: &TestNamespace, point: &str) {
    let fs = MountedFs::Memory(MemoryFs::new(namespace.table().allocate_device()));
    let cap = crate::ProcessAuthority::root()
        .derive_mount_cap(point)
        .expect("root may mount anywhere");
    block_on(namespace.mount(&cap, fs, FileSystemRights::READ | FileSystemRights::WRITE))
        .expect("mount must succeed");
}

#[test]
fn create_directory_adds_node() {
    let namespace = namespace();
    block_on(create_directory_at(&namespace, held(&root()), "tmp", 7))
        .expect("directory creation must succeed");

    let metadata =
        block_on(stat_path(&namespace, "/tmp", Follow::No)).expect("directory must exist");
    assert_eq!(metadata.kind, NodeKind::Directory);
    assert_eq!(metadata.modified_nanos, 7);
}

#[test]
fn create_directory_rejects_existing_path() {
    let namespace = namespace();
    block_on(create_directory_at(&namespace, held(&root()), "tmp", 1))
        .expect("initial directory creation must succeed");

    let error = block_on(create_directory_at(&namespace, held(&root()), "tmp", 2))
        .expect_err("creating the same directory twice must fail");
    assert!(matches!(error, fs_types::ErrorCode::Exist));
}

#[test]
fn boot_image_programs_are_visible() {
    let namespace = namespace();

    let program = block_on(stat_path(&namespace, "/bin/tool", Follow::No)).expect("program exists");
    assert_eq!(program.kind, NodeKind::File);
    assert_eq!(program.modified_nanos, 42);

    let directory = block_on(stat_path(&namespace, "/bin", Follow::No)).expect("directory exists");
    assert_eq!(directory.kind, NodeKind::Directory);

    let empty = block_on(stat_path(&namespace, "/bin/empty", Follow::No)).expect("empty exists");
    assert_eq!(empty.kind, NodeKind::Directory);
    assert_eq!(empty.modified_nanos, 43);
}

#[test]
fn writing_a_boot_image_file_writes_the_trees_own_copy() {
    let namespace = namespace();
    let other_tree = namespace.descend();
    let descriptor = open(
        &namespace,
        &root(),
        "bin/tool",
        fs_types::OpenFlags::empty(),
        fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
    )
    .expect("a boot image file opens for writing");

    let end = block_on(super::write(
        &namespace,
        held(&descriptor),
        WritePosition::At(0),
        b"T",
        1,
    ))
    .expect("the write lands in the tree's copy");
    assert_eq!(end, 1);
    assert_eq!(read_all(&namespace, &descriptor), b"Tool");
    assert_eq!(read_all(&other_tree, &descriptor), b"tool");
}

#[test]
fn open_at_rejects_descriptor_right_widening() {
    let namespace = namespace();
    let error = open(
        &namespace,
        &readonly_root(),
        "bin/tool",
        fs_types::OpenFlags::empty(),
        fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
    )
    .err()
    .expect("open_at must not grant write through a read-only base descriptor");
    assert!(matches!(error, fs_types::ErrorCode::ReadOnly));
}

#[test]
fn opening_existing_directory_without_directory_flag_fails() {
    let namespace = namespace();
    block_on(create_directory_at(&namespace, held(&root()), "tmp", 1))
        .expect("directory creation must succeed");

    let error = open(
        &namespace,
        &root(),
        "tmp",
        fs_types::OpenFlags::CREATE,
        fs_types::DescriptorFlags::WRITE,
    )
    .err()
    .expect("opening an existing directory as a file must fail");
    assert!(matches!(error, fs_types::ErrorCode::IsDirectory));
}

/// `st_dev`/`st_ino` come from object identity: every object has a nonzero
/// inode, distinct from its siblings', on its device.
#[test]
fn boot_image_objects_report_nonzero_distinct_inodes() {
    let namespace = namespace();
    let directory = object_identity(
        &block_on(stat_path(&namespace, "/bin", Follow::No)).expect("directory stat"),
    );
    let program = object_identity(
        &block_on(stat_path(&namespace, "/bin/tool", Follow::No)).expect("program stat"),
    );

    assert_eq!(program.domain(), AuthorityDomain::GUEST_BOOTFS);
    assert_eq!(directory.domain(), program.domain());
    assert_ne!(directory.local(), 0);
    assert_ne!(program.local(), 0);
    assert_ne!(directory.local(), program.local());
}

#[test]
fn descriptors_carry_stable_object_identity() {
    let namespace = namespace();
    let open_tool = || {
        open(
            &namespace,
            &root(),
            "bin/tool",
            fs_types::OpenFlags::empty(),
            fs_types::DescriptorFlags::READ,
        )
        .expect("open must succeed")
    };

    assert_eq!(open_tool().identity, open_tool().identity);
}

#[test]
fn rename_preserves_object_identity_local_part() {
    let namespace = namespace();
    create_file(&namespace, "tmp", b"data");
    let before = metadata_hash(&block_on(stat_path(&namespace, "/tmp", Follow::No)).expect("stat"));

    block_on(rename_at(
        &namespace,
        held(&root()),
        "tmp",
        held(&root()),
        "renamed",
        2,
    ))
    .expect("rename must succeed");
    let after =
        metadata_hash(&block_on(stat_path(&namespace, "/renamed", Follow::No)).expect("stat"));

    assert_eq!(before.lower, after.lower);
}

#[test]
fn rename_across_mounts_is_cross_device() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    create_file(&namespace, "tmp", b"data");

    let error = block_on(rename_at(
        &namespace,
        held(&root()),
        "tmp",
        held(&root()),
        "mnt/tmp",
        2,
    ))
    .expect_err("rename across mounts must fail");

    assert!(matches!(error, fs_types::ErrorCode::CrossDevice));
}

#[test]
fn rename_into_a_missing_directory_fails() {
    let namespace = namespace();
    let stale = FsDescriptor {
        path: String::from("/stale"),
        ..root()
    };
    create_file(&namespace, "tmp", b"data");

    let error = block_on(rename_at(
        &namespace,
        held(&root()),
        "tmp",
        held(&stale),
        "tmp",
        2,
    ))
    .expect_err("rename into a directory that does not exist must fail");

    assert!(matches!(error, fs_types::ErrorCode::NoEntry));
}

#[test]
fn hardlink_preserves_identity_and_shared_contents() {
    let namespace = namespace();
    let original = create_file(&namespace, "tmp", b"data");
    block_on(link_at(
        &namespace,
        &root(),
        "tmp",
        held(&root()),
        "alias",
        2,
    ))
    .expect("hardlink must succeed");
    let alias = open(
        &namespace,
        &root(),
        "alias",
        fs_types::OpenFlags::empty(),
        fs_types::DescriptorFlags::READ,
    )
    .expect("alias open must succeed");

    assert_eq!(original.identity, alias.identity);
    assert!(!alias.flags.contains(fs_types::DescriptorFlags::WRITE));
    assert_eq!(
        block_on(stat_path(&namespace, "/tmp", Follow::No))
            .expect("stat must succeed")
            .link_count,
        2
    );

    block_on(super::write(
        &namespace,
        held(&original),
        WritePosition::At(0),
        b"bolt",
        3,
    ))
    .expect("linked write must succeed");
    assert_eq!(read_all(&namespace, &alias), b"bolt");
}

#[test]
fn set_size_resizes_all_links_without_widening_descriptor_rights() {
    let namespace = namespace();
    let original = create_file(&namespace, "tmp", b"data");
    block_on(link_at(
        &namespace,
        &root(),
        "tmp",
        held(&root()),
        "alias",
        3,
    ))
    .expect("hardlink must succeed");
    block_on(set_size(&namespace, held(&original), 6, 4)).expect("set-size must grow the file");

    let alias = open(
        &namespace,
        &root(),
        "alias",
        fs_types::OpenFlags::empty(),
        fs_types::DescriptorFlags::READ,
    )
    .expect("alias open must succeed");
    assert_eq!(read_all(&namespace, &alias), b"data\0\0");
    assert_eq!(
        block_on(stat_path(&namespace, "/tmp", Follow::No))
            .expect("stat must succeed")
            .size,
        6
    );

    let read_only = FsDescriptor {
        flags: fs_types::DescriptorFlags::READ,
        ..original
    };
    let error = read_only
        .grant::<RegularFile, Write>()
        .err()
        .expect("set-size through a read-only descriptor must be refused");
    assert!(matches!(error, fs_types::ErrorCode::ReadOnly));
}

#[test]
fn set_times_updates_stat_and_metadata_hash() {
    let namespace = namespace();
    let file = create_file(&namespace, "tmp", b"");
    let before = metadata_hash(&block_on(stat_path(&namespace, "/tmp", Follow::No)).expect("stat"));

    block_on(set_times(
        &namespace,
        file.grant_touch().expect("a file opened for writing"),
        Some(11),
        Some(17),
        19,
    ))
    .expect("set-times must succeed");
    let metadata = block_on(stat_path(&namespace, "/tmp", Follow::No)).expect("stat must succeed");
    assert_eq!(metadata.access_nanos, 11);
    assert_eq!(metadata.modified_nanos, 17);
    assert_eq!(metadata.status_nanos, 19);
    let after = metadata_hash(&metadata);
    assert_ne!(before.upper, after.upper);
}

#[test]
fn hardlink_rejects_directory_and_cross_mount() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    block_on(create_directory_at(&namespace, held(&root()), "dir", 1))
        .expect("directory creation must succeed");
    create_file(&namespace, "tmp", b"");

    let directory_error = block_on(link_at(
        &namespace,
        &root(),
        "dir",
        held(&root()),
        "dir-link",
        1,
    ))
    .expect_err("hardlinking directories must fail");
    assert!(matches!(directory_error, fs_types::ErrorCode::NotPermitted));

    let file_error = block_on(link_at(
        &namespace,
        &root(),
        "tmp",
        held(&root()),
        "mnt/tmp",
        1,
    ))
    .expect_err("hardlink across mounts must fail");
    assert!(matches!(file_error, fs_types::ErrorCode::CrossDevice));
}

#[test]
fn symlink_target_is_read_without_normalization() {
    let namespace = namespace();
    block_on(symlink_at(
        &namespace,
        held(&root()),
        "bin/tool",
        "tool-link",
        1,
    ))
    .expect("symlink creation must succeed");

    assert_eq!(
        block_on(readlink_at(&namespace, &root(), "tool-link")).expect("readlink must succeed"),
        "bin/tool"
    );
    assert_eq!(
        block_on(stat_path(&namespace, "/tool-link", Follow::No))
            .expect("symlink stat must succeed")
            .kind,
        NodeKind::Symlink
    );
}

#[test]
fn symlink_targets_stay_inside_the_link_directory() {
    assert!(matches!(
        crate::symlink_target("/host/link", "/etc/shadow"),
        Err(helios_hal::fs::FsError::NotPermitted)
    ));
    assert!(matches!(
        crate::symlink_target("/host/link", "share/../../private"),
        Err(helios_hal::fs::FsError::NotPermitted)
    ));
    assert_eq!(
        crate::symlink_target("/host/link", "share/target")
            .expect("a confined relative target resolves"),
        "/host/share/target"
    );
}

#[test]
fn symlink_rejects_rooted_and_parent_escape_targets() {
    let namespace = namespace();

    let absolute = block_on(symlink_at(
        &namespace,
        held(&root()),
        "/bin/tool",
        "absolute",
        1,
    ))
    .expect_err("an absolute symlink target must be rejected");
    assert!(matches!(absolute, fs_types::ErrorCode::NotPermitted));

    let parent = block_on(symlink_at(
        &namespace,
        held(&root()),
        "../bin/tool",
        "escape",
        1,
    ))
    .expect_err("a parent-escaping symlink target must be rejected");
    assert!(matches!(parent, fs_types::ErrorCode::NotPermitted));
}

#[test]
fn symlink_follow_is_confined_and_loop_limited() {
    let namespace = namespace();
    block_on(symlink_at(
        &namespace,
        held(&root()),
        "bin/tool",
        "tool-link",
        1,
    ))
    .expect("symlink creation must succeed");
    let opened = block_on(open_at(
        &namespace,
        &root(),
        fs_types::PathFlags::SYMLINK_FOLLOW,
        "tool-link",
        fs_types::OpenFlags::empty(),
        fs_types::DescriptorFlags::READ,
        1,
    ))
    .expect("symlink follow must open the target");
    assert_eq!(read_all(&namespace, &opened), b"tool");

    block_on(symlink_at(&namespace, held(&root()), "loop-b", "loop-a", 1))
        .expect("loop symlink a must be created");
    block_on(symlink_at(&namespace, held(&root()), "loop-a", "loop-b", 1))
        .expect("loop symlink b must be created");
    let error = block_on(open_at(
        &namespace,
        &root(),
        fs_types::PathFlags::SYMLINK_FOLLOW,
        "loop-a",
        fs_types::OpenFlags::empty(),
        fs_types::DescriptorFlags::READ,
        1,
    ))
    .err()
    .expect("a symlink loop must be rejected");
    assert!(matches!(error, fs_types::ErrorCode::Loop));
}

/// A cwasm signed by a key the kernel does not trust: it parses as an
/// artifact, and only a signature check tells it apart.
fn foreign_signed_cwasm() -> &'static [u8] {
    let signed = helios_artifact::sign_payload_with_secret_key_bytes(b"payload", &[7; 32])
        .expect("signing a test payload must succeed");
    alloc::boxed::Box::leak(signed.into_boxed_slice())
}

/// A namespace whose boot image carries `foreign_signed_cwasm` at
/// `/bin/prog`.
fn namespace_with_bootfs_cwasm() -> TestNamespace {
    let files = alloc::boxed::Box::leak(alloc::boxed::Box::new([EmbeddedBootFile::new(
        "bin/prog",
        foreign_signed_cwasm(),
        42,
    )]));
    const DIRECTORIES: &[EmbeddedBootDirectory] = &[EmbeddedBootDirectory::new("mnt", 0)];
    let image = EmbeddedBootFs::new(DIRECTORIES, files);
    Namespace::new(MountTable::new(BootFs::new(image, BOOT_DEVICE), 1))
}

fn program_source(namespace: &TestNamespace, absolute: &str) -> ProgramSource {
    ProgramSource::from_file(
        block_on(read_program_file(namespace, absolute, "/"))
            .expect("program file must be readable"),
    )
}

fn boot_provenance(namespace: &TestNamespace, absolute: &str) -> bool {
    matches!(
        block_on(read_program_file(namespace, absolute, "/")),
        Ok(FileContents::Boot(_))
    )
}

#[test]
fn bootfs_cwasm_loads_without_signature_verification() {
    let namespace = namespace_with_bootfs_cwasm();

    let ProgramSource::BootfsArtifact(bytes) = program_source(&namespace, "/bin/prog") else {
        panic!("a bootfs cwasm must classify as a bootfs artifact");
    };
    // The signature would not verify, so accepting it proves the bootfs
    // path pays no verification.
    assert!(cwasm::verify_signed_artifact(UntrustedCwasm::new(bytes.as_slice())).is_err());
    let trusted = cwasm::trust_bootfs_artifact(&bytes).expect("a bootfs artifact loads unverified");
    assert_eq!(trusted.payload(), b"payload");
}

#[test]
fn guest_written_cwasm_must_verify() {
    let namespace = namespace_with_bootfs_cwasm();
    block_on(create_directory_at(&namespace, held(&root()), "tmp", 1))
        .expect("creating /tmp must succeed");
    mount_memory(&namespace, "/mnt");

    // The bytes of a bootfs artifact, copied by a guest through an
    // ordinary WASI write, into the root layer and onto a mounted tree:
    // neither copy has bootfs provenance.
    let FileContents::Boot(copied) = block_on(read_program_file(&namespace, "/bin/prog", "/"))
        .expect("bootfs program is readable")
    else {
        panic!("a boot image file must carry boot provenance");
    };
    for copy in ["tmp/copy", "mnt/copy"] {
        create_file(&namespace, copy, copied.as_slice());
        let ProgramSource::SignedArtifact(bytes) =
            program_source(&namespace, &alloc::format!("/{copy}"))
        else {
            panic!("a guest-written cwasm at /{copy} must classify as a signed artifact");
        };
        assert!(matches!(
            cwasm::verify_signed_artifact(UntrustedCwasm::new(&bytes)),
            Err(cwasm::ArtifactTrustError::Trailer(
                helios_artifact::TrailerError::UntrustedSigner
            ))
        ));
    }

    // A kernel-signed artifact with one signature byte flipped, as
    // `write_program_file` would store it, is refused too.
    let mut tampered = cwasm::sign_trusted_artifact_payload(b"payload")
        .expect("signing with the trusted root must succeed");
    let last_signature_byte = tampered.len() - 13;
    tampered[last_signature_byte] ^= 1;
    block_on(write_program_file(
        &namespace,
        "/tmp/tampered",
        &tampered,
        4,
    ))
    .expect("writing a program file must succeed");
    let ProgramSource::SignedArtifact(bytes) = program_source(&namespace, "/tmp/tampered") else {
        panic!("a guest-written cwasm must classify as a signed artifact");
    };
    assert!(matches!(
        cwasm::verify_signed_artifact(UntrustedCwasm::new(&bytes)),
        Err(cwasm::ArtifactTrustError::Trailer(
            helios_artifact::TrailerError::InvalidPayloadSignature
        ))
    ));

    // What the in-kernel compiler writes is signed by the trusted root,
    // and that is what lets a guest exec it from anywhere.
    let signed = cwasm::sign_trusted_artifact_payload(b"payload")
        .expect("signing with the trusted root must succeed");
    block_on(write_program_file(&namespace, "/tmp/compiled", &signed, 5))
        .expect("writing a program file must succeed");
    let ProgramSource::SignedArtifact(bytes) = program_source(&namespace, "/tmp/compiled") else {
        panic!("a guest-written cwasm must classify as a signed artifact");
    };
    let trusted = cwasm::verify_signed_artifact(UntrustedCwasm::new(&bytes))
        .expect("a kernel-signed artifact must verify");
    assert_eq!(trusted.payload(), b"payload");
}

#[test]
fn writing_a_bootfs_node_drops_its_provenance() {
    // The boot image is never written in place: each write path copies
    // the file up first, and the copy's bytes are written bytes.
    let changes: [fn(&TestNamespace, &FsDescriptor) -> super::FsOpResult<()>; 3] = [
        |namespace, descriptor| {
            block_on(super::write(
                namespace,
                held(descriptor),
                WritePosition::At(0),
                b"p",
                2,
            ))
            .map(drop)
        },
        |namespace, descriptor| block_on(set_size(namespace, held(descriptor), 1, 3)),
        |namespace, _| {
            block_on(write_program_file(
                namespace,
                "/bin/prog",
                foreign_signed_cwasm(),
                4,
            ))
        },
    ];
    for change in changes {
        let namespace = namespace_with_bootfs_cwasm();
        let untouched = namespace.descend();
        assert!(boot_provenance(&namespace, "/bin/prog"));
        let descriptor = open(
            &namespace,
            &root(),
            "bin/prog",
            fs_types::OpenFlags::empty(),
            fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
        )
        .expect("opening the node for writing must succeed");
        change(&namespace, &descriptor).expect("the change lands in a copy");
        assert!(!boot_provenance(&namespace, "/bin/prog"));
        assert!(boot_provenance(&untouched, "/bin/prog"));
    }
}

/// A preopen of `/work`, with every right a directory grant carries.
fn work() -> FsDescriptor {
    FsDescriptor {
        path: String::from("/work"),
        ..root()
    }
}

#[test]
fn the_base_itself_is_never_removed_renamed_or_replaced_through_it() {
    let namespace = namespace();
    block_on(create_directory_at(&namespace, held(&root()), "work", 1)).expect("creates /work");
    block_on(create_directory_at(&namespace, held(&root()), "bob", 1)).expect("creates /bob");

    for path in ["", ".", "sub/.."] {
        assert!(matches!(
            block_on(remove_directory_at(&namespace, held(&work()), path)),
            Err(fs_types::ErrorCode::Invalid)
        ));
        assert!(matches!(
            block_on(unlink_file_at(&namespace, held(&work()), path)),
            Err(fs_types::ErrorCode::Invalid)
        ));
        assert!(matches!(
            block_on(symlink_at(&namespace, held(&work()), "bob", path, 1)),
            Err(fs_types::ErrorCode::Invalid)
        ));
        assert!(matches!(
            block_on(rename_at(
                &namespace,
                held(&work()),
                path,
                held(&root()),
                "moved",
                1
            )),
            Err(fs_types::ErrorCode::Invalid)
        ));
        assert!(matches!(
            block_on(rename_at(
                &namespace,
                held(&root()),
                "bob",
                held(&work()),
                path,
                1
            )),
            Err(fs_types::ErrorCode::Invalid)
        ));
    }
    let work = block_on(stat_path(&namespace, "/work", Follow::No)).expect("/work stays");
    assert_eq!(work.kind, NodeKind::Directory);
}

#[test]
fn a_link_at_the_base_does_not_lead_out_of_it() {
    let namespace = namespace();
    block_on(create_directory_at(&namespace, held(&root()), "bob", 1)).expect("creates /bob");
    create_file(&namespace, "bob/secret", b"bob's");
    // Whoever holds the parent replaces the preopen's directory with a
    // link to its sibling: `work`'s holder still names `/work`.
    block_on(symlink_at(&namespace, held(&root()), "bob", "work", 1)).expect("links /work");

    for path in [".", ""] {
        let opened = block_on(open_at(
            &namespace,
            &work(),
            fs_types::PathFlags::SYMLINK_FOLLOW,
            path,
            fs_types::OpenFlags::DIRECTORY,
            fs_types::DescriptorFlags::READ,
            1,
        ));
        assert!(matches!(opened, Err(fs_types::ErrorCode::NotPermitted)));
        assert!(matches!(
            block_on(stat_at(
                &namespace,
                &work(),
                fs_types::PathFlags::SYMLINK_FOLLOW,
                path
            )),
            Err(fs_types::ErrorCode::NotPermitted)
        ));
    }
    // A load granted at `/work` reads nothing through the link either.
    assert!(matches!(
        block_on(read_program_file(&namespace, "/work", "/work")),
        Err(fs_types::ErrorCode::NotPermitted)
    ));
    // The parent, whose floor holds both, still follows it.
    let followed = block_on(open_at(
        &namespace,
        &root(),
        fs_types::PathFlags::SYMLINK_FOLLOW,
        "work",
        fs_types::OpenFlags::DIRECTORY,
        fs_types::DescriptorFlags::READ,
        1,
    ))
    .expect("the root's descriptor follows a link inside the root");
    assert_eq!(followed.path, "/bob");
}

#[test]
fn a_program_load_floor_is_the_widest_grant_that_loads() {
    let mut authority = crate::ProcessAuthority::empty();
    for (source, guest, rights) in [
        ("/work", "/work", crate::DirectoryAuthorityRights::READ),
        (
            "/work/bin",
            "/bin",
            crate::DirectoryAuthorityRights::EXECUTE,
        ),
    ] {
        authority.insert_directory_preopen(
            crate::DirectoryPreopen::new(source, guest, rights).expect("a valid grant"),
        );
    }
    assert_eq!(
        authority.program_load_floor("/work/bin/tool"),
        Some("/work")
    );
    assert_eq!(authority.program_load_floor("/work"), Some("/work"));
    assert_eq!(authority.program_load_floor("/workshop/tool"), None);
}

#[test]
fn an_append_resolves_with_the_end_of_file_it_reached() {
    let namespace = namespace();
    let appender = create_file(&namespace, "log", b"abc");
    // Another writer grows the file behind the appender's back.
    let other = open(
        &namespace,
        &root(),
        "log",
        fs_types::OpenFlags::empty(),
        fs_types::DescriptorFlags::WRITE,
    )
    .expect("a second writer opens the file");
    let end = block_on(super::write(
        &namespace,
        held(&other),
        WritePosition::At(3),
        "日本語".as_bytes(),
        2,
    ))
    .expect("the second writer extends the file");
    assert_eq!(end, 3 + "日本語".len() as u64);

    let end = block_on(super::write(
        &namespace,
        held(&appender),
        WritePosition::End,
        "한국어".as_bytes(),
        3,
    ))
    .expect("the append lands");
    let contents = read_all(&namespace, &appender);
    assert_eq!(end, contents.len() as u64);
    assert_eq!(contents, "abc日本語한국어".as_bytes());
}

#[test]
fn a_directory_holding_a_mount_point_is_busy() {
    let namespace = namespace();
    for directory in ["a", "a/b"] {
        block_on(create_directory_at(&namespace, held(&root()), directory, 1)).expect("creates");
    }
    mount_memory(&namespace, "/a/b");

    for (source, path) in [("the holder", "a"), ("the mount point", "a/b")] {
        assert!(
            matches!(
                block_on(remove_directory_at(&namespace, held(&root()), path)),
                Err(fs_types::ErrorCode::Busy)
            ),
            "removing {source} must be refused as busy"
        );
        assert!(
            matches!(
                block_on(rename_at(
                    &namespace,
                    held(&root()),
                    path,
                    held(&root()),
                    "z",
                    1
                )),
                Err(fs_types::ErrorCode::Busy)
            ),
            "renaming {source} must be refused as busy"
        );
    }
    let mounted = block_on(stat_path(&namespace, "/a/b", Follow::No)).expect("still mounted");
    assert_eq!(mounted.kind, NodeKind::Directory);
}

#[test]
fn a_mount_with_a_descriptor_or_stream_open_on_it_is_busy() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    let descriptor = create_file(&namespace, "mnt/notes", "日本語 한국어".as_bytes());
    let stream =
        super::read_stream_target(&namespace, held(&descriptor)).expect("a stream over the file");

    let unmount = || block_on(namespace.table().unmount("/mnt"));
    assert_eq!(unmount(), Err(crate::MountError::Busy("/mnt".into())));
    drop(descriptor);
    assert_eq!(unmount(), Err(crate::MountError::Busy("/mnt".into())));
    assert_eq!(
        block_on(super::read_held(stream.clone(), 0, 64))
            .expect("the stream still reads")
            .as_ref(),
        "日本語 한국어".as_bytes()
    );
    drop(stream);
    unmount().expect("nothing holds the mount any more");
}

/// Mounts a host share at `point` whose every request fails with
/// `unsupported`, read-write. A refusal with any other code therefore proves
/// that no request reached the host.
fn mount_unreachable_host(namespace: &TestNamespace, point: &str) {
    let fs = MountedFs::Host(HostShare::new(UnsupportedHostFileSystem));
    let cap = crate::ProcessAuthority::root()
        .derive_mount_cap(point)
        .expect("root may mount anywhere");
    block_on(namespace.mount(&cap, fs, FileSystemRights::READ | FileSystemRights::WRITE))
        .expect("mount must succeed");
}

/// A directory descriptor at `path` that holds exactly `flags`.
fn directory_holding(path: &str, flags: fs_types::DescriptorFlags) -> FsDescriptor {
    FsDescriptor {
        path: String::from(path),
        flags,
        ..root()
    }
}

/// A grant of the share's directory that holds every right, and one that
/// only reads.
fn host_bases() -> (FsDescriptor, FsDescriptor) {
    let rights = root().flags;
    (
        directory_holding("/mnt/a", rights),
        directory_holding("/mnt/a", fs_types::DescriptorFlags::READ),
    )
}

#[test]
fn a_read_only_base_creates_no_directory_on_the_host_share() {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    let (rw, ro) = host_bases();

    // The control: a request the base may make reaches the share.
    let reached = block_on(async { create_directory_at(&namespace, rw.grant()?, "new", 1).await });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));

    let refused = block_on(async { create_directory_at(&namespace, ro.grant()?, "new", 1).await });
    assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
}

#[test]
fn a_read_only_base_removes_no_directory_on_the_host_share() {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    let (rw, ro) = host_bases();

    let reached = block_on(async { remove_directory_at(&namespace, rw.grant()?, "old").await });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));

    let refused = block_on(async { remove_directory_at(&namespace, ro.grant()?, "old").await });
    assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
}

#[test]
fn a_read_only_base_renames_nothing_on_the_host_share() {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    let (rw, ro) = host_bases();

    let reached =
        block_on(async { rename_at(&namespace, rw.grant()?, "old", rw.grant()?, "new", 1).await });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));

    let from_read_only =
        block_on(async { rename_at(&namespace, ro.grant()?, "old", rw.grant()?, "new", 1).await });
    assert!(matches!(from_read_only, Err(fs_types::ErrorCode::ReadOnly)));
    let into_read_only =
        block_on(async { rename_at(&namespace, rw.grant()?, "old", ro.grant()?, "new", 1).await });
    assert!(matches!(into_read_only, Err(fs_types::ErrorCode::ReadOnly)));
}

#[test]
fn a_read_only_base_unlinks_nothing_on_the_host_share() {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    let (rw, ro) = host_bases();

    let reached = block_on(async { unlink_file_at(&namespace, rw.grant()?, "old").await });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));

    let refused = block_on(async { unlink_file_at(&namespace, ro.grant()?, "old").await });
    assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
}

/// The times `/tmp` shows, as `(access, modified)`.
fn times_of(namespace: &TestNamespace, path: &str) -> (u64, u64) {
    let metadata = block_on(stat_path(namespace, path, Follow::No)).expect("stat must succeed");
    (metadata.access_nanos, metadata.modified_nanos)
}

#[test]
fn set_times_needs_write_on_the_descriptor() {
    let namespace = namespace();
    let file = create_file(&namespace, "tmp", b"");
    let before = times_of(&namespace, "/tmp");
    let read_only = FsDescriptor {
        flags: fs_types::DescriptorFlags::READ,
        ..file.clone()
    };

    let refused = read_only.grant_touch().err();
    assert!(matches!(refused, Some(fs_types::ErrorCode::ReadOnly)));
    assert_eq!(times_of(&namespace, "/tmp"), before);

    let writer = FsDescriptor {
        flags: fs_types::DescriptorFlags::WRITE,
        ..file
    };
    block_on(set_times(
        &namespace,
        writer
            .grant_touch()
            .expect("a writable file may be retimed"),
        Some(11),
        Some(17),
        19,
    ))
    .expect("a descriptor with write sets the times");
    assert_eq!(times_of(&namespace, "/tmp"), (11, 17));

    // A directory is retimed by the right to change it, not by `write`.
    block_on(create_directory_at(&namespace, held(&root()), "dir", 1)).expect("creates /dir");
    let directory = |flags| directory_holding("/dir", flags);
    assert!(matches!(
        directory(fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE)
            .grant_touch()
            .err(),
        Some(fs_types::ErrorCode::ReadOnly)
    ));
    let mutator = directory(fs_types::DescriptorFlags::MUTATE_DIRECTORY);
    block_on(set_times(
        &namespace,
        mutator.grant_touch().expect("a mutable directory"),
        Some(23),
        Some(29),
        31,
    ))
    .expect("a directory descriptor with mutate-directory sets the times");
    assert_eq!(times_of(&namespace, "/dir"), (23, 29));
}

#[test]
fn set_times_asks_the_host_only_with_the_right() {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    let file = |flags| FsDescriptor {
        path: String::from("/mnt/a/file"),
        kind: FsNodeKind::File,
        flags,
        ..root()
    };

    let reached = block_on(async {
        set_times(
            &namespace,
            file(fs_types::DescriptorFlags::WRITE).grant_touch()?,
            Some(1),
            Some(2),
            3,
        )
        .await
    });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));

    let refused = block_on(async {
        set_times(
            &namespace,
            file(fs_types::DescriptorFlags::READ).grant_touch()?,
            Some(1),
            Some(2),
            3,
        )
        .await
    });
    assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
}

#[test]
fn set_times_at_needs_write_or_mutate_on_the_base() {
    let namespace = namespace();
    create_file(&namespace, "tmp", b"");
    let before = times_of(&namespace, "/tmp");
    let base = |flags| directory_holding("/", flags);

    let refused = block_on(async {
        set_times_at(
            &namespace,
            readonly_root().grant()?,
            "tmp",
            Some(11),
            Some(17),
            19,
        )
        .await
    });
    assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
    assert_eq!(times_of(&namespace, "/tmp"), before);

    for (flags, times) in [
        (fs_types::DescriptorFlags::WRITE, (11, 17)),
        (fs_types::DescriptorFlags::MUTATE_DIRECTORY, (23, 29)),
    ] {
        let base = base(flags);
        block_on(set_times_at(
            &namespace,
            held(&base),
            "tmp",
            Some(times.0),
            Some(times.1),
            31,
        ))
        .expect("a base with write or mutate-directory sets the times");
        assert_eq!(times_of(&namespace, "/tmp"), times);
    }

    // On the host share the refusal also comes before any request.
    mount_unreachable_host(&namespace, "/mnt");
    let (rw, ro) = host_bases();
    let reached = block_on(async {
        set_times_at(&namespace, rw.grant()?, "file", Some(1), Some(2), 3).await
    });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));
    let refused = block_on(async {
        set_times_at(&namespace, ro.grant()?, "file", Some(1), Some(2), 3).await
    });
    assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
}

#[test]
fn a_read_only_base_creates_or_truncates_nothing_before_asking_the_host() {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    let (rw, ro) = host_bases();
    let read = fs_types::DescriptorFlags::READ;

    // The control: a base that may create and truncate reaches the share.
    for open_flags in [fs_types::OpenFlags::CREATE, fs_types::OpenFlags::TRUNCATE] {
        let reached = open(&namespace, &rw, "file", open_flags, read);
        assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));
    }

    // Without the base's rights neither is asked of the host.
    for open_flags in [fs_types::OpenFlags::CREATE, fs_types::OpenFlags::TRUNCATE] {
        let refused = open(&namespace, &ro, "file", open_flags, read);
        assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
    }
}

#[test]
fn a_read_only_base_creates_or_truncates_nothing_in_memory() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    let notes = create_file(&namespace, "mnt/notes", "日本語 한국어".as_bytes());
    let read = fs_types::DescriptorFlags::READ;
    let read_only = directory_holding("/mnt", read);
    let no_write = directory_holding("/mnt", read | fs_types::DescriptorFlags::MUTATE_DIRECTORY);

    // A new name, and an existing one: creating is refused either way.
    for name in ["new", "notes"] {
        let refused = open(
            &namespace,
            &read_only,
            name,
            fs_types::OpenFlags::CREATE,
            read,
        );
        assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
    }
    assert!(matches!(
        block_on(stat_path(&namespace, "/mnt/new", Follow::No)),
        Err(fs_types::ErrorCode::NoEntry)
    ));

    // Truncating needs the base's `write` beside its `mutate-directory`.
    for base in [&read_only, &no_write] {
        let refused = open(
            &namespace,
            base,
            "notes",
            fs_types::OpenFlags::TRUNCATE,
            read,
        );
        assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
    }
    assert_eq!(read_all(&namespace, &notes), "日本語 한국어".as_bytes());
}

/// The two exhaustion errors of the filesystem contract reach the WASI
/// error codes a guest tells apart: the storage budget and the pool.
#[test]
fn storage_exhaustion_maps_to_the_wasi_insufficient_codes() {
    use helios_hal::fs::FsError;

    assert!(matches!(
        super::fs_error_code(FsError::NoSpace),
        fs_types::ErrorCode::InsufficientSpace
    ));
    assert!(matches!(
        super::fs_error_code(FsError::OutOfMemory),
        fs_types::ErrorCode::InsufficientMemory
    ));
}

//! The WASI filesystem operations, run against a namespace over a test
//! boot image.

use alloc::string::String;

use futures_lite::future::block_on;
use helios_hal::fs::{FileSystemRights, NodeKind};

use super::{
    FileWriteMode, FsDescriptor, FsNodeKind, create_directory_at, fs_types, link_at, metadata_hash,
    open_at, read, read_program_file, readlink_at, rename_at, set_size, set_times, stat_path,
    symlink_at, write_program_file,
};
use crate::wasmtime_adapter::component_host::ProgramSource;
use crate::wasmtime_adapter::cwasm::{self, UntrustedCwasm};
use crate::{
    AuthorityDomain, BOOT_DEVICE, BootFs, EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs,
    FileContents, MemoryFs, MountTable, MountedFs, Namespace, UnsupportedHostFileSystem,
    object_identity,
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
    Namespace::new(MountTable::new(BootFs::new(IMAGE, BOOT_DEVICE)))
}

fn root() -> FsDescriptor {
    FsDescriptor {
        path: String::from("/"),
        kind: FsNodeKind::Directory,
        flags: fs_types::DescriptorFlags::READ
            | fs_types::DescriptorFlags::WRITE
            | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
        identity: None,
    }
}

fn readonly_root() -> FsDescriptor {
    FsDescriptor {
        flags: fs_types::DescriptorFlags::READ,
        ..root()
    }
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
        &descriptor,
        FileWriteMode::At(0),
        bytes,
        1,
    ))
    .expect("file write must succeed");
    descriptor
}

fn read_all(namespace: &TestNamespace, descriptor: &FsDescriptor) -> alloc::vec::Vec<u8> {
    block_on(read(namespace, descriptor, 0, 64))
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
    block_on(create_directory_at(&namespace, &root(), "tmp", 7))
        .expect("directory creation must succeed");

    let metadata = block_on(stat_path(&namespace, "/tmp", false)).expect("directory must exist");
    assert_eq!(metadata.kind, NodeKind::Directory);
    assert_eq!(metadata.modified_nanos, 7);
}

#[test]
fn create_directory_rejects_existing_path() {
    let namespace = namespace();
    block_on(create_directory_at(&namespace, &root(), "tmp", 1))
        .expect("initial directory creation must succeed");

    let error = block_on(create_directory_at(&namespace, &root(), "tmp", 2))
        .expect_err("creating the same directory twice must fail");
    assert!(matches!(error, fs_types::ErrorCode::Exist));
}

#[test]
fn boot_image_programs_are_visible() {
    let namespace = namespace();

    let program = block_on(stat_path(&namespace, "/bin/tool", false)).expect("program exists");
    assert_eq!(program.kind, NodeKind::File);
    assert_eq!(program.modified_nanos, 42);

    let directory = block_on(stat_path(&namespace, "/bin", false)).expect("directory exists");
    assert_eq!(directory.kind, NodeKind::Directory);

    let empty = block_on(stat_path(&namespace, "/bin/empty", false)).expect("empty exists");
    assert_eq!(empty.kind, NodeKind::Directory);
    assert_eq!(empty.modified_nanos, 43);
}

#[test]
fn boot_image_file_rejects_writes() {
    let namespace = namespace();
    let descriptor = open(
        &namespace,
        &root(),
        "bin/tool",
        fs_types::OpenFlags::empty(),
        fs_types::DescriptorFlags::WRITE,
    )
    .expect("a boot image file opens for writing; the write is what fails");

    let error = block_on(super::write(
        &namespace,
        &descriptor,
        FileWriteMode::At(0),
        b"x",
        1,
    ))
    .expect_err("a boot image file must reject writes");
    assert!(matches!(error, fs_types::ErrorCode::ReadOnly));
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
    block_on(create_directory_at(&namespace, &root(), "tmp", 1))
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
    let directory =
        object_identity(&block_on(stat_path(&namespace, "/bin", false)).expect("directory stat"));
    let program = object_identity(
        &block_on(stat_path(&namespace, "/bin/tool", false)).expect("program stat"),
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
    let before = metadata_hash(&block_on(stat_path(&namespace, "/tmp", false)).expect("stat"));

    block_on(rename_at(&namespace, &root(), "tmp", &root(), "renamed", 2))
        .expect("rename must succeed");
    let after = metadata_hash(&block_on(stat_path(&namespace, "/renamed", false)).expect("stat"));

    assert_eq!(before.lower, after.lower);
}

#[test]
fn rename_across_mounts_is_cross_device() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    create_file(&namespace, "tmp", b"data");

    let error = block_on(rename_at(&namespace, &root(), "tmp", &root(), "mnt/tmp", 2))
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

    let error = block_on(rename_at(&namespace, &root(), "tmp", &stale, "tmp", 2))
        .expect_err("rename into a directory that does not exist must fail");

    assert!(matches!(error, fs_types::ErrorCode::NoEntry));
}

#[test]
fn hardlink_preserves_identity_and_shared_contents() {
    let namespace = namespace();
    let original = create_file(&namespace, "tmp", b"data");
    block_on(link_at(&namespace, &root(), "tmp", &root(), "alias", 2))
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
        block_on(stat_path(&namespace, "/tmp", false))
            .expect("stat must succeed")
            .link_count,
        2
    );

    block_on(super::write(
        &namespace,
        &original,
        FileWriteMode::At(0),
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
    block_on(link_at(&namespace, &root(), "tmp", &root(), "alias", 3))
        .expect("hardlink must succeed");
    block_on(set_size(&namespace, &original, 6, 4)).expect("set-size must grow the file");

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
        block_on(stat_path(&namespace, "/tmp", false))
            .expect("stat must succeed")
            .size,
        6
    );

    let read_only = FsDescriptor {
        flags: fs_types::DescriptorFlags::READ,
        ..original
    };
    let error = block_on(set_size(&namespace, &read_only, 1, 5))
        .expect_err("set-size through a read-only descriptor must fail");
    assert!(matches!(error, fs_types::ErrorCode::ReadOnly));
}

#[test]
fn set_times_updates_stat_and_metadata_hash() {
    let namespace = namespace();
    create_file(&namespace, "tmp", b"");
    let before = metadata_hash(&block_on(stat_path(&namespace, "/tmp", false)).expect("stat"));

    block_on(set_times(&namespace, "/tmp", Some(11), Some(17), 19))
        .expect("set-times must succeed");
    let metadata = block_on(stat_path(&namespace, "/tmp", false)).expect("stat must succeed");
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
    block_on(create_directory_at(&namespace, &root(), "dir", 1))
        .expect("directory creation must succeed");
    create_file(&namespace, "tmp", b"");

    let directory_error = block_on(link_at(&namespace, &root(), "dir", &root(), "dir-link", 1))
        .expect_err("hardlinking directories must fail");
    assert!(matches!(directory_error, fs_types::ErrorCode::NotPermitted));

    let file_error = block_on(link_at(&namespace, &root(), "tmp", &root(), "mnt/tmp", 1))
        .expect_err("hardlink across mounts must fail");
    assert!(matches!(file_error, fs_types::ErrorCode::CrossDevice));
}

#[test]
fn symlink_target_is_read_without_normalization() {
    let namespace = namespace();
    block_on(symlink_at(&namespace, &root(), "bin/tool", "tool-link", 1))
        .expect("symlink creation must succeed");

    assert_eq!(
        block_on(readlink_at(&namespace, &root(), "tool-link")).expect("readlink must succeed"),
        "bin/tool"
    );
    assert_eq!(
        block_on(stat_path(&namespace, "/tool-link", false))
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

    let absolute = block_on(symlink_at(&namespace, &root(), "/bin/tool", "absolute", 1))
        .expect_err("an absolute symlink target must be rejected");
    assert!(matches!(absolute, fs_types::ErrorCode::NotPermitted));

    let parent = block_on(symlink_at(&namespace, &root(), "../bin/tool", "escape", 1))
        .expect_err("a parent-escaping symlink target must be rejected");
    assert!(matches!(parent, fs_types::ErrorCode::NotPermitted));
}

#[test]
fn symlink_follow_is_confined_and_loop_limited() {
    let namespace = namespace();
    block_on(symlink_at(&namespace, &root(), "bin/tool", "tool-link", 1))
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

    block_on(symlink_at(&namespace, &root(), "loop-b", "loop-a", 1))
        .expect("loop symlink a must be created");
    block_on(symlink_at(&namespace, &root(), "loop-a", "loop-b", 1))
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
    Namespace::new(MountTable::new(BootFs::new(image, BOOT_DEVICE)))
}

fn program_source(namespace: &TestNamespace, absolute: &str) -> ProgramSource {
    ProgramSource::from_file(
        block_on(read_program_file(namespace, absolute)).expect("program file must be readable"),
    )
}

fn boot_provenance(namespace: &TestNamespace, absolute: &str) -> bool {
    matches!(
        block_on(read_program_file(namespace, absolute)),
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
    block_on(create_directory_at(&namespace, &root(), "tmp", 1))
        .expect("creating /tmp must succeed");
    mount_memory(&namespace, "/mnt");

    // The bytes of a bootfs artifact, copied by a guest through an
    // ordinary WASI write, into the root layer and onto a mounted tree:
    // neither copy has bootfs provenance.
    let FileContents::Boot(copied) =
        block_on(read_program_file(&namespace, "/bin/prog")).expect("bootfs program is readable")
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
    let namespace = namespace_with_bootfs_cwasm();
    assert!(boot_provenance(&namespace, "/bin/prog"));
    let descriptor = open(
        &namespace,
        &root(),
        "bin/prog",
        fs_types::OpenFlags::empty(),
        fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
    )
    .expect("opening the node for writing must succeed");

    // The boot image is never written in place: every write path either
    // leaves the boot bytes as they were or stores written bytes.
    let write = block_on(super::write(
        &namespace,
        &descriptor,
        FileWriteMode::At(0),
        b"p",
        2,
    ));
    let resize = block_on(set_size(&namespace, &descriptor, 1, 3));
    let replace = block_on(write_program_file(
        &namespace,
        "/bin/prog",
        foreign_signed_cwasm(),
        4,
    ));
    for outcome in [write, resize, replace] {
        match outcome {
            Ok(()) => assert!(!boot_provenance(&namespace, "/bin/prog")),
            Err(error) => {
                assert!(matches!(error, fs_types::ErrorCode::ReadOnly));
                assert!(boot_provenance(&namespace, "/bin/prog"));
            }
        }
    }
}

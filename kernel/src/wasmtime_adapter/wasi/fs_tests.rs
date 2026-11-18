//! The WASI filesystem operations, run against a namespace over a test
//! boot image.

use alloc::string::String;

use futures_lite::future::block_on;
use helios_hal::fs::{FileSystemRights, NodeKind, WritePosition};

use super::{
    FsDescriptor, FsNodeKind, Granted, Kind, RegularFile, Right, Write, create_directory_at,
    fs_types, link_at, metadata_hash, open_at, read, read_directory, read_program_file,
    read_stream_target, readlink_at, remove_directory_at, rename_at, set_size, set_times,
    set_times_at, stat_at, stat_path, symlink_at, unlink_file_at, write_program_file,
    write_stream_target,
};
use crate::test_support::{UnsupportedHostFileSystem, UnsupportedStorage};
use crate::wasmtime_adapter::component_host::ProgramSource;
use crate::wasmtime_adapter::cwasm::{self, UntrustedCwasm};
use crate::{
    AuthorityDomain, BOOT_DEVICE, BootFs, EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs,
    FileContents, Follow, FsDescriptorFlags, HostShare, MemoryFs, MountTable, MountedFs, Namespace,
    object_identity,
};

type TestNamespace = Namespace<UnsupportedStorage>;

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
        flags: FsDescriptorFlags::READ
            | FsDescriptorFlags::WRITE
            | FsDescriptorFlags::MUTATE_DIRECTORY,
        identity: None,
        mount: None,
    }
}

fn readonly_root() -> FsDescriptor {
    FsDescriptor {
        flags: FsDescriptorFlags::READ,
        ..root()
    }
}

/// The proof of a right the test's descriptor is known to hold.
fn held<K: Kind, R: Right>(descriptor: &FsDescriptor) -> Granted<'_, K, R> {
    descriptor
        .grant()
        .expect("the descriptor holds the right the operation demands")
}

fn create_file<S: crate::Storage>(
    namespace: &Namespace<S>,
    path: &str,
    bytes: &[u8],
) -> FsDescriptor {
    let descriptor = open(
        namespace,
        &root(),
        path,
        fs_types::OpenFlags::CREATE,
        FsDescriptorFlags::READ | FsDescriptorFlags::WRITE,
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

fn read_all<S: crate::Storage>(
    namespace: &Namespace<S>,
    descriptor: &FsDescriptor,
) -> alloc::vec::Vec<u8> {
    block_on(read(namespace, held(descriptor), 0, 64))
        .expect("read must succeed")
        .to_vec()
}

fn open<S: crate::Storage>(
    namespace: &Namespace<S>,
    base: &FsDescriptor,
    path: &str,
    open_flags: fs_types::OpenFlags,
    flags: FsDescriptorFlags,
) -> Result<FsDescriptor, fs_types::ErrorCode> {
    block_on(async {
        open_at(
            namespace,
            base.grant()?,
            fs_types::PathFlags::empty(),
            path,
            open_flags,
            flags,
            1,
        )
        .await
    })
}

fn mount_memory(namespace: &TestNamespace, point: &str) {
    let fs = MountedFs::Memory(MemoryFs::new(namespace.table().allocate_device()));
    let cap = crate::ProcessAuthority::root()
        .derive_mount_cap(point)
        .expect("root may mount anywhere");
    block_on(namespace.mount(
        &cap,
        fs,
        granted(FileSystemRights::READ | FileSystemRights::WRITE),
    ))
    .expect("mount must succeed");
}

/// A shared mount granting `rights`.
fn granted(rights: FileSystemRights) -> crate::MountGrant {
    crate::MountGrant {
        rights,
        sharing: crate::MountSharing::Shared,
    }
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
        FsDescriptorFlags::READ | FsDescriptorFlags::WRITE,
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
    assert_eq!(end.end(), Some(1));
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
        FsDescriptorFlags::READ | FsDescriptorFlags::WRITE,
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
        FsDescriptorFlags::WRITE,
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
            FsDescriptorFlags::READ,
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

    rename(&namespace, &root(), "tmp", &root(), "renamed").expect("rename must succeed");
    let after =
        metadata_hash(&block_on(stat_path(&namespace, "/renamed", Follow::No)).expect("stat"));

    assert_eq!(before.lower, after.lower);
}

#[test]
fn rename_across_mounts_is_cross_device() {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    create_file(&namespace, "tmp", b"data");

    let error = rename(&namespace, &root(), "tmp", &root(), "mnt/tmp")
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

    let error = rename(&namespace, &root(), "tmp", &stale, "tmp")
        .expect_err("rename into a directory that does not exist must fail");

    assert!(matches!(error, fs_types::ErrorCode::NoEntry));
}

#[test]
fn hardlink_preserves_identity_and_shared_contents() {
    let namespace = namespace();
    let original = create_file(&namespace, "tmp", b"data");
    link(&namespace, &root(), "tmp", &root(), "alias").expect("hardlink must succeed");
    let alias = open(
        &namespace,
        &root(),
        "alias",
        fs_types::OpenFlags::empty(),
        FsDescriptorFlags::READ,
    )
    .expect("alias open must succeed");

    assert_eq!(original.identity, alias.identity);
    assert!(!alias.flags.contains(FsDescriptorFlags::WRITE));
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
    link(&namespace, &root(), "tmp", &root(), "alias").expect("hardlink must succeed");
    block_on(set_size(&namespace, held(&original), 6, 4)).expect("set-size must grow the file");

    let alias = open(
        &namespace,
        &root(),
        "alias",
        fs_types::OpenFlags::empty(),
        FsDescriptorFlags::READ,
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
        flags: FsDescriptorFlags::READ,
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

    let directory_error = link(&namespace, &root(), "dir", &root(), "dir-link")
        .expect_err("hardlinking directories must fail");
    assert!(matches!(directory_error, fs_types::ErrorCode::NotPermitted));

    let file_error = link(&namespace, &root(), "tmp", &root(), "mnt/tmp")
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
        block_on(readlink_at(&namespace, held(&root()), "tool-link"))
            .expect("readlink must succeed"),
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
        held(&root()),
        fs_types::PathFlags::SYMLINK_FOLLOW,
        "tool-link",
        fs_types::OpenFlags::empty(),
        FsDescriptorFlags::READ,
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
        held(&root()),
        fs_types::PathFlags::SYMLINK_FOLLOW,
        "loop-a",
        fs_types::OpenFlags::empty(),
        FsDescriptorFlags::READ,
        1,
    ))
    .err()
    .expect("a symlink loop must be rejected");
    assert!(matches!(error, fs_types::ErrorCode::Loop));
}

/// A Wasmtime component artifact as the boot payload carries it: the
/// bare precompiled ELF, with no signature trailer. Classification
/// reads only the header (the ELF identification, the Wasmtime OS ABI and
/// the component flag), so the fixture is that header with no sections:
/// the kernel's Wasmtime has no compiler to build a real one.
fn bootfs_cwasm() -> &'static [u8] {
    let mut header = [0_u8; 64];
    header[..4].copy_from_slice(b"\x7fELF");
    header[4] = 2; // ELFCLASS64
    header[5] = 1; // little-endian
    header[6] = 1; // EV_CURRENT
    header[7] = 200; // ELFOSABI_WASMTIME
    header[16..18].copy_from_slice(&1_u16.to_le_bytes()); // ET_REL
    header[20..24].copy_from_slice(&1_u32.to_le_bytes()); // e_version
    header[48..52].copy_from_slice(&2_u32.to_le_bytes()); // EF_WASMTIME_COMPONENT
    header[52..54].copy_from_slice(&64_u16.to_le_bytes()); // e_ehsize
    header[58..60].copy_from_slice(&64_u16.to_le_bytes()); // e_shentsize
    alloc::boxed::Box::leak(alloc::boxed::Box::new(header))
}

/// A cwasm signed by a key the kernel does not trust: it parses as an
/// artifact, and only a signature check tells it apart.
fn foreign_signed_cwasm() -> &'static [u8] {
    let signed = helios_artifact::sign_payload_with_secret_key_bytes(b"payload", &[7; 32])
        .expect("signing a test payload must succeed");
    alloc::boxed::Box::leak(signed.into_boxed_slice())
}

/// The compiler-signing key of one boot, derived from a fixed seed.
fn boot_key(seed: u8) -> crate::CwasmSigningKey {
    crate::CwasmSigningKey::from_seed(&helios_hal::measured_boot::SealedSecret::new(
        [seed; helios_hal::measured_boot::SealedSecret::LEN],
    ))
}

/// A namespace whose boot image carries `bootfs_cwasm` at `/bin/prog`.
fn namespace_with_bootfs_cwasm() -> TestNamespace {
    let files = alloc::boxed::Box::leak(alloc::boxed::Box::new([EmbeddedBootFile::new(
        "bin/prog",
        bootfs_cwasm(),
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
    // It carries no signature trailer, so accepting it proves the
    // bootfs path pays no per-artifact verification.
    assert!(
        cwasm::verify_signed_artifact(UntrustedCwasm::new(bytes.as_slice()), &boot_key(1)).is_err()
    );
    let trusted = cwasm::trust_bootfs_artifact(&bytes);
    assert_eq!(trusted.payload(), bytes.as_slice());
}

#[test]
fn guest_written_cwasm_must_verify() {
    let kernel_key = boot_key(1);
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
        // A bare cwasm has no signature to verify, so the copy is never
        // a bootfs or signed artifact; it is raw bytes the wasm
        // validator then refuses.
        assert!(
            matches!(
                program_source(&namespace, &alloc::format!("/{copy}")),
                ProgramSource::RawWasm(_)
            ),
            "a guest-written copy at /{copy} must classify as raw wasm"
        );
    }

    block_on(write_program_file(
        &namespace,
        "/tmp/foreign",
        foreign_signed_cwasm(),
        3,
    ))
    .expect("writing a program file must succeed");
    let ProgramSource::SignedArtifact(bytes) = program_source(&namespace, "/tmp/foreign") else {
        panic!("a guest-written cwasm must classify as a signed artifact");
    };
    assert!(matches!(
        cwasm::verify_signed_artifact(UntrustedCwasm::new(&bytes), &kernel_key),
        Err(cwasm::ArtifactTrustError::Trailer(
            helios_artifact::TrailerError::UntrustedSigner
        ))
    ));

    // The build root verifies the boot payload and nothing else: a cwasm
    // signed with its secret, which any image built before it left the
    // kernel would have carried, is refused.
    block_on(write_program_file(
        &namespace,
        "/tmp/root-signed",
        &cwasm::sign_artifact_with_build_root_for_tests(b"payload"),
        3,
    ))
    .expect("writing a program file must succeed");
    let ProgramSource::SignedArtifact(bytes) = program_source(&namespace, "/tmp/root-signed")
    else {
        panic!("a guest-written cwasm must classify as a signed artifact");
    };
    assert!(matches!(
        cwasm::verify_signed_artifact(UntrustedCwasm::new(&bytes), &kernel_key),
        Err(cwasm::ArtifactTrustError::Trailer(
            helios_artifact::TrailerError::UntrustedSigner
        ))
    ));

    // What an earlier boot's key signed does not verify under this
    // boot's key.
    let previous_boot = cwasm::sign_artifact_payload(b"payload", &boot_key(2))
        .expect("signing with a boot key must succeed");
    assert!(matches!(
        cwasm::verify_signed_artifact(UntrustedCwasm::new(&previous_boot), &kernel_key),
        Err(cwasm::ArtifactTrustError::Trailer(
            helios_artifact::TrailerError::UntrustedSigner
        ))
    ));

    // A kernel-signed artifact with one signature byte flipped, as
    // `write_program_file` would store it, is refused too.
    let mut tampered = cwasm::sign_artifact_payload(b"payload", &kernel_key)
        .expect("signing with the kernel key must succeed");
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
        cwasm::verify_signed_artifact(UntrustedCwasm::new(&bytes), &kernel_key),
        Err(cwasm::ArtifactTrustError::Trailer(
            helios_artifact::TrailerError::InvalidPayloadSignature
        ))
    ));

    // What the in-kernel compiler writes is signed by this boot's key,
    // and that is what lets a guest exec it from anywhere.
    let signed = cwasm::sign_artifact_payload(b"payload", &kernel_key)
        .expect("signing with the kernel key must succeed");
    block_on(write_program_file(&namespace, "/tmp/compiled", &signed, 5))
        .expect("writing a program file must succeed");
    let ProgramSource::SignedArtifact(bytes) = program_source(&namespace, "/tmp/compiled") else {
        panic!("a guest-written cwasm must classify as a signed artifact");
    };
    let trusted = cwasm::verify_signed_artifact(UntrustedCwasm::new(&bytes), &kernel_key)
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
            FsDescriptorFlags::READ | FsDescriptorFlags::WRITE,
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
            block_on(remove_directory_at(
                &namespace,
                held(&work()),
                path,
                7_000_000_000
            )),
            Err(fs_types::ErrorCode::Invalid)
        ));
        assert!(matches!(
            block_on(unlink_file_at(
                &namespace,
                held(&work()),
                path,
                7_000_000_000
            )),
            Err(fs_types::ErrorCode::Invalid)
        ));
        assert!(matches!(
            block_on(symlink_at(&namespace, held(&work()), "bob", path, 1)),
            Err(fs_types::ErrorCode::Invalid)
        ));
        assert!(matches!(
            rename(&namespace, &work(), path, &root(), "moved"),
            Err(fs_types::ErrorCode::Invalid)
        ));
        assert!(matches!(
            rename(&namespace, &root(), "bob", &work(), path),
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
            held(&work()),
            fs_types::PathFlags::SYMLINK_FOLLOW,
            path,
            fs_types::OpenFlags::DIRECTORY,
            FsDescriptorFlags::READ,
            1,
        ));
        assert!(matches!(opened, Err(fs_types::ErrorCode::NotPermitted)));
        assert!(matches!(
            block_on(stat_at(
                &namespace,
                held(&work()),
                fs_types::PathFlags::SYMLINK_FOLLOW,
                path
            )),
            Err(fs_types::ErrorCode::NotPermitted)
        ));
    }
    // A load granted at `/work` reads nothing through the link either.
    assert!(matches!(
        block_on(read_program_file(&namespace, "/work", "/work")),
        Err(helios_hal::fs::FsError::NotPermitted)
    ));
    // The parent, whose floor holds both, still follows it.
    let followed = block_on(open_at(
        &namespace,
        held(&root()),
        fs_types::PathFlags::SYMLINK_FOLLOW,
        "work",
        fs_types::OpenFlags::DIRECTORY,
        FsDescriptorFlags::READ,
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
        FsDescriptorFlags::WRITE,
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
    assert_eq!(end.end(), Some(3 + "日本語".len() as u64));

    let end = block_on(super::write(
        &namespace,
        held(&appender),
        WritePosition::End,
        "한국어".as_bytes(),
        3,
    ))
    .expect("the append lands");
    let contents = read_all(&namespace, &appender);
    assert_eq!(end.end(), Some(contents.len() as u64));
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
                block_on(remove_directory_at(
                    &namespace,
                    held(&root()),
                    path,
                    7_000_000_000
                )),
                Err(fs_types::ErrorCode::Busy)
            ),
            "removing {source} must be refused as busy"
        );
        assert!(
            matches!(
                rename(&namespace, &root(), path, &root(), "z"),
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
    block_on(namespace.mount(
        &cap,
        fs,
        granted(FileSystemRights::READ | FileSystemRights::WRITE),
    ))
    .expect("mount must succeed");
}

/// A directory descriptor at `path` that holds exactly `flags`.
fn directory_holding(path: &str, flags: FsDescriptorFlags) -> FsDescriptor {
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
        directory_holding("/mnt/a", FsDescriptorFlags::READ),
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

    let reached = block_on(async {
        remove_directory_at(&namespace, rw.grant()?, "old", 7_000_000_000).await
    });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));

    let refused = block_on(async {
        remove_directory_at(&namespace, ro.grant()?, "old", 7_000_000_000).await
    });
    assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
}

#[test]
fn a_read_only_base_renames_nothing_on_the_host_share() {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    let (rw, ro) = host_bases();

    let reached = rename(&namespace, &rw, "old", &rw, "new");
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));

    // A source that may be read but not changed, into a destination that
    // may change but not write: only the source's `mutate-directory` is
    // missing.
    let read = FsDescriptorFlags::READ;
    let no_write = directory_holding("/mnt/a", read | FsDescriptorFlags::MUTATE_DIRECTORY);
    let from_read_only = rename(&namespace, &ro, "old", &no_write, "new");
    assert!(matches!(from_read_only, Err(fs_types::ErrorCode::ReadOnly)));
    let into_read_only = rename(&namespace, &rw, "old", &ro, "new");
    assert!(matches!(into_read_only, Err(fs_types::ErrorCode::ReadOnly)));
}

#[test]
fn a_read_only_base_unlinks_nothing_on_the_host_share() {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    let (rw, ro) = host_bases();

    let reached =
        block_on(async { unlink_file_at(&namespace, rw.grant()?, "old", 7_000_000_000).await });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));

    let refused =
        block_on(async { unlink_file_at(&namespace, ro.grant()?, "old", 7_000_000_000).await });
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
        flags: FsDescriptorFlags::READ,
        ..file.clone()
    };

    let refused = read_only.grant_touch().err();
    assert!(matches!(refused, Some(fs_types::ErrorCode::ReadOnly)));
    assert_eq!(times_of(&namespace, "/tmp"), before);

    let writer = FsDescriptor {
        flags: FsDescriptorFlags::WRITE,
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
        directory(FsDescriptorFlags::READ | FsDescriptorFlags::WRITE)
            .grant_touch()
            .err(),
        Some(fs_types::ErrorCode::ReadOnly)
    ));
    let mutator = directory(FsDescriptorFlags::MUTATE_DIRECTORY);
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
            file(FsDescriptorFlags::WRITE).grant_touch()?,
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
            file(FsDescriptorFlags::READ).grant_touch()?,
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
            fs_types::PathFlags::empty(),
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
        (FsDescriptorFlags::WRITE, (11, 17)),
        (FsDescriptorFlags::MUTATE_DIRECTORY, (23, 29)),
    ] {
        let base = base(flags);
        block_on(set_times_at(
            &namespace,
            held(&base),
            fs_types::PathFlags::empty(),
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
        set_times_at(
            &namespace,
            rw.grant()?,
            fs_types::PathFlags::empty(),
            "file",
            Some(1),
            Some(2),
            3,
        )
        .await
    });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));
    let refused = block_on(async {
        set_times_at(
            &namespace,
            ro.grant()?,
            fs_types::PathFlags::empty(),
            "file",
            Some(1),
            Some(2),
            3,
        )
        .await
    });
    assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
}

#[test]
fn a_read_only_base_creates_or_truncates_nothing_before_asking_the_host() {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    let (rw, ro) = host_bases();
    let read = FsDescriptorFlags::READ;

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
    let read = FsDescriptorFlags::READ;
    let read_only = directory_holding("/mnt", read);
    let no_write = directory_holding("/mnt", read | FsDescriptorFlags::MUTATE_DIRECTORY);

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

/// Links `source_path` below `source` to `destination_path` below
/// `destination`, minting the proof the way every surface does.
fn link(
    namespace: &TestNamespace,
    source: &FsDescriptor,
    source_path: &str,
    destination: &FsDescriptor,
    destination_path: &str,
) -> Result<(), fs_types::ErrorCode> {
    block_on(async {
        let proof = source.grant()?.alias_into(destination.grant()?)?;
        link_at(namespace, proof, source_path, destination_path, 1).await
    })
}

/// Renames `source_path` below `source` to `destination_path` below
/// `destination`, minting the proof the way every surface does.
fn rename(
    namespace: &TestNamespace,
    source: &FsDescriptor,
    source_path: &str,
    destination: &FsDescriptor,
    destination_path: &str,
) -> Result<(), fs_types::ErrorCode> {
    block_on(async {
        let proof = source
            .grant()?
            .alias_into(destination.grant()?)?
            .removing_source()?;
        rename_at(namespace, proof, source_path, destination_path, 1).await
    })
}

fn list(
    namespace: &TestNamespace,
    directory: &FsDescriptor,
) -> Result<alloc::vec::Vec<fs_types::DirectoryEntry>, fs_types::ErrorCode> {
    block_on(async { read_directory(namespace, directory.grant()?).await })
}

fn missing(namespace: &TestNamespace, path: &str) -> bool {
    matches!(
        block_on(stat_path(namespace, path, Follow::No)),
        Err(fs_types::ErrorCode::NoEntry)
    )
}

const SECRET: &str = "秘密の文書 비밀 문서 机密文件";

/// `/a/secret` holding [`SECRET`], and an empty `/b`, on the root layer.
fn secret_in_a() -> TestNamespace {
    let namespace = namespace();
    for name in ["a", "b"] {
        block_on(create_directory_at(&namespace, held(&root()), name, 1))
            .expect("directory creation must succeed");
    }
    create_file(&namespace, "a/secret", SECRET.as_bytes());
    namespace
}

#[test]
fn a_link_needs_the_read_right_on_its_source() {
    let namespace = secret_in_a();
    let write_only = FsDescriptorFlags::WRITE | FsDescriptorFlags::MUTATE_DIRECTORY;
    let source = directory_holding("/a", write_only);
    let destination = directory_holding("/b", root().flags);

    let refused = link(&namespace, &source, "secret", &destination, "copy");
    assert!(
        matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
        "a link from a base that cannot be read must be refused, got {refused:?}"
    );
    assert!(missing(&namespace, "/b/copy"));
}

#[test]
fn a_link_cannot_make_a_read_only_file_writable() {
    let namespace = secret_in_a();
    let read = FsDescriptorFlags::READ;
    let read_only = directory_holding("/a", read);
    let read_write = directory_holding("/b", root().flags);

    let refused = link(&namespace, &read_only, "secret", &read_write, "copy");
    assert!(
        matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
        "a read-only source must not feed a writable destination, got {refused:?}"
    );
    assert!(missing(&namespace, "/b/copy"));
    let secret = open(
        &namespace,
        &root(),
        "a/secret",
        fs_types::OpenFlags::empty(),
        read,
    )
    .expect("the secret still opens");
    assert_eq!(read_all(&namespace, &secret), SECRET.as_bytes());

    // The same refusal before any request reaches a host share.
    let host_namespace = namespace_with_host();
    let (rw, ro) = host_bases();
    let reached = link(&host_namespace, &rw, "secret", &rw, "copy");
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));
    let refused = link(&host_namespace, &ro, "secret", &rw, "copy");
    assert!(
        matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
        "the host share must not be asked, got {refused:?}"
    );

    // A destination that cannot write takes a read-only source, and a
    // writable source feeds a writable destination.
    let no_write = directory_holding("/b", read | FsDescriptorFlags::MUTATE_DIRECTORY);
    link(&namespace, &read_only, "secret", &no_write, "view").expect("read-only to no-write");
    let writable = directory_holding("/a", root().flags);
    link(&namespace, &writable, "secret", &read_write, "copy").expect("read-write to read-write");
    let copy = open(
        &namespace,
        &root(),
        "b/copy",
        fs_types::OpenFlags::empty(),
        read,
    )
    .expect("the copy opens");
    assert_eq!(read_all(&namespace, &copy), SECRET.as_bytes());
}

#[test]
fn a_link_needs_mutate_on_its_destination() {
    let namespace = secret_in_a();
    let source = directory_holding("/a", root().flags);
    let destination = directory_holding("/b", FsDescriptorFlags::READ | FsDescriptorFlags::WRITE);

    let refused = link(&namespace, &source, "secret", &destination, "copy");
    assert!(matches!(refused, Err(fs_types::ErrorCode::ReadOnly)));
    assert!(missing(&namespace, "/b/copy"));
}

/// An in-memory mount at `/mnt` holding `/mnt/a/secret` ([`SECRET`]), a link
/// `/mnt/a/pointer` to it and an empty `/mnt/b`.
fn secret_on_a_memory_mount() -> TestNamespace {
    let namespace = namespace();
    mount_memory(&namespace, "/mnt");
    for name in ["mnt/a", "mnt/b"] {
        block_on(create_directory_at(&namespace, held(&root()), name, 1))
            .expect("directory creation must succeed");
    }
    create_file(&namespace, "mnt/a/secret", SECRET.as_bytes());
    block_on(symlink_at(
        &namespace,
        held(&directory_holding("/mnt/a", root().flags)),
        "secret",
        "pointer",
        1,
    ))
    .expect("the link is created");
    namespace
}

/// What `stat-at`, `readlink-at`, `open-at` and `metadata-hash-at` (which
/// is `stat-at`) answer through `base`, as error codes.
fn lookups_through(
    namespace: &TestNamespace,
    base: &FsDescriptor,
) -> [Result<(), fs_types::ErrorCode>; 3] {
    let stat = block_on(async {
        stat_at(
            namespace,
            base.grant()?,
            fs_types::PathFlags::empty(),
            "secret",
        )
        .await
        .map(drop)
    });
    let readlink = block_on(async {
        readlink_at(namespace, base.grant()?, "pointer")
            .await
            .map(drop)
    });
    let open = open(
        namespace,
        base,
        "secret",
        fs_types::OpenFlags::empty(),
        FsDescriptorFlags::READ,
    )
    .map(drop);
    [stat, readlink, open]
}

#[test]
fn a_lookup_needs_the_read_right_on_its_base() {
    let namespace = secret_on_a_memory_mount();
    let readable = directory_holding("/mnt/a", FsDescriptorFlags::READ);
    let unreadable = directory_holding(
        "/mnt/a",
        FsDescriptorFlags::WRITE | FsDescriptorFlags::MUTATE_DIRECTORY,
    );

    // The control: a readable base looks everything up.
    for answer in lookups_through(&namespace, &readable) {
        assert!(
            matches!(answer, Ok(())),
            "a readable base looks up: {answer:?}"
        );
    }
    for answer in lookups_through(&namespace, &unreadable) {
        assert!(
            matches!(answer, Err(fs_types::ErrorCode::NotPermitted)),
            "a lookup through an unreadable base must be refused, got {answer:?}"
        );
    }

    // On a host share the refusal comes before any request: the share
    // answers every request `unsupported`, so any other code proves none
    // was made.
    let host_namespace = namespace_with_host();
    let (rw, _) = host_bases();
    for answer in lookups_through(&host_namespace, &rw) {
        assert!(
            matches!(answer, Err(fs_types::ErrorCode::Unsupported)),
            "a readable base reaches the share, got {answer:?}"
        );
    }
    let unreadable = directory_holding(
        "/mnt/a",
        FsDescriptorFlags::WRITE | FsDescriptorFlags::MUTATE_DIRECTORY,
    );
    for answer in lookups_through(&host_namespace, &unreadable) {
        assert!(
            matches!(answer, Err(fs_types::ErrorCode::NotPermitted)),
            "a lookup through an unreadable base must be refused, got {answer:?}"
        );
    }
}

#[test]
fn a_rename_needs_the_read_right_on_its_source() {
    let namespace = secret_on_a_memory_mount();
    let source = directory_holding(
        "/mnt/a",
        FsDescriptorFlags::WRITE | FsDescriptorFlags::MUTATE_DIRECTORY,
    );
    let destination = directory_holding("/mnt/b", root().flags);

    let refused = rename(&namespace, &source, "secret", &destination, "moved");
    assert!(
        matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
        "the rename must be refused, got {refused:?}"
    );
    assert!(missing(&namespace, "/mnt/b/moved"));
    assert!(!missing(&namespace, "/mnt/a/secret"));

    // The same refusal before any request reaches a host share.
    let host_namespace = namespace_with_host();
    let (rw, _) = host_bases();
    let reached = rename(&host_namespace, &rw, "secret", &rw, "moved");
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));
    let unreadable = directory_holding("/mnt/a", source.flags);
    let refused = rename(&host_namespace, &unreadable, "secret", &rw, "moved");
    assert!(
        matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
        "the rename must be refused, got {refused:?}"
    );
}

#[test]
fn a_rename_cannot_make_a_read_only_file_writable() {
    let namespace = secret_on_a_memory_mount();
    let read = FsDescriptorFlags::READ;
    let mutate = FsDescriptorFlags::MUTATE_DIRECTORY;
    let read_only = directory_holding("/mnt/a", read | mutate);
    let read_write = directory_holding("/mnt/b", root().flags);

    let refused = rename(&namespace, &read_only, "secret", &read_write, "moved");
    assert!(
        matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
        "the rename must be refused, got {refused:?}"
    );
    assert!(missing(&namespace, "/mnt/b/moved"));
    assert!(!missing(&namespace, "/mnt/a/secret"));

    // The same refusal before any request reaches a host share.
    let host_namespace = namespace_with_host();
    let refused = rename(
        &host_namespace,
        &directory_holding("/mnt/a", read | mutate),
        "secret",
        &directory_holding("/mnt/b", root().flags),
        "moved",
    );
    assert!(
        matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
        "the rename must be refused, got {refused:?}"
    );

    // Bases of equal rights still rename, and so does a destination that
    // cannot write.
    let writable = directory_holding("/mnt/a", root().flags);
    rename(&namespace, &writable, "secret", &read_write, "moved").expect("equal rights rename");
    let no_write = directory_holding("/mnt/a", read | mutate);
    rename(&namespace, &read_write, "moved", &no_write, "back").expect("a no-write destination");
    assert!(missing(&namespace, "/mnt/b/moved"));
    let back = open(
        &namespace,
        &root(),
        "mnt/a/back",
        fs_types::OpenFlags::empty(),
        read,
    )
    .expect("the file arrives");
    assert_eq!(read_all(&namespace, &back), SECRET.as_bytes());
}

fn namespace_with_host() -> TestNamespace {
    let namespace = namespace();
    mount_unreachable_host(&namespace, "/mnt");
    namespace
}

#[test]
fn listing_a_directory_needs_the_read_right() {
    let namespace = secret_in_a();
    let mutate_only = FsDescriptorFlags::MUTATE_DIRECTORY;
    let readable = directory_holding("/a", FsDescriptorFlags::READ);
    let names: alloc::vec::Vec<_> = list(&namespace, &readable)
        .expect("a readable directory lists")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["secret"]);
    let refused = list(&namespace, &directory_holding("/a", mutate_only));
    assert!(
        matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
        "listing without the read right must be refused, got {:?}",
        refused.map(|entries| entries.len())
    );

    let host_namespace = namespace_with_host();
    let (rw, _) = host_bases();
    let reached = list(&host_namespace, &rw);
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));
    let refused = list(&host_namespace, &directory_holding("/mnt/a", mutate_only));
    assert!(
        matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
        "the host share must not be listed, got {:?}",
        refused.map(|entries| entries.len())
    );
}

#[test]
fn reads_need_the_read_right_on_every_backing() {
    let namespace = namespace_with_host();
    mount_memory(&namespace, "/memory");
    let on_root = create_file(&namespace, "root-file", SECRET.as_bytes());
    let in_memory = create_file(&namespace, "memory/file", SECRET.as_bytes());
    let on_host = FsDescriptor {
        path: String::from("/mnt/file"),
        ..on_root.clone()
    };

    for file in [&on_root, &in_memory, &on_host] {
        let write_only = FsDescriptor {
            flags: FsDescriptorFlags::WRITE,
            ..file.clone()
        };
        let read_only = FsDescriptor {
            flags: FsDescriptorFlags::READ,
            ..file.clone()
        };
        let path = &file.path;

        let refused = block_on(async { read(&namespace, write_only.grant()?, 0, 64).await });
        assert!(
            matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
            "reading {path} without the right must be refused, got {:?}",
            refused.map(|bytes| bytes.len())
        );
        let refused = write_only
            .grant()
            .and_then(|file| read_stream_target(&namespace, file));
        assert!(
            matches!(refused, Err(fs_types::ErrorCode::NotPermitted)),
            "a read stream on {path} needs the right"
        );
        let refused = read_only
            .grant()
            .and_then(|file| write_stream_target(&namespace, file));
        assert!(
            matches!(refused, Err(fs_types::ErrorCode::ReadOnly)),
            "a write stream on {path} needs the right"
        );
    }

    // The control: with the right, the host share is the one that answers.
    let readable = FsDescriptor {
        flags: FsDescriptorFlags::READ,
        ..on_host
    };
    let reached = block_on(async { read(&namespace, readable.grant()?, 0, 64).await });
    assert!(matches!(reached, Err(fs_types::ErrorCode::Unsupported)));
}

#[test]
fn a_create_lands_on_the_mount_that_owns_the_new_entry() {
    let namespace = namespace();
    let fs = MountedFs::Memory(MemoryFs::new(namespace.table().allocate_device()));
    let cap = crate::ProcessAuthority::root()
        .derive_mount_cap("/mnt")
        .expect("root may mount anywhere");
    block_on(namespace.mount(&cap, fs, granted(FileSystemRights::READ)))
        .expect("mount must succeed");

    let created = open(
        &namespace,
        &root(),
        "mnt/new",
        fs_types::OpenFlags::CREATE,
        FsDescriptorFlags::READ,
    );
    assert!(matches!(created, Err(fs_types::ErrorCode::ReadOnly)));
    let directory = block_on(create_directory_at(&namespace, held(&root()), "mnt/d", 1));
    assert!(matches!(directory, Err(fs_types::ErrorCode::ReadOnly)));
    let symlink = block_on(symlink_at(&namespace, held(&root()), "target", "mnt/l", 1));
    assert!(matches!(symlink, Err(fs_types::ErrorCode::ReadOnly)));
    for name in ["new", "d", "l"] {
        assert!(missing(&namespace, &alloc::format!("/mnt/{name}")));
    }
}

#[test]
fn file_stream_fills_destinations_without_moving_source_bytes() {
    use super::fs::file_write_consumer;
    use super::stream::test_source::{Reads, TrackedBuffer};
    use crate::wasmtime_adapter::component_host::service::test_store;
    use core::sync::atomic::Ordering;
    use std::sync::Arc;
    use wasmtime::component::StreamReader;

    for (position, expected) in [
        (WritePosition::At(0), "文件・ファイル・파일"),
        (WritePosition::End, "prefix文件・ファイル・파일"),
    ] {
        let namespace = namespace();
        let descriptor = create_file(&namespace, "stream", b"prefix");
        let target = write_stream_target(&namespace, held(&descriptor)).expect("write target");
        let bytes = "文件・ファイル・파일".as_bytes();
        let reads = Arc::new(Reads::default());
        let mut config = wasmtime::Config::new();
        config
            .wasm_component_model(true)
            .wasm_component_model_async(true);
        let engine = wasmtime::Engine::new(&config).expect("stream engine");
        let mut store = wasmtime::Store::new(&engine, test_store::store_data_with(|_| {}));
        let (tx, rx) = futures::channel::oneshot::channel();
        let consumer = file_write_consumer(|state| state, target, position, tx);
        let source = TrackedBuffer {
            bytes,
            reads: reads.clone(),
        };
        block_on(store.run_concurrent(async |accessor| {
            accessor
                .with(|mut store| StreamReader::new(&mut store, source)?.pipe(store, consumer))
                .expect("connect file stream");
            rx.await.expect("completion sent").expect("write completed");
        }))
        .expect("stream event loop");
        assert_eq!(reads.skipped.load(Ordering::Relaxed), bytes.len());
        assert_eq!(reads.moved.load(Ordering::Relaxed), 0);
        assert_eq!(read_all(&namespace, &descriptor), expected.as_bytes());
    }
}

#[derive(Clone)]
struct MemoryHostFileSystem {
    inner: MemoryFs,
    domain: AuthorityDomain,
}

impl MemoryHostFileSystem {
    fn new(device: helios_hal::fs::DeviceId) -> Self {
        Self {
            inner: MemoryFs::new(device),
            domain: AuthorityDomain::HOST_SHARE_9P,
        }
    }

    fn host_error(error: helios_hal::fs::FsError) -> crate::HostFsError {
        use helios_hal::fs::FsError;
        use helios_hal::io::IoError;
        let io = match error {
            FsError::NoEntry => IoError::NotFound,
            FsError::Exists => IoError::AlreadyExists,
            FsError::NotDirectory => IoError::NotDirectory,
            FsError::IsDirectory => IoError::IsDirectory,
            FsError::NotEmpty => IoError::DirectoryNotEmpty,
            FsError::ReadOnly => IoError::ReadOnly,
            FsError::NotPermitted => IoError::PermissionDenied,
            FsError::Unsupported | FsError::Invalid | FsError::Loop | FsError::CrossDevice => {
                IoError::Unsupported
            }
            FsError::NoSpace => return crate::HostFsError::Protocol("no space"),
            FsError::NameTooLong => {
                return crate::HostFsError::NameTooLong {
                    len: 256,
                    max_len: 255,
                };
            }
            FsError::Overflow | FsError::IllegalByteSequence | FsError::Busy | FsError::Io => {
                IoError::DeviceFault
            }
        };
        crate::HostFsError::Transport(io)
    }

    fn fs_path(path: &str) -> Result<helios_hal::fs::FsPath<'_>, crate::HostFsError> {
        let trimmed = path.strip_prefix('/').unwrap_or(path);
        helios_hal::fs::FsPath::new(trimmed)
            .ok_or(crate::HostFsError::Protocol("invalid host path"))
    }

    fn host_metadata(
        &self,
        path: helios_hal::fs::FsPath<'_>,
        metadata: helios_hal::fs::Metadata,
    ) -> Result<crate::HostMetadata, crate::HostFsError> {
        let unix = self.inner.unix_metadata(path).map_err(Self::host_error)?;
        let kind_bits = match metadata.kind {
            NodeKind::Directory => 0o040_000,
            NodeKind::File => 0o100_000,
            NodeKind::Symlink => 0o120_000,
            NodeKind::CharacterDevice => 0o020_000,
            NodeKind::BlockDevice => 0o060_000,
            NodeKind::Fifo => 0o010_000,
            NodeKind::Socket => 0o140_000,
        };
        let qid_type = match metadata.kind {
            NodeKind::Directory => 0x80,
            NodeKind::Symlink => 0x02,
            _ => 0x00,
        };
        Ok(crate::HostMetadata {
            identity: crate::ObjectIdentity::new(self.domain, metadata.id.inode),
            qid_path: metadata.id.inode,
            qid_type,
            mode: kind_bits | unix.attributes.permissions,
            uid: unix.attributes.uid,
            gid: unix.attributes.gid,
            rdev: u64::from(unix.rdev.encode()),
            size: metadata.size,
            link_count: metadata.link_count,
            access_nanos: metadata.access_nanos,
            modified_nanos: metadata.modified_nanos,
            status_nanos: metadata.status_nanos,
        })
    }
}

impl crate::HostFileSystem for MemoryHostFileSystem {
    type Write = core::convert::Infallible;

    fn cache_stats(&self) -> Option<crate::HostFsCacheStats> {
        None
    }

    async fn after_machine_restore(&self, _restore: crate::HostShareRestore) {}

    async fn prepare_snapshot(&self) {}

    fn snapshot_finished(&self) {}

    async fn stat_path(&self, path: &str) -> Result<crate::HostMetadata, crate::HostFsError> {
        let path = Self::fs_path(path)?;
        use helios_hal::fs::FileSystem as _;
        let metadata = self.inner.stat(path).await.map_err(Self::host_error)?;
        self.host_metadata(path, metadata)
    }

    async fn read_dir(
        &self,
        path: &str,
        limit: crate::HostDirLimit,
    ) -> Result<alloc::vec::Vec<crate::HostDirEntry>, crate::HostFsError> {
        let path = Self::fs_path(path)?;
        use helios_hal::fs::FileSystem as _;
        let entries = self
            .inner
            .read_dir(path, helios_hal::fs::DirCookie::START, limit.max_entries)
            .await
            .map_err(Self::host_error)?;
        Ok(entries
            .into_iter()
            .map(|entry| crate::HostDirEntry {
                name: entry.name,
                kind: Some(entry.kind),
            })
            .collect())
    }

    async fn read_file(&self, path: &str) -> Result<alloc::vec::Vec<u8>, crate::HostFsError> {
        self.read_file_range(path, 0, u32::MAX).await
    }

    async fn read_file_range(
        &self,
        path: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<alloc::vec::Vec<u8>, crate::HostFsError> {
        let path = Self::fs_path(path)?;
        use helios_hal::fs::FileSystem as _;
        let bytes = self
            .inner
            .read_at(path, offset, max_bytes as usize)
            .await
            .map_err(Self::host_error)?;
        Ok(bytes.to_vec())
    }

    async fn prepare_write(
        &self,
        _path: &str,
        _position: WritePosition,
        _len: usize,
    ) -> Result<Self::Write, crate::HostFsError> {
        Err(crate::HostFsError::Transport(
            helios_hal::io::IoError::Unsupported,
        ))
    }

    async fn sync_file(&self, path: &str) -> Result<(), crate::HostFsError> {
        let path = Self::fs_path(path)?;
        use helios_hal::fs::FileSystem as _;
        self.inner.sync(path).await.map_err(Self::host_error)
    }

    async fn truncate_file(&self, path: &str) -> Result<(), crate::HostFsError> {
        let path = Self::fs_path(path)?;
        use helios_hal::fs::FileSystem as _;
        self.inner
            .open(path, helios_hal::fs::OpenFlags::TRUNCATE, 1)
            .await
            .map(|_| ())
            .map_err(Self::host_error)
    }

    async fn set_file_size(&self, path: &str, size: u64) -> Result<(), crate::HostFsError> {
        let path = Self::fs_path(path)?;
        use helios_hal::fs::FileSystem as _;
        self.inner
            .set_size(path, size, 1)
            .await
            .map_err(Self::host_error)
    }

    async fn set_times(
        &self,
        path: &str,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
    ) -> Result<(), crate::HostFsError> {
        let path = Self::fs_path(path)?;
        use helios_hal::fs::FileSystem as _;
        self.inner
            .set_times(path, access_nanos, modified_nanos, 99)
            .await
            .map_err(Self::host_error)
    }

    async fn create_file(
        &self,
        path: &str,
        creation: crate::HostCreation,
    ) -> Result<(), crate::HostFsError> {
        let path = Self::fs_path(path)?;
        let parent_uid = path
            .split_last()
            .and_then(|(parent, _)| self.inner.unix_metadata(parent).ok())
            .map_or(0, |meta| meta.attributes.uid);
        self.inner
            .create_file_with(
                path,
                crate::UnixAttributes {
                    permissions: creation.permissions,
                    uid: parent_uid,
                    gid: creation.gid,
                },
                1,
            )
            .map(|_| ())
            .map_err(Self::host_error)
    }

    async fn create_directory(
        &self,
        path: &str,
        creation: crate::HostCreation,
    ) -> Result<(), crate::HostFsError> {
        let path = Self::fs_path(path)?;
        let parent_uid = path
            .split_last()
            .and_then(|(parent, _)| self.inner.unix_metadata(parent).ok())
            .map_or(0, |meta| meta.attributes.uid);
        self.inner
            .create_directory_with(
                path,
                crate::UnixAttributes {
                    permissions: creation.permissions,
                    uid: parent_uid,
                    gid: creation.gid,
                },
                1,
            )
            .map_err(Self::host_error)
    }

    async fn remove(&self, path: &str, directory: bool) -> Result<(), crate::HostFsError> {
        let path = Self::fs_path(path)?;
        use helios_hal::fs::FileSystem as _;
        if directory {
            self.inner
                .remove_directory(path, 1)
                .await
                .map_err(Self::host_error)
        } else {
            self.inner.unlink(path, 1).await.map_err(Self::host_error)
        }
    }

    async fn rename(&self, source: &str, destination: &str) -> Result<(), crate::HostFsError> {
        let source = Self::fs_path(source)?;
        let destination = Self::fs_path(destination)?;
        use helios_hal::fs::FileSystem as _;
        self.inner
            .rename(source, destination, helios_hal::fs::RenameMode::Replace, 1)
            .await
            .map_err(Self::host_error)
    }

    async fn hard_link(&self, source: &str, destination: &str) -> Result<(), crate::HostFsError> {
        let source = Self::fs_path(source)?;
        let destination = Self::fs_path(destination)?;
        use helios_hal::fs::FileSystem as _;
        self.inner
            .link(source, destination, 1)
            .await
            .map_err(Self::host_error)
    }

    async fn symlink(
        &self,
        target: &str,
        link_path: &str,
        gid: u32,
    ) -> Result<(), crate::HostFsError> {
        let link_path = Self::fs_path(link_path)?;
        let parent_uid = link_path
            .split_last()
            .and_then(|(parent, _)| self.inner.unix_metadata(parent).ok())
            .map_or(0, |meta| meta.attributes.uid);
        self.inner
            .symlink_with(
                target,
                link_path,
                crate::UnixAttributes {
                    permissions: 0o777,
                    uid: parent_uid,
                    gid,
                },
                1,
            )
            .map_err(Self::host_error)
    }

    async fn read_link(&self, path: &str) -> Result<String, crate::HostFsError> {
        let path = Self::fs_path(path)?;
        use helios_hal::fs::FileSystem as _;
        self.inner.read_link(path).await.map_err(Self::host_error)
    }
}

#[derive(Clone)]
struct MemoryHostStorage;

impl crate::Storage for MemoryHostStorage {
    type HostShare = MemoryHostFileSystem;
    type Disk = crate::NoDisks;
}

fn memory_host_store_data() -> (
    crate::wasmtime_adapter::component_host::StoreData<
        crate::test_support::TestCpu,
        crate::test_support::TestNetworkService,
        MemoryHostStorage,
    >,
    MemoryHostFileSystem,
) {
    use crate::wasmtime_adapter::component_host::service::test_store::DiscardPort;
    use crate::wasmtime_adapter::component_host::{HostRuntimeState, StoreData};
    use crate::{Executor, InstanceRegistry, ProcessAuthority, TaskFunding};
    use helios_hal::cpu::{Cpu as _, ProcessorId};
    use helios_hal::watchdog::ProgressCounter;
    use triomphe::Arc;

    fn read_nothing(_: &mut alloc::vec::Vec<u8>, _: u32) {}

    let cpu = crate::test_support::TestCpu::with_entropy(0x5a);
    let runtime_state: HostRuntimeState<
        crate::test_support::TestCpu,
        crate::test_support::TestNetworkService,
        MemoryHostStorage,
    > = crate::RuntimeState::new(
        cpu.timebase(),
        1,
        0,
        crate::BootPayload::for_tests(),
        crate::isa::GuestIsa::for_build_target(cpu.native_feature_probe()),
    );
    runtime_state.install_root_entropy(Arc::new(
        crate::RootEntropy::from_platform(&cpu, None, None, crate::MachineGeneration::new(None))
            .expect("the fixture CPU has an entropy source"),
    ));
    let namespace = runtime_state.root_namespace();
    let cap = ProcessAuthority::root()
        .derive_mount_cap("/host")
        .expect("root may mount /host");
    let host_fs = MemoryHostFileSystem::new(namespace.table().allocate_device());
    block_on(namespace.mount(
        &cap,
        MountedFs::Host(HostShare::new(host_fs.clone())),
        granted(FileSystemRights::READ | FileSystemRights::WRITE),
    ))
    .expect("host mount must succeed");

    let executor = Executor::new(
        ProgressCounter::new(),
        crate::test_support::single_node_pool(1),
        ProcessorId::new(0),
    );
    let registry = InstanceRegistry::new();
    let instance = registry.register("fs-host-store", 0);
    let store = StoreData::new(
        wasmtime::component::ResourceTable::new(),
        cpu,
        executor.timer(cpu),
        executor
            .spawner(cpu)
            .instance_spawner(TaskFunding::Instance, &instance),
        runtime_state.clone(),
        registry,
        instance,
        None,
        runtime_state.root_namespace().descend(),
        alloc::vec::Vec::new(),
        alloc::vec::Vec::new(),
        ProcessAuthority::empty(),
        crate::component::ComponentOutputMode::Serial,
        read_nothing,
        crate::DebugSerialWriter::of::<DiscardPort>(),
    );
    (store, host_fs)
}

#[test]
fn open_at_refuses_create_directory_before_host_mutation() {
    use super::preview2::p2fs;
    use crate::wasmtime_adapter::component_host::service::test_store;

    let namespace = namespace_with_host();
    let (rw, _) = host_bases();
    let refused = open(
        &namespace,
        &rw,
        "newdir",
        fs_types::OpenFlags::CREATE | fs_types::OpenFlags::DIRECTORY,
        FsDescriptorFlags::READ,
    );
    assert!(matches!(refused, Err(fs_types::ErrorCode::Invalid)));

    // Both preview2 and p3 share the same validation and refuse before reaching the host.
    let mut store = test_store::store_data_with(|_| {});
    mount_unreachable_host(store.filesystem(), "/mnt");
    let p2_base = store
        .table
        .push(directory_holding("/mnt/a", root().flags))
        .expect("p2 base must fit");
    let p2_prepared =
        <wasmtime::component::HasSelf<_> as p2fs::HostDescriptorWithStore<()>>::prepare_open_at(
            &mut &mut store,
            p2_base,
            p2fs::PathFlags::empty(),
            "newdir",
            p2fs::OpenFlags::CREATE | p2fs::OpenFlags::DIRECTORY,
            p2fs::DescriptorFlags::READ,
        )
        .expect("prepare_open_at must not trap");
    assert!(matches!(
        block_on(super::fs::execute_open(p2_prepared)),
        Err(fs_types::ErrorCode::Invalid)
    ));

    let p3_base = store
        .table
        .push(directory_holding("/mnt/a", root().flags))
        .expect("p3 base must fit");
    let p3_prepared = <wasmtime::component::HasSelf<_> as fs_types::HostDescriptorWithStore<()>>::prepare_open_at(
        &mut &mut store,
        p3_base,
        fs_types::PathFlags::empty(),
        "newdir",
        fs_types::OpenFlags::CREATE | fs_types::OpenFlags::DIRECTORY,
        fs_types::DescriptorFlags::READ,
    )
    .expect("prepare_open_at must not trap");
    assert!(matches!(
        block_on(super::fs::execute_open(p3_prepared)),
        Err(fs_types::ErrorCode::Invalid)
    ));
}

#[test]
fn preview2_and_p3_open_at_validate_all_open_flags_on_embedded_and_host_share() {
    use super::preview2::p2fs;
    use helios_hal::fs::FileSystem as _;

    let (mut store, host_fs) = memory_host_store_data();
    // Seed an existing file and directory on both embedded root (`/`) and host share (`/host`).
    block_on(host_fs.inner.open(
        MemoryHostFileSystem::fs_path("existing.txt").unwrap(),
        helios_hal::fs::OpenFlags::CREATE,
        1,
    ))
    .expect("host file created");
    block_on(async {
        let write = host_fs
            .inner
            .prepare_write(
                MemoryHostFileSystem::fs_path("existing.txt").unwrap(),
                WritePosition::At(0),
                7,
                2,
            )
            .await
            .expect("prepare host write");
        use helios_hal::fs::{FilledWrite as _, PreparedWrite as _};
        let filled = write.fill(|at, dst| dst.copy_from_slice(&b"payload"[at..at + dst.len()]));
        filled.submit().await.expect("submit host write");
        host_fs
            .inner
            .create_directory(MemoryHostFileSystem::fs_path("subdir").unwrap(), 1)
            .await
            .expect("host subdir");
    });

    let root_ns = store.filesystem().clone();
    block_on(create_directory_at(&root_ns, held(&root()), "emb", 1)).expect("emb dir");
    block_on(create_directory_at(
        &root_ns,
        held(&root()),
        "emb/subdir",
        1,
    ))
    .expect("emb subdir");
    let emb_file = create_file(&root_ns, "emb/existing.txt", b"payload");
    assert_eq!(read_all(&root_ns, &emb_file), b"payload");

    for base_path in ["/emb", "/host"] {
        let base_p2 = store
            .table
            .push(directory_holding(base_path, root().flags))
            .expect("base fits");
        // CREATE | EXCLUSIVE on existing file -> Exist (in preview2 and p3).
        let p2_excl = <wasmtime::component::HasSelf<_> as p2fs::HostDescriptorWithStore<()>>::prepare_open_at(
            &mut &mut store,
            base_p2,
            p2fs::PathFlags::empty(),
            "existing.txt",
            p2fs::OpenFlags::CREATE | p2fs::OpenFlags::EXCLUSIVE,
            p2fs::DescriptorFlags::READ,
        )
        .expect("prepare");
        assert!(matches!(
            block_on(super::fs::execute_open(p2_excl)),
            Err(fs_types::ErrorCode::Exist)
        ));

        let base_p3 = store
            .table
            .push(directory_holding(base_path, root().flags))
            .expect("base fits");
        let p3_excl = <wasmtime::component::HasSelf<_> as fs_types::HostDescriptorWithStore<()>>::prepare_open_at(
            &mut &mut store,
            base_p3,
            fs_types::PathFlags::empty(),
            "existing.txt",
            fs_types::OpenFlags::CREATE | fs_types::OpenFlags::EXCLUSIVE,
            fs_types::DescriptorFlags::READ,
        )
        .expect("prepare");
        assert!(matches!(
            block_on(super::fs::execute_open(p3_excl)),
            Err(fs_types::ErrorCode::Exist)
        ));

        // DIRECTORY on a regular file -> NotDirectory (in preview2 and p3).
        let base_p2 = store
            .table
            .push(directory_holding(base_path, root().flags))
            .expect("base fits");
        let p2_dir_on_file = <wasmtime::component::HasSelf<_> as p2fs::HostDescriptorWithStore<
            (),
        >>::prepare_open_at(
            &mut &mut store,
            base_p2,
            p2fs::PathFlags::empty(),
            "existing.txt",
            p2fs::OpenFlags::DIRECTORY,
            p2fs::DescriptorFlags::READ,
        )
        .expect("prepare");
        assert!(matches!(
            block_on(super::fs::execute_open(p2_dir_on_file)),
            Err(fs_types::ErrorCode::NotDirectory)
        ));

        // DIRECTORY on a directory succeeds; without DIRECTORY -> IsDirectory.
        let base_p3 = store
            .table
            .push(directory_holding(base_path, root().flags))
            .expect("base fits");
        let p3_dir = <wasmtime::component::HasSelf<_> as fs_types::HostDescriptorWithStore<()>>::prepare_open_at(
            &mut &mut store,
            base_p3,
            fs_types::PathFlags::empty(),
            "subdir",
            fs_types::OpenFlags::DIRECTORY,
            fs_types::DescriptorFlags::READ,
        )
        .expect("prepare");
        assert_eq!(
            block_on(super::fs::execute_open(p3_dir))
                .expect("dir opens")
                .kind,
            FsNodeKind::Directory
        );

        let base_p2 = store
            .table
            .push(directory_holding(base_path, root().flags))
            .expect("base fits");
        let p2_no_dir = <wasmtime::component::HasSelf<_> as p2fs::HostDescriptorWithStore<()>>::prepare_open_at(
            &mut &mut store,
            base_p2,
            p2fs::PathFlags::empty(),
            "subdir",
            p2fs::OpenFlags::empty(),
            p2fs::DescriptorFlags::READ,
        )
        .expect("prepare");
        assert!(matches!(
            block_on(super::fs::execute_open(p2_no_dir)),
            Err(fs_types::ErrorCode::IsDirectory)
        ));

        // CREATE | DIRECTORY on missing path -> Invalid and creates nothing.
        let base_p3 = store
            .table
            .push(directory_holding(base_path, root().flags))
            .expect("base fits");
        let p3_create_dir = <wasmtime::component::HasSelf<_> as fs_types::HostDescriptorWithStore<()>>::prepare_open_at(
            &mut &mut store,
            base_p3,
            fs_types::PathFlags::empty(),
            "never-created",
            fs_types::OpenFlags::CREATE | fs_types::OpenFlags::DIRECTORY,
            fs_types::DescriptorFlags::READ,
        )
        .expect("prepare");
        assert!(matches!(
            block_on(super::fs::execute_open(p3_create_dir)),
            Err(fs_types::ErrorCode::Invalid)
        ));
        assert!(matches!(
            block_on(stat_path(
                store.filesystem(),
                &alloc::format!("{base_path}/never-created"),
                Follow::No
            )),
            Err(fs_types::ErrorCode::NoEntry)
        ));

        // TRUNCATE on regular file truncates to 0; on directory -> IsDirectory.
        let base_p2 = store
            .table
            .push(directory_holding(base_path, root().flags))
            .expect("base fits");
        let p2_trunc_dir = <wasmtime::component::HasSelf<_> as p2fs::HostDescriptorWithStore<()>>::prepare_open_at(
            &mut &mut store,
            base_p2,
            p2fs::PathFlags::empty(),
            "subdir",
            p2fs::OpenFlags::DIRECTORY | p2fs::OpenFlags::TRUNCATE,
            p2fs::DescriptorFlags::READ,
        )
        .expect("prepare");
        assert!(matches!(
            block_on(super::fs::execute_open(p2_trunc_dir)),
            Err(fs_types::ErrorCode::IsDirectory)
        ));

        let base_p2 = store
            .table
            .push(directory_holding(base_path, root().flags))
            .expect("base fits");
        let p2_trunc = <wasmtime::component::HasSelf<_> as p2fs::HostDescriptorWithStore<()>>::prepare_open_at(
            &mut &mut store,
            base_p2,
            p2fs::PathFlags::empty(),
            "existing.txt",
            p2fs::OpenFlags::TRUNCATE,
            p2fs::DescriptorFlags::READ,
        )
        .expect("prepare");
        let truncated = block_on(super::fs::execute_open(p2_trunc)).expect("truncate succeeds");
        assert_eq!(
            block_on(read(store.filesystem(), held(&truncated), 0, 64))
                .expect("read truncated")
                .len(),
            0
        );
    }
}

#[test]
fn host_share_and_embedded_symlinks_honor_symlink_follow_across_stat_open_and_set_times() {
    use super::preview2::p2fs;
    use helios_hal::fs::{FileSystem as _, FilledWrite as _, PreparedWrite as _};

    let (mut store, host_fs) = memory_host_store_data();
    // Seed /host with target file, subdir, and symlinks to each.
    block_on(async {
        host_fs
            .inner
            .open(
                MemoryHostFileSystem::fs_path("target").unwrap(),
                helios_hal::fs::OpenFlags::CREATE,
                10,
            )
            .await
            .unwrap();
        let write = host_fs
            .inner
            .prepare_write(
                MemoryHostFileSystem::fs_path("target").unwrap(),
                WritePosition::At(0),
                4,
                10,
            )
            .await
            .unwrap();
        write
            .fill(|at, dst| dst.copy_from_slice(&b"host"[at..at + dst.len()]))
            .submit()
            .await
            .unwrap();
        host_fs
            .inner
            .create_directory(MemoryHostFileSystem::fs_path("dir").unwrap(), 10)
            .await
            .unwrap();
        host_fs
            .inner
            .symlink(
                "target",
                MemoryHostFileSystem::fs_path("file-link").unwrap(),
                10,
            )
            .await
            .unwrap();
        host_fs
            .inner
            .symlink(
                "dir",
                MemoryHostFileSystem::fs_path("dir-link").unwrap(),
                10,
            )
            .await
            .unwrap();
    });

    // Seed /emb with target file, subdir, and symlinks to each.
    let ns = store.filesystem().clone();
    block_on(create_directory_at(&ns, held(&root()), "emb", 10)).unwrap();
    block_on(create_directory_at(&ns, held(&root()), "emb/dir", 10)).unwrap();
    create_file(&ns, "emb/target", b"emb!");
    block_on(symlink_at(
        &ns,
        held(&directory_holding("/emb", root().flags)),
        "target",
        "file-link",
        10,
    ))
    .unwrap();
    block_on(symlink_at(
        &ns,
        held(&directory_holding("/emb", root().flags)),
        "dir",
        "dir-link",
        10,
    ))
    .unwrap();

    for (base_path, expected_bytes) in [("/emb", b"emb!".as_slice()), ("/host", b"host".as_slice())]
    {
        let base = directory_holding(base_path, root().flags);

        // stat-at without SYMLINK_FOLLOW reports SymbolicLink; with it reports RegularFile / Directory.
        let link_stat = super::descriptor_stat(
            &block_on(stat_at(
                store.filesystem(),
                held(&base),
                fs_types::PathFlags::empty(),
                "file-link",
            ))
            .expect("lstat symlink"),
        );
        assert!(matches!(
            link_stat.type_,
            fs_types::DescriptorType::SymbolicLink
        ));

        let target_stat = super::descriptor_stat(
            &block_on(stat_at(
                store.filesystem(),
                held(&base),
                fs_types::PathFlags::SYMLINK_FOLLOW,
                "file-link",
            ))
            .expect("stat symlink target"),
        );
        assert!(matches!(
            target_stat.type_,
            fs_types::DescriptorType::RegularFile
        ));
        assert_eq!(target_stat.size, expected_bytes.len() as u64);

        let dir_stat = super::descriptor_stat(
            &block_on(stat_at(
                store.filesystem(),
                held(&base),
                fs_types::PathFlags::SYMLINK_FOLLOW,
                "dir-link",
            ))
            .expect("stat dir symlink target"),
        );
        assert!(matches!(
            dir_stat.type_,
            fs_types::DescriptorType::Directory
        ));

        // open-at without SYMLINK_FOLLOW refuses the symlink with Loop (and CREATE|EXCLUSIVE with Exist).
        let nofollow_err = block_on(open_at(
            store.filesystem(),
            held(&base),
            fs_types::PathFlags::empty(),
            "file-link",
            fs_types::OpenFlags::empty(),
            FsDescriptorFlags::READ,
            1,
        ))
        .err()
        .expect("open-at without symlink-follow must refuse symlink");
        assert!(matches!(nofollow_err, fs_types::ErrorCode::Loop));

        let excl_err = block_on(open_at(
            store.filesystem(),
            held(&base),
            fs_types::PathFlags::empty(),
            "file-link",
            fs_types::OpenFlags::CREATE | fs_types::OpenFlags::EXCLUSIVE,
            FsDescriptorFlags::READ,
            1,
        ))
        .err()
        .expect("create|exclusive on existing symlink must return Exist");
        assert!(matches!(excl_err, fs_types::ErrorCode::Exist));

        // open-at with SYMLINK_FOLLOW opens the target file or directory.
        let opened_file = block_on(open_at(
            store.filesystem(),
            held(&base),
            fs_types::PathFlags::SYMLINK_FOLLOW,
            "file-link",
            fs_types::OpenFlags::empty(),
            FsDescriptorFlags::READ,
            1,
        ))
        .expect("open-at with symlink-follow opens target file");
        assert_eq!(opened_file.kind, FsNodeKind::File);
        assert_eq!(
            block_on(read(store.filesystem(), held(&opened_file), 0, 16))
                .unwrap()
                .as_ref(),
            expected_bytes
        );

        let opened_dir = block_on(open_at(
            store.filesystem(),
            held(&base),
            fs_types::PathFlags::SYMLINK_FOLLOW,
            "dir-link",
            fs_types::OpenFlags::DIRECTORY,
            FsDescriptorFlags::READ,
            1,
        ))
        .expect("open-at with symlink-follow opens target dir");
        assert_eq!(opened_dir.kind, FsNodeKind::Directory);

        // set-times-at without SYMLINK_FOLLOW updates the link itself; with SYMLINK_FOLLOW updates target.
        // Exercise both preview2 and p3 prepare_set_times_at entry points.
        let base_p2 = store.table.push(base.clone()).expect("base fits");
        let p2_nofollow = <wasmtime::component::HasSelf<_> as p2fs::HostDescriptorWithStore<()>>::prepare_set_times_at(
            &mut &mut store,
            base_p2,
            p2fs::PathFlags::empty(),
            "file-link",
            p2fs::NewTimestamp::Timestamp(p2fs::Datetime {
                seconds: 1,
                nanoseconds: 111,
            }),
            p2fs::NewTimestamp::Timestamp(p2fs::Datetime {
                seconds: 2,
                nanoseconds: 222,
            }),
        )
        .expect("prepare")
        .expect("prepared");
        block_on(p2_nofollow.execute()).expect("p2 set-times-at nofollow succeeds");

        let link_meta = block_on(stat_at(
            store.filesystem(),
            held(&base),
            fs_types::PathFlags::empty(),
            "file-link",
        ))
        .unwrap();
        assert_eq!(link_meta.access_nanos, 1_000_000_111);
        assert_eq!(link_meta.modified_nanos, 2_000_000_222);

        let target_before = block_on(stat_at(
            store.filesystem(),
            held(&base),
            fs_types::PathFlags::empty(),
            "target",
        ))
        .unwrap();
        assert_ne!(target_before.modified_nanos, 2_000_000_222);

        let base_p3 = store.table.push(base.clone()).expect("base fits");
        let p3_follow = <wasmtime::component::HasSelf<_> as fs_types::HostDescriptorWithStore<()>>::prepare_set_times_at(
            &mut &mut store,
            base_p3,
            fs_types::PathFlags::SYMLINK_FOLLOW,
            "file-link",
            fs_types::NewTimestamp::Timestamp(fs_types::Instant {
                seconds: 3,
                nanoseconds: 333,
            }),
            fs_types::NewTimestamp::Timestamp(fs_types::Instant {
                seconds: 4,
                nanoseconds: 444,
            }),
        )
        .expect("prepare")
        .expect("prepared");
        block_on(p3_follow.execute()).expect("p3 set-times-at follow succeeds");

        let target_after = block_on(stat_at(
            store.filesystem(),
            held(&base),
            fs_types::PathFlags::empty(),
            "target",
        ))
        .unwrap();
        assert_eq!(target_after.access_nanos, 3_000_000_333);
        assert_eq!(target_after.modified_nanos, 4_000_000_444);

        let link_after = block_on(stat_at(
            store.filesystem(),
            held(&base),
            fs_types::PathFlags::empty(),
            "file-link",
        ))
        .unwrap();
        assert_eq!(link_after.access_nanos, 1_000_000_111);
        assert_eq!(link_after.modified_nanos, 2_000_000_222);
    }
}

#[test]
fn wasi_ignores_unix_metadata_and_inherits_from_parent_across_writable_mounts() {
    use bytes::Bytes;
    use core::num::NonZeroUsize;
    use ferrosys::{
        FormatOptions, Metadata as BtrfsSeedMeta, Timestamp as BtrfsStamp, TreeBuilder, format_to,
    };
    use helios_hal::fs::{DeviceId, FsPath};
    use std::io::{Read as _, Seek as _, SeekFrom};

    use crate::io::{MemoryDisk, cache_over};
    use crate::vfs::{BtrfsFs, BtrfsWriteOptions};
    use crate::{BOOT_DEVICE, EmbeddedBootFs, MountTable, ProcessAuthority, UnixAttributes};

    #[derive(Clone)]
    struct AllMountsStorage;
    impl crate::Storage for AllMountsStorage {
        type HostShare = MemoryHostFileSystem;
        type Disk = MemoryDisk;
    }

    struct RawBytes(alloc::vec::Vec<u8>);
    impl helios_btrfs::BlockSource for RawBytes {
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
        async fn read_at(&self, offset: u64, out: &mut [u8]) -> helios_hal::io::IoResult<()> {
            let start = offset as usize;
            out.copy_from_slice(&self.0[start..start + out.len()]);
            Ok(())
        }
    }

    const BTRFS_BYTES: usize = 128 << 20;
    const PARENTS: [(&str, u16, u32, u32); 3] = [
        ("sgid", 0o2775, 1000, 100),
        ("sticky", 0o1777, 0, 0),
        ("locked", 0o0000, 999, 999),
    ];

    let stamp = BtrfsStamp::from_secs(1_700_000_000);
    let mut btrfs_tree = TreeBuilder::new().root(BtrfsSeedMeta::new(0o755, stamp));
    for (name, mode, uid, gid) in PARENTS {
        btrfs_tree = btrfs_tree.directory(
            alloc::format!("/{name}"),
            BtrfsSeedMeta::new(mode, stamp).owned_by(uid, gid),
        );
    }
    let mut image = tempfile::tempfile().expect("image file");
    image.set_len(BTRFS_BYTES as u64).unwrap();
    format_to(
        &mut image,
        btrfs_tree,
        BTRFS_BYTES as u64,
        FormatOptions::new([0x71; 16], stamp),
    )
    .unwrap();
    image.seek(SeekFrom::Start(0)).unwrap();
    let mut raw_btrfs = alloc::vec::Vec::new();
    image.read_to_end(&mut raw_btrfs).unwrap();
    let btrfs_disk = MemoryDisk::holding_writable("wasi-btrfs", &raw_btrfs);
    let btrfs_cache = cache_over(&btrfs_disk, 256, 1);

    let namespace: Namespace<AllMountsStorage> = Namespace::new(MountTable::new(
        crate::BootFs::new(EmbeddedBootFs::empty(), BOOT_DEVICE),
        1,
    ));
    let root_upper = namespace.root().upper().clone();
    let mem_fs = MemoryFs::new(namespace.table().allocate_device());
    let host_fs = MemoryHostFileSystem::new(namespace.table().allocate_device());

    for fs in [&root_upper, &mem_fs, &host_fs.inner] {
        for (name, mode, uid, gid) in PARENTS {
            fs.create_directory_with(
                FsPath::new(name).unwrap(),
                UnixAttributes {
                    permissions: u32::from(mode),
                    uid,
                    gid,
                },
                10,
            )
            .unwrap();
        }
    }
    // Seed a `0000` file with an ACL on `/mem` to verify WASI ignores both:
    // the mount's rights alone grant or refuse access.
    mem_fs
        .create_file_with(
            FsPath::new("locked/secret").unwrap(),
            UnixAttributes {
                permissions: 0o0000,
                uid: 999,
                gid: 999,
            },
            10,
        )
        .unwrap();
    mem_fs
        .set_xattr(
            FsPath::new("locked/secret").unwrap(),
            "system.posix_acl_access",
            Bytes::from_static(&[2, 0, 0, 0]),
            crate::XattrSetMode::Create,
            10,
        )
        .unwrap();

    let authority = ProcessAuthority::root();
    block_on(namespace.mount(
        &authority.derive_mount_cap("/mem").unwrap(),
        MountedFs::Memory(mem_fs.clone()),
        granted(FileSystemRights::READ | FileSystemRights::WRITE),
    ))
    .unwrap();
    block_on(namespace.mount(
        &authority.derive_mount_cap("/host").unwrap(),
        MountedFs::Host(HostShare::new(host_fs.clone())),
        granted(FileSystemRights::READ | FileSystemRights::WRITE),
    ))
    .unwrap();

    let timer = crate::test_support::test_timer(crate::test_support::TestCpu::without_entropy());
    block_on(async {
        let (btrfs_fs, btrfs_service) = BtrfsFs::mount_writable(
            btrfs_cache.disk(0),
            || DeviceId::new(220),
            BtrfsWriteOptions {
                entropy: crate::test_support::test_root_entropy(),
                concurrency: NonZeroUsize::new(1).unwrap(),
                compress: None,
                commit: None,
            },
        )
        .await
        .unwrap();
        namespace
            .mount(
                &authority.derive_mount_cap("/btrfs").unwrap(),
                MountedFs::Btrfs(btrfs_fs.clone()),
                granted(FileSystemRights::READ | FileSystemRights::WRITE),
            )
            .await
            .unwrap();

        let client = async {
            // Even with mode 0000, uid 999 and an ACL, WASI opens and writes
            // the file on a read-write mount.
            let secret = open_at(
                &namespace,
                held(&root()),
                fs_types::PathFlags::empty(),
                "mem/locked/secret",
                fs_types::OpenFlags::empty(),
                FsDescriptorFlags::READ | FsDescriptorFlags::WRITE,
                20,
            )
            .await
            .expect("WASI ignores stored Unix mode and ACLs");
            write(&namespace, held(&secret), 0, b"granted-by-mount", 21)
                .await
                .expect("WASI writes through mount rights");
            assert_eq!(
                read(&namespace, held(&secret), 0, 64)
                    .await
                    .unwrap()
                    .as_ref(),
                b"granted-by-mount"
            );
            drop(secret);

            // Create a regular file, a directory and a symlink through WASI
            // under each parent across every writable filesystem.
            for prefix in ["", "mem/", "host/", "btrfs/"] {
                for (parent, ..) in PARENTS {
                    let base = alloc::format!("{prefix}{parent}");
                    open_at(
                        &namespace,
                        held(&root()),
                        fs_types::PathFlags::empty(),
                        &alloc::format!("{base}/file"),
                        fs_types::OpenFlags::CREATE | fs_types::OpenFlags::EXCLUSIVE,
                        FsDescriptorFlags::READ | FsDescriptorFlags::WRITE,
                        30,
                    )
                    .await
                    .unwrap_or_else(|err| panic!("WASI create file in {base}: {err:?}"));
                    create_directory_at(
                        &namespace,
                        held(&root()),
                        &alloc::format!("{base}/dir"),
                        30,
                    )
                    .await
                    .unwrap_or_else(|err| panic!("WASI create dir in {base}: {err:?}"));
                    symlink_at(
                        &namespace,
                        held(&root()),
                        "file",
                        &alloc::format!("{base}/link"),
                        30,
                    )
                    .await
                    .unwrap_or_else(|err| panic!("WASI symlink in {base}: {err:?}"));
                }
            }
            namespace
                .unmount(&authority.derive_mount_cap("/btrfs").unwrap())
                .await
                .unwrap();
            btrfs_fs.unmount().await;
            drop(btrfs_fs);
        };
        futures::future::join(client, btrfs_service.run(timer)).await;
    });

    for (label, fs) in [
        ("root", &root_upper),
        ("memory", &mem_fs),
        ("host", &host_fs.inner),
    ] {
        for (parent, mode, uid, gid) in PARENTS {
            let mode = u32::from(mode);
            let check = |leaf: &str, expected: u32| {
                let path = alloc::format!("{parent}/{leaf}");
                assert_eq!(
                    fs.unix_metadata(FsPath::new(&path).unwrap())
                        .unwrap()
                        .attributes,
                    UnixAttributes {
                        permissions: expected,
                        uid,
                        gid,
                    },
                    "{label}:{path}"
                );
            };
            check("file", mode & 0o666);
            check("dir", mode & 0o2777);
            check("link", 0o777);
        }
    }

    let btrfs_view = block_on(helios_btrfs::Filesystem::mount(RawBytes(
        btrfs_disk.bytes_at(0, BTRFS_BYTES),
    )))
    .unwrap();
    for (parent, mode, uid, gid) in PARENTS {
        let mode = u32::from(mode);
        let dir_inode =
            block_on(btrfs_view.resolve(alloc::format!("/{parent}").as_bytes())).unwrap();
        for (leaf, expected) in [
            ("file", mode & 0o666),
            ("dir", mode & 0o2777),
            ("link", 0o777),
        ] {
            let inode = block_on(btrfs_view.lookup(&dir_inode, leaf.as_bytes()))
                .unwrap()
                .unwrap();
            let meta = *inode.metadata();
            assert_eq!(
                (meta.permissions, meta.uid, meta.gid),
                (expected, uid, gid),
                "btrfs:{parent}/{leaf}"
            );
        }
    }
}

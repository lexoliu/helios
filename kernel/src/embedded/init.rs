use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use helios_artifact::bootfs::{self, BootfsError, EntryKind};
use helios_hal::boot::{BootModule, BootModules};

use crate::{EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs, EmbeddedComponent};

/// The file name every boot path publishes the payload under — the
/// Limine entry's `module_path`, the hosted `--bootfs` argument.
const BOOTFS_MODULE_NAME: &[u8] = b"helios-bootfs";

const EMBEDDED_SYSTEM_COMPONENT_PATH: &str = "bin/debugger";

/// The user payload a backend handed the kernel, parsed into the views
/// the rest of the runtime reads.
///
/// The payload arrives as a boot module — a Limine module on the
/// bare-metal targets, an initrd on riscv64, a mapped file on hosted —
/// rather than linked into the kernel image, so the kernel binary does
/// not change when the boot programs do.
#[derive(Clone, Copy)]
pub struct BootPayload {
    init: EmbeddedInit,
}

/// The boot-time contents of a [`BootPayload`]: the init component, its
/// `argv0`, and the boot filesystem view — the runtime shape of what the
/// generated `EmbeddedInitDescriptor` used to describe.
#[derive(Clone, Copy)]
pub struct EmbeddedInit {
    component: EmbeddedComponent,
    argv0: &'static str,
    bootfs: EmbeddedBootFs,
}

impl EmbeddedInit {
    pub const fn component(&self) -> EmbeddedComponent {
        self.component
    }

    pub const fn argv0(&self) -> &'static str {
        self.argv0
    }

    pub const fn bootfs(&self) -> EmbeddedBootFs {
        self.bootfs
    }

    pub fn boot_component(&self, path: &str) -> Option<EmbeddedComponent> {
        self.bootfs
            .files()
            .iter()
            .find(|file| file.path() == path)
            .map(|file| EmbeddedComponent::new(file.path(), file.contents()))
    }
}

impl BootPayload {
    /// Parses the payload image a backend loaded.
    ///
    /// The wire format's entry table records path and data as
    /// offset-length pairs into its own sections, while the kernel's
    /// view types hand out `&'static str` and `&'static [u8]` — the one
    /// kernel-heap allocation a payload costs is materialising those two
    /// index slices here, at bring-up. File contents stay borrowed from
    /// the image itself.
    pub fn parse(bytes: &'static [u8]) -> Result<Self, BootfsError> {
        let image = bootfs::Image::parse(bytes)?;
        let init_component = image
            .entry_of_kind(EntryKind::InitComponent)
            .expect("a parsed image carries exactly one init-component entry");
        let init_argv0 = image
            .entry_of_kind(EntryKind::InitArgv0)
            .expect("a parsed image carries exactly one init-argv0 entry");

        let mut directories = Vec::new();
        let mut files = Vec::new();
        for entry in image.entries() {
            match entry.kind() {
                EntryKind::Directory => directories.push(EmbeddedBootDirectory::new(
                    entry.path(),
                    entry.modified_nanos(),
                )),
                EntryKind::File => files.push(EmbeddedBootFile::new(
                    entry.path(),
                    entry.data(),
                    entry.modified_nanos(),
                )),
                EntryKind::InitComponent | EntryKind::InitArgv0 => {}
            }
        }
        Ok(Self {
            init: EmbeddedInit {
                component: EmbeddedComponent::new(init_component.path(), init_component.data()),
                argv0: init_argv0.path(),
                bootfs: EmbeddedBootFs::new(
                    Box::leak(directories.into_boxed_slice()),
                    Box::leak(files.into_boxed_slice()),
                ),
            },
        })
    }

    /// Parses the payload out of the modules the bootloader handed
    /// over: the one whose path ends in `helios-bootfs`.
    ///
    /// `BootModule::address` must already be a readable pointer — under
    /// Limine it is an HHDM address, so the payload needs no mapping of
    /// its own. Panics naming every module the bootloader did load when
    /// none is the payload, and naming both when two claim the payload
    /// name: a kernel booted without one has no init to run and nothing
    /// honest to continue with.
    pub fn from_boot_modules<'a>(modules: &impl BootModules<'a>) -> Self {
        let mut found: Option<BootModule<'a>> = None;
        let mut names = String::new();
        for module in modules.modules() {
            if !names.is_empty() {
                names.push_str(", ");
            }
            write!(names, "{}", String::from_utf8_lossy(module.path).as_ref())
                .expect("writing to a String cannot fail");
            let is_bootfs = module
                .path
                .rsplit(|&byte| byte == b'/')
                .next()
                .is_some_and(|name| name == BOOTFS_MODULE_NAME);
            if is_bootfs {
                if let Some(first) = &found {
                    panic!(
                        "two helios-bootfs modules among boot modules: {} and {}",
                        String::from_utf8_lossy(first.path).as_ref(),
                        String::from_utf8_lossy(module.path).as_ref(),
                    );
                }
                found = Some(module);
            }
        }
        let module = found
            .unwrap_or_else(|| panic!("no helios-bootfs module among boot modules: [{names}]"));
        // SAFETY: the boot protocol maps each module's `address`/`size`
        // readable for the kernel's lifetime.
        let bytes =
            unsafe { core::slice::from_raw_parts(module.address as *const u8, module.size) };
        Self::parse(bytes)
            .unwrap_or_else(|error| panic!("helios-bootfs module is not a valid payload: {error}"))
    }

    /// The boot-time view the payload carries.
    pub fn embedded_init(&self) -> EmbeddedInit {
        self.init
    }

    /// The boot filesystem the payload carries.
    pub fn bootfs(&self) -> EmbeddedBootFs {
        self.init.bootfs()
    }

    /// The bootfs file at `path` as a component, when the payload
    /// carries it.
    pub fn boot_component(&self, path: &str) -> Option<EmbeddedComponent> {
        self.init.boot_component(path)
    }

    /// The embedded debugger system component, when the
    /// `embedded-debugger` feature is enabled and the payload's bootfs
    /// carries it. Kernels built without the feature never autostart a
    /// system component.
    pub fn system_component(&self) -> Option<EmbeddedComponent> {
        if cfg!(feature = "embedded-debugger") {
            self.boot_component(EMBEDDED_SYSTEM_COMPONENT_PATH)
        } else {
            None
        }
    }

    /// Whether this payload carries a system component this build would
    /// autostart.
    pub fn has_system_component(&self) -> bool {
        self.system_component().is_some()
    }
}

#[cfg(test)]
impl BootPayload {
    /// A minimal valid payload for tests that need a [`RuntimeState`]
    /// but do not exercise the bootfs.
    ///
    /// [`RuntimeState`]: crate::RuntimeState
    pub(crate) fn for_tests() -> Self {
        let image = bootfs::write_image(&[
            bootfs::WriteEntry {
                kind: EntryKind::InitComponent,
                path: "init",
                data: b"",
                modified_nanos: 0,
            },
            bootfs::WriteEntry {
                kind: EntryKind::InitArgv0,
                path: "init",
                data: b"",
                modified_nanos: 0,
            },
        ]);
        Self::parse(Box::leak(image.into_boxed_slice())).expect("test payload must parse")
    }
}

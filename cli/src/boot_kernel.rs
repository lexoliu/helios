//! The kernel image as a boot disk carries it: the linked ELF without its
//! DWARF.
//!
//! The optimised profiles link the kernel with debuginfo so the inspector
//! can expand a sampled address into its inline chain (#451). Limine reads
//! the whole executable file into guest memory and the kernel reserves
//! that copy for its lifetime, so DWARF on the boot disk would cost the
//! guest tens of megabytes it never reads. The disk gets the image with
//! every `.debug_*` section removed, and the host keeps the full file to
//! symbolize against.
//!
//! The rewrite goes through `object`'s ELF builder, which re-serialises
//! the dynamic tables a position-independent image carries rather than
//! copying their bytes. [`without_debuginfo`] therefore checks that every
//! program header and every allocated section of the result is the one it
//! read, byte for byte, and refuses to hand back an image whose loaded
//! contents differ from the file the host symbolizes.

use std::path::Path;

use object::build::elf::Builder;
use object::elf::SHF_ALLOC;
use object::pod::bytes_of_slice;
use object::read::elf::{ElfFile64, SectionHeader as _};
use object::{Endianness, Object as _, ObjectSection as _};

/// Why the kernel image could not be written without its DWARF.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BootKernelError {
    #[error("failed to read kernel image {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse kernel image {path} as a 64-bit ELF: {source}")]
    Parse {
        path: String,
        #[source]
        source: object::read::Error,
    },
    #[error("failed to rewrite kernel image {path} without its DWARF: {source}")]
    Rewrite {
        path: String,
        #[source]
        source: object::build::Error,
    },
    #[error("kernel image {path} has an allocated debug section {section}")]
    AllocatedDebugSection { path: String, section: String },
    #[error(
        "rewriting kernel image {path} without its DWARF changed {what}; the boot disk \
         would load an image other than the one the host symbolizes"
    )]
    LoadedImageChanged { path: String, what: String },
}

/// The bytes of the kernel image at `path` with every `.debug_*` section
/// removed and everything it loads unchanged.
pub(crate) fn without_debuginfo(path: &Path) -> Result<Vec<u8>, BootKernelError> {
    let original = std::fs::read(path).map_err(|source| BootKernelError::Read {
        path: path.display().to_string(),
        source,
    })?;
    strip(path, &original)
}

/// `original`, read from `path`, with every `.debug_*` section removed.
fn strip(path: &Path, original: &[u8]) -> Result<Vec<u8>, BootKernelError> {
    let display = || path.display().to_string();
    let mut builder = Builder::read64(original).map_err(|source| BootKernelError::Rewrite {
        path: display(),
        source,
    })?;
    for section in builder.sections.iter_mut() {
        if section.name.starts_with(b".debug_") {
            if section.sh_flags & u64::from(SHF_ALLOC) != 0 {
                return Err(BootKernelError::AllocatedDebugSection {
                    path: display(),
                    section: section.name.to_string(),
                });
            }
            section.delete = true;
        }
    }
    let mut stripped = Vec::new();
    builder
        .write(&mut stripped)
        .map_err(|source| BootKernelError::Rewrite {
            path: display(),
            source,
        })?;
    let parse = |bytes| {
        ElfFile64::<Endianness>::parse(bytes).map_err(|source| BootKernelError::Parse {
            path: display(),
            source,
        })
    };
    let changed = |what: String| BootKernelError::LoadedImageChanged {
        path: display(),
        what,
    };
    let (before, after) = (parse(original)?, parse(&stripped)?);
    if bytes_of_slice(before.elf_program_headers()) != bytes_of_slice(after.elf_program_headers()) {
        return Err(changed("its program headers".to_owned()));
    }
    let parsed = |source| BootKernelError::Parse {
        path: display(),
        source,
    };
    let (sections_before, sections_after) = (
        allocated_sections(&before).map_err(parsed)?,
        allocated_sections(&after).map_err(parsed)?,
    );
    if sections_before.len() != sections_after.len() {
        return Err(changed("its set of allocated sections".to_owned()));
    }
    for (old, new) in sections_before.iter().zip(&sections_after) {
        if old != new {
            return Err(changed(format!("allocated section {}", old.name)));
        }
    }
    Ok(stripped)
}

/// What the loader sees of one `SHF_ALLOC` section.
#[derive(Debug, PartialEq, Eq)]
struct AllocatedSection<'data> {
    name: &'data str,
    address: u64,
    size: u64,
    /// The file bytes; empty for `SHT_NOBITS`.
    data: &'data [u8],
}

/// Every `SHF_ALLOC` section of `image`, in section-table order.
fn allocated_sections<'data>(
    image: &ElfFile64<'data, Endianness>,
) -> Result<Vec<AllocatedSection<'data>>, object::read::Error> {
    image
        .sections()
        .filter(|section| {
            section.elf_section_header().sh_flags(image.endian()) & u64::from(SHF_ALLOC) != 0
        })
        .map(|section| {
            Ok(AllocatedSection {
                name: section.name()?,
                address: section.address(),
                size: section.size(),
                data: section.data()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use object::build::elf::{Builder, SectionData};
    use object::elf::{
        EM_X86_64, ET_DYN, PF_R, PF_X, SHF_ALLOC, SHF_EXECINSTR, SHT_PROGBITS, SHT_STRTAB,
    };
    use object::{Endianness, Object as _, ObjectSection as _, ObjectSegment as _};

    use std::path::Path;

    use super::strip;

    /// A position-independent image with one executable segment holding
    /// `.text`, and a `.debug_info` and a `.comment` the loader never sees.
    fn image() -> Vec<u8> {
        let mut builder = Builder::new(Endianness::Little, true);
        builder.header.e_type = ET_DYN;
        builder.header.e_machine = EM_X86_64;
        builder.header.e_phoff = 0x40;
        builder.load_align = 0x1000;
        let names = builder.sections.add();
        names.name = b".shstrtab"[..].into();
        names.sh_type = SHT_STRTAB;
        names.data = SectionData::SectionString;
        let text = builder.sections.add();
        text.name = b".text"[..].into();
        text.sh_type = SHT_PROGBITS;
        text.sh_flags = u64::from(SHF_ALLOC | SHF_EXECINSTR);
        text.sh_addralign = 16;
        text.data = SectionData::Data(vec![0xc3; 0x40].into());
        text.sh_size = 0x40;
        // The segment starts past the file and program headers, as a
        // linker places it.
        text.sh_offset = 0x1000;
        text.sh_addr = 0x1000;
        let segment = builder.segments.add();
        segment.p_type = object::elf::PT_LOAD;
        segment.p_flags = PF_R | PF_X;
        segment.p_align = 0x1000;
        segment.append_section_range(text);
        segment.sections.push(text.id());
        for (name, contents) in [(".debug_info", &b"dwarf"[..]), (".comment", b"lld")] {
            let section = builder.sections.add();
            section.name = name.as_bytes().into();
            section.sh_type = SHT_PROGBITS;
            section.sh_addralign = 1;
            section.data = SectionData::Data(contents.to_vec().into());
        }
        let mut bytes = Vec::new();
        builder
            .write(&mut bytes)
            .expect("the test image is well formed");
        bytes
    }

    #[test]
    fn the_boot_image_drops_dwarf_and_keeps_every_loaded_byte() {
        let stripped = strip(Path::new("helios"), &image()).expect("the test image strips");

        let image = object::File::parse(&*stripped).expect("the stripped image parses");
        assert!(image.section_by_name(".debug_info").is_none());
        assert!(image.section_by_name(".comment").is_some());
        let text = image.section_by_name(".text").expect(".text survives");
        assert_eq!(text.address(), 0x1000);
        assert_eq!(text.data().expect(".text has bytes"), &[0xc3; 0x40][..]);
        let segments = image.segments().collect::<Vec<_>>();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].address(), 0x1000);
        assert_eq!(segments[0].size(), 0x40);
    }
}

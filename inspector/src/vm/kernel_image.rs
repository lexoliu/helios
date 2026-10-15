//! The kernel image's own function symbols, and the map from a runtime
//! address back onto them.
//!
//! Two readers walk the same population: the profile-use build counts
//! the image's `STT_FUNC` symbols as the denominator of its uncovered
//! list, and a capture of a wedged guest names the function each vCPU
//! was executing. Both go through [`function_symbols`], so they cannot
//! disagree about what a function is.

use std::ops::Range;
use std::path::Path;

use object::elf::PF_X;
use object::{Object as _, ObjectSegment as _, ObjectSymbol as _, SegmentFlags, SymbolKind};

/// One function the image defines, as its symbol table records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FunctionSymbol<'data> {
    pub(crate) name: &'data str,
    /// The link-time virtual address of the function's first byte.
    pub(crate) address: u64,
    pub(crate) size: u64,
}

/// Every defined `STT_FUNC` symbol of `image`.
pub(crate) fn function_symbols<'data, 'file>(
    image: &'file object::File<'data>,
) -> impl Iterator<Item = Result<FunctionSymbol<'data>, object::Error>> + 'file {
    image
        .symbols()
        .filter(|symbol| symbol.kind() == SymbolKind::Text && !symbol.is_undefined())
        .map(|symbol| {
            Ok(FunctionSymbol {
                name: symbol.name()?,
                address: symbol.address(),
                size: symbol.size(),
            })
        })
}

/// The bytes of the kernel image at `path`.
pub(crate) fn read_image(path: &Path) -> Result<Vec<u8>, KernelSymbolsError> {
    std::fs::read(path).map_err(|source| KernelSymbolsError::Read {
        path: path.display().to_string(),
        source,
    })
}

/// `bytes`, read from `path`, parsed as an object file.
pub(crate) fn parse_image<'data>(
    path: &Path,
    bytes: &'data [u8],
) -> Result<object::File<'data>, KernelSymbolsError> {
    object::File::parse(bytes).map_err(|source| KernelSymbolsError::Symbols {
        path: path.display().to_string(),
        source,
    })
}

/// Why the kernel image could not be turned into a symbol table.
#[derive(Debug, thiserror::Error)]
pub(crate) enum KernelSymbolsError {
    #[error("failed to read kernel image {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read the symbols of kernel image {path}: {source}")]
    Symbols {
        path: String,
        #[source]
        source: object::Error,
    },
    #[error("kernel image {path} has no loadable segment to take its link base from")]
    NoLoadSegment { path: String },
    #[error("kernel image {path} has no executable loadable segment")]
    NoExecutableSegment { path: String },
    #[error("kernel image {path} has a segment carrying {flags:?} rather than ELF flags")]
    NotElfSegment { path: String, flags: SegmentFlags },
    #[error(
        "kernel image {path} has an executable segment of {size:#x} bytes at {address:#x}, \
         which runs past the end of the address space"
    )]
    SegmentOverflow {
        path: String,
        address: u64,
        size: u64,
    },
}

/// A function of the image, owned so the table outlives the file bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Function {
    pub(crate) name: String,
    pub(crate) address: u64,
    pub(crate) size: u64,
}

/// The image's functions, ordered by address, the address the image was
/// linked to start at, and the link-time ranges of its executable
/// segments.
#[derive(Debug)]
pub(crate) struct KernelSymbols {
    /// The lowest `PT_LOAD` virtual address: the address the bootloader's
    /// reported virtual base corresponds to.
    link_base: u64,
    /// Every executable `PT_LOAD` segment, `p_vaddr..p_vaddr + p_memsz`:
    /// the addresses a return address into the kernel can hold.
    text: Vec<Range<u64>>,
    /// Sorted by `address`. A zero-sized symbol contains no address, so
    /// none is kept.
    functions: Vec<Function>,
}

impl KernelSymbols {
    /// Reads the function symbols and the link base out of the ELF at
    /// `path`.
    pub(crate) fn read(path: &Path) -> Result<Self, KernelSymbolsError> {
        let bytes = read_image(path)?;
        Self::from_image(path, &parse_image(path, &bytes)?)
    }

    /// The function symbols and the link base of an already parsed image;
    /// `path` names it in errors.
    pub(crate) fn from_image(
        path: &Path,
        image: &object::File<'_>,
    ) -> Result<Self, KernelSymbolsError> {
        let display = || path.display().to_string();
        let symbols = |source| KernelSymbolsError::Symbols {
            path: display(),
            source,
        };
        // `object` yields only `PT_LOAD` entries as an ELF's segments.
        let link_base = image
            .segments()
            .map(|segment| segment.address())
            .min()
            .ok_or_else(|| KernelSymbolsError::NoLoadSegment { path: display() })?;
        let mut text = Vec::new();
        for segment in image.segments() {
            let executable = match segment.flags() {
                SegmentFlags::Elf { p_flags } => p_flags & PF_X != 0,
                flags => {
                    return Err(KernelSymbolsError::NotElfSegment {
                        path: display(),
                        flags,
                    });
                }
            };
            if executable {
                let (address, size) = (segment.address(), segment.size());
                let end = address.checked_add(size).ok_or_else(|| {
                    KernelSymbolsError::SegmentOverflow {
                        path: display(),
                        address,
                        size,
                    }
                })?;
                text.push(address..end);
            }
        }
        if text.is_empty() {
            return Err(KernelSymbolsError::NoExecutableSegment { path: display() });
        }
        let functions = function_symbols(image)
            .map(|symbol| {
                symbol.map(|symbol| Function {
                    name: symbol.name.to_owned(),
                    address: symbol.address,
                    size: symbol.size,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(symbols)?;
        Ok(Self::new(link_base, text, functions))
    }

    /// A table from its parts, all at link-time addresses.
    pub(crate) fn new(link_base: u64, text: Vec<Range<u64>>, mut functions: Vec<Function>) -> Self {
        functions.retain(|function| function.size != 0);
        functions.sort_by_key(|function| function.address);
        Self {
            link_base,
            text,
            functions,
        }
    }

    /// The table at the addresses the image was linked to: what a reader of
    /// the ELF file itself, rather than of a running guest, symbolizes.
    pub(crate) fn linked(&self) -> LoadedKernelSymbols<'_> {
        self.loaded_at(self.link_base)
    }

    /// The table as seen from a kernel the bootloader placed at
    /// `virtual_base`.
    pub(crate) fn loaded_at(&self, virtual_base: u64) -> LoadedKernelSymbols<'_> {
        LoadedKernelSymbols {
            symbols: self,
            slide: virtual_base.wrapping_sub(self.link_base),
        }
    }
}

/// A symbol table shifted by the distance between where the image was
/// linked and where it runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LoadedKernelSymbols<'table> {
    symbols: &'table KernelSymbols,
    /// Runtime address minus link address, modulo 2^64: a kernel loaded
    /// below its link base slides by a negative distance, and wrapping
    /// arithmetic undoes either direction.
    slide: u64,
}

/// A runtime address named by the function containing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolizedAddress {
    /// The demangled name, without the crate hash.
    pub(crate) function: String,
    pub(crate) offset: u64,
}

impl core::fmt::Display for SymbolizedAddress {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}+{:#x}", self.function, self.offset)
    }
}

impl LoadedKernelSymbols<'_> {
    /// Whether the runtime address `address` lies in one of the image's
    /// executable segments: the only addresses a return address into the
    /// kernel can hold.
    pub(crate) fn contains_text(&self, address: u64) -> bool {
        let link_address = address.wrapping_sub(self.slide);
        self.symbols
            .text
            .iter()
            .any(|segment| segment.contains(&link_address))
    }

    /// The function whose bytes contain the runtime address `address`.
    pub(crate) fn symbolize(&self, address: u64) -> Option<SymbolizedAddress> {
        let link_address = address.wrapping_sub(self.slide);
        let functions = &self.symbols.functions;
        let after = functions.partition_point(|function| function.address <= link_address);
        let function = functions[..after].last()?;
        let offset = link_address - function.address;
        (offset < function.size).then(|| SymbolizedAddress {
            function: format!("{:#}", rustc_demangle::demangle(&function.name)),
            offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Function, KernelSymbols, SymbolizedAddress};

    fn function(name: &str, address: u64, size: u64) -> Function {
        Function {
            name: name.to_owned(),
            address,
            size,
        }
    }

    /// A small image linked at the canonical higher-half base, with a
    /// v0-mangled Rust function, a legacy-mangled one, a C symbol and a
    /// zero-sized label inside the first function.
    fn table() -> KernelSymbols {
        let text = 0xffff_ffff_8000_1000..0xffff_ffff_8000_2080;
        KernelSymbols::new(
            0xffff_ffff_8000_0000,
            vec![text],
            vec![
                function("memcpy", 0xffff_ffff_8000_2000, 0x80),
                function(
                    "_ZN10helios_x863smp12wait_for_ack17h0123456789abcdefE",
                    0xffff_ffff_8000_1100,
                    0x40,
                ),
                function(
                    "_RNvNtCs1234_13helios_kernel4exec4park",
                    0xffff_ffff_8000_1000,
                    0xc0,
                ),
                function("label_inside_park", 0xffff_ffff_8000_1010, 0),
            ],
        )
    }

    #[test]
    fn the_slide_is_the_runtime_base_minus_the_link_base() {
        let table = table();
        let loaded = table.loaded_at(0xffff_ffff_8120_0000);
        assert_eq!(loaded.slide, 0x120_0000);
        // A base below the link address slides back down.
        let below = table.loaded_at(0xffff_ffff_7fe0_0000);
        assert_eq!(
            below.symbolize(0xffff_ffff_7fe0_1004),
            Some(SymbolizedAddress {
                function: "helios_kernel::exec::park".to_owned(),
                offset: 4,
            })
        );
    }

    #[test]
    fn a_runtime_address_names_the_function_that_contains_it() {
        let table = table();
        let loaded = table.loaded_at(0xffff_ffff_8120_0000);
        let park = loaded
            .symbolize(0xffff_ffff_8120_101c)
            .expect("inside park, past the zero-sized label");
        assert_eq!(park.to_string(), "helios_kernel::exec::park+0x1c");
        let ack = loaded
            .symbolize(0xffff_ffff_8120_1100)
            .expect("the first byte of wait_for_ack");
        assert_eq!(ack.to_string(), "helios_x86::smp::wait_for_ack+0x0");
        let copy = loaded
            .symbolize(0xffff_ffff_8120_207f)
            .expect("the last byte of memcpy");
        assert_eq!(copy.to_string(), "memcpy+0x7f");
    }

    #[test]
    fn an_address_outside_every_function_is_unnamed() {
        let table = table();
        let loaded = table.loaded_at(0xffff_ffff_8120_0000);
        // The gap between park's end and wait_for_ack's start.
        assert_eq!(loaded.symbolize(0xffff_ffff_8120_1100 - 1), None);
        // One past memcpy, the last function.
        assert_eq!(loaded.symbolize(0xffff_ffff_8120_2080), None);
        // Below the first function.
        assert_eq!(loaded.symbolize(0xffff_ffff_8120_0000), None);
    }

    #[test]
    fn only_an_address_inside_an_executable_segment_is_text() {
        let table = KernelSymbols::new(
            0xffff_ffff_8000_0000,
            vec![
                0xffff_ffff_8000_1000..0xffff_ffff_8000_2000,
                0xffff_ffff_8000_8000..0xffff_ffff_8000_9000,
            ],
            Vec::new(),
        );
        let loaded = table.loaded_at(0xffff_ffff_8120_0000);
        // Both segments, shifted by the slide, at their first and last
        // byte.
        assert!(loaded.contains_text(0xffff_ffff_8120_1000));
        assert!(loaded.contains_text(0xffff_ffff_8120_1fff));
        assert!(loaded.contains_text(0xffff_ffff_8120_8800));
        // The gap between them, one past each, and the unslid address.
        assert!(!loaded.contains_text(0xffff_ffff_8120_2000));
        assert!(!loaded.contains_text(0xffff_ffff_8120_9000));
        assert!(!loaded.contains_text(0xffff_ffff_8000_1000));
        // A small integer and a user address are never text.
        assert!(!loaded.contains_text(0x246));
        assert!(!loaded.contains_text(0x0000_7f00_0010_2030));
    }
}

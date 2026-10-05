//! The kernel image's own function symbols, and the map from a runtime
//! address back onto them.
//!
//! Two readers walk the same population: the profile-use build counts
//! the image's `STT_FUNC` symbols as the denominator of its uncovered
//! list, and a capture of a wedged guest names the function each vCPU
//! was executing. Both go through [`function_symbols`], so they cannot
//! disagree about what a function is.

use std::path::Path;

use object::{Object as _, ObjectSegment as _, ObjectSymbol as _, SymbolKind};

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
}

/// A function of the image, owned so the table outlives the file bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Function {
    name: String,
    address: u64,
    size: u64,
}

/// The image's functions, ordered by address, and the address the image
/// was linked to start at.
#[derive(Debug)]
pub(crate) struct KernelSymbols {
    /// The lowest `PT_LOAD` virtual address: the address the bootloader's
    /// reported virtual base corresponds to.
    link_base: u64,
    /// Sorted by `address`. A zero-sized symbol contains no address, so
    /// none is kept.
    functions: Vec<Function>,
}

impl KernelSymbols {
    /// Reads the function symbols and the link base out of the ELF at
    /// `path`.
    pub(crate) fn read(path: &Path) -> Result<Self, KernelSymbolsError> {
        let display = || path.display().to_string();
        let bytes = std::fs::read(path).map_err(|source| KernelSymbolsError::Read {
            path: display(),
            source,
        })?;
        let symbols = |source| KernelSymbolsError::Symbols {
            path: display(),
            source,
        };
        let image = object::File::parse(&*bytes).map_err(symbols)?;
        // `object` yields only `PT_LOAD` entries as an ELF's segments.
        let link_base = image
            .segments()
            .map(|segment| segment.address())
            .min()
            .ok_or_else(|| KernelSymbolsError::NoLoadSegment { path: display() })?;
        let functions = function_symbols(&image)
            .map(|symbol| {
                symbol.map(|symbol| Function {
                    name: symbol.name.to_owned(),
                    address: symbol.address,
                    size: symbol.size,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(symbols)?;
        Ok(Self::new(link_base, functions))
    }

    fn new(link_base: u64, mut functions: Vec<Function>) -> Self {
        functions.retain(|function| function.size != 0);
        functions.sort_by_key(|function| function.address);
        Self {
            link_base,
            functions,
        }
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
        KernelSymbols::new(
            0xffff_ffff_8000_0000,
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
}

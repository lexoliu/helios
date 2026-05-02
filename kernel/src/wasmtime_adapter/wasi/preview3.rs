use helios_hal::cpu::Cpu;
use wasmtime::Result;
use wasmtime::component::{HasSelf, Linker};

use super::WasiImportSet;
use super::bindings as wasi;
use crate::wasmtime_adapter::component_host::StoreData;

#[cfg(test)]
pub(crate) const LINKED_INTERFACES: &[&str] = &[
    "wasi:clocks/monotonic-clock",
    "wasi:clocks/system-clock",
    "wasi:cli/environment",
    "wasi:cli/exit",
    "wasi:cli/stdin",
    "wasi:cli/stdout",
    "wasi:cli/stderr",
    "wasi:cli/terminal-input",
    "wasi:cli/terminal-output",
    "wasi:cli/terminal-stdin",
    "wasi:cli/terminal-stdout",
    "wasi:cli/terminal-stderr",
    "wasi:random/random",
    "wasi:random/insecure",
    "wasi:random/insecure-seed",
    "wasi:filesystem/types",
    "wasi:filesystem/preopens",
    "wasi:sockets/types",
    "wasi:sockets/ip-name-lookup",
];

#[cfg(test)]
pub(crate) const WIT_PACKAGES: &[(&str, &str)] = &[
    ("wasi:cli", include_str!("../../../../wit/deps/cli.wit")),
    (
        "wasi:clocks",
        include_str!("../../../../wit/deps/clocks.wit"),
    ),
    (
        "wasi:filesystem",
        include_str!("../../../../wit/deps/filesystem.wit"),
    ),
    (
        "wasi:random",
        include_str!("../../../../wit/deps/random.wit"),
    ),
    (
        "wasi:sockets",
        include_str!("../../../../wit/deps/sockets.wit"),
    ),
];

pub(crate) fn add_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
    imports: &WasiImportSet,
) -> Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if imports.has("wasi:clocks/monotonic-clock", "0.3") {
        wasi::clocks::monotonic_clock::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:clocks/system-clock", "0.3") {
        wasi::clocks::system_clock::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/environment", "0.3") {
        wasi::cli::environment::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/exit", "0.3") {
        wasi::cli::exit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            &Default::default(),
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stdin", "0.3") {
        wasi::cli::stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stdout", "0.3") {
        wasi::cli::stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/stderr", "0.3") {
        wasi::cli::stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-input", "0.3") {
        wasi::cli::terminal_input::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-output", "0.3") {
        wasi::cli::terminal_output::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-stdin", "0.3") {
        wasi::cli::terminal_stdin::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-stdout", "0.3") {
        wasi::cli::terminal_stdout::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:cli/terminal-stderr", "0.3") {
        wasi::cli::terminal_stderr::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:random/random", "0.3") {
        wasi::random::random::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:random/insecure", "0.3") {
        wasi::random::insecure::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:random/insecure-seed", "0.3") {
        wasi::random::insecure_seed::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:filesystem/types", "0.3") {
        wasi::filesystem::types::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:filesystem/preopens", "0.3") {
        wasi::filesystem::preopens::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:sockets/types", "0.3") {
        wasi::sockets::types::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    if imports.has("wasi:sockets/ip-name-lookup", "0.3") {
        wasi::sockets::ip_name_lookup::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(
            linker,
            |state| state,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::WIT_PACKAGES;

    #[test]
    fn preview3_wit_packages_use_expected_rc_version() {
        for (package, wit) in WIT_PACKAGES {
            let expected = {
                let mut expected = String::from("package ");
                expected.push_str(package);
                expected.push_str("@0.3.0-rc-2026-03-15;");
                expected
            };
            assert!(
                wit.lines().any(|line| line.trim() == expected),
                "preview3 WIT package {package} does not declare @0.3.0-rc-2026-03-15"
            );
        }
    }
}

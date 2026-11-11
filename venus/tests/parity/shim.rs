//! Mesa's generator and the C entry points over its output.
//!
//! The pinned `vn_protocol.py` generates the driver and the renderer
//! headers (through `uv`, with the Mako version the pin was tested with);
//! each side is compiled into one shared library together with entry points
//! rendered from `templates/parity_{driver,renderer}.c`, and loaded here.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;

use askama::Template;
use helios_venus_gen::analysis::Analysis;
use helios_venus_gen::inputs::Inputs;
use libloading::Library;

/// The Mako release `vn_protocol.py` is run with. Mesa's CI installs Mako
/// without a pin; this is the release the output was checked against.
const MAKO: &str = "mako==1.4.3";

/// The host the test binary runs on, set by `build.rs`.
const HOST: &str = env!("HELIOS_VENUS_HOST_TARGET");

#[derive(Debug, thiserror::Error)]
pub enum ShimError {
    #[error("{what}: {source}")]
    Io {
        what: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{command} failed ({status}):\n{output}")]
    Command {
        command: String,
        status: String,
        output: String,
    },
    #[error("cannot render the C entry points: {0}")]
    Template(#[from] askama::Error),
    #[error("{0}")]
    Compiler(String),
    #[error("cannot load {path}: {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },
    #[error("{library} has no entry point {symbol}: {source}")]
    Symbol {
        library: &'static str,
        symbol: String,
        #[source]
        source: libloading::Error,
    },
}

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> ShimError {
    let what = what.into();
    move |source| ShimError::Io { what, source }
}

struct DriverCommand {
    name: String,
    args: Vec<String>,
}

struct RendererCommand {
    name: String,
    fields: Vec<String>,
    has_return: bool,
}

#[derive(Template)]
#[template(path = "parity_driver.c", escape = "none")]
struct DriverSource<'a> {
    commands: &'a [DriverCommand],
}

#[derive(Template)]
#[template(path = "parity_renderer.c", escape = "none")]
struct RendererSource<'a> {
    commands: &'a [RendererCommand],
}

/// How a driver entry point receives one parameter out of `void **a`.
fn driver_arg(an: &Analysis, var: &helios_venus_gen::registry::Variable, index: usize) -> String {
    if var.ty.is_static_array() {
        // The slot is the array, which decays to a pointer.
        format!("a[{index}]")
    } else if var.ty.is_pointer() {
        format!("*(void **)a[{index}]")
    } else {
        let name = &an.reg.types[var.ty.base].name;
        format!("*({name} *)a[{index}]")
    }
}

fn commands(an: &Analysis) -> (Vec<DriverCommand>, Vec<RendererCommand>) {
    let mut driver = Vec::new();
    let mut renderer = Vec::new();
    for &id in &an.generated_commands {
        let ty = &an.reg.types[id];
        driver.push(DriverCommand {
            name: ty.name.clone(),
            args: ty
                .variables
                .iter()
                .enumerate()
                .map(|(i, var)| driver_arg(an, var, i))
                .collect(),
        });
        renderer.push(RendererCommand {
            name: ty.name.clone(),
            fields: ty.variables.iter().map(|v| v.name.clone()).collect(),
            has_return: ty.ret.is_some(),
        });
    }
    (driver, renderer)
}

fn run(command: &mut Command) -> Result<(), ShimError> {
    // The program and its arguments only: the command's `Debug` output would
    // include the whole environment.
    let text = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let output = command
        .output()
        .map_err(io(format!("cannot start {text}")))?;
    if output.status.success() {
        return Ok(());
    }
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Err(ShimError::Command {
        command: text,
        status: output.status.to_string(),
        output: combined,
    })
}

/// Generates one side with Mesa's generator into `out`.
fn generate(inputs: &Inputs, renderer: bool, out: &Path) -> Result<(), ShimError> {
    std::fs::create_dir_all(out).map_err(io(format!("cannot create {}", out.display())))?;
    let mut command = Command::new("uv");
    command
        .args(["run", "--quiet", "--with", MAKO, "python"])
        .arg(inputs.path("vn_protocol.py"))
        .arg("--srcdir")
        .arg(inputs.root())
        .arg("--outdir")
        .arg(out);
    if renderer {
        command.arg("--renderer");
    }
    run(&mut command)
}

/// Compiles `source` with everything it includes into a shared library.
fn compile(source: &Path, includes: &[&Path], library: &Path) -> Result<(), ShimError> {
    let compiler = cc::Build::new()
        .target(HOST)
        .host(HOST)
        .opt_level(0)
        .cargo_metadata(false)
        .cargo_warnings(false)
        .cargo_debug(false)
        .try_get_compiler()
        .map_err(|e| ShimError::Compiler(e.to_string()))?;
    let mut command = compiler.to_command();
    command.args(["-shared", "-fPIC", "-std=gnu11", "-w"]);
    for include in includes {
        command.arg("-I").arg(include);
    }
    command.arg(source).arg("-o").arg(library);
    run(&mut command)
}

/// The generated headers and the loaded libraries.
pub struct Mesa {
    driver: Library,
    renderer: Library,
}

impl Mesa {
    /// Generates both sides (unless `work` already holds them for these
    /// inputs), compiles and loads them.
    pub fn build(inputs: &Inputs, an: &Analysis, work: &Path) -> Result<Self, ShimError> {
        let stamp: String = inputs
            .manifest()
            .files
            .iter()
            .map(|f| format!("{} {}\n", f.path, f.sha256))
            .collect();
        let stamp_path = work.join("inputs.stamp");
        let current = std::fs::read_to_string(&stamp_path).is_ok_and(|text| text == stamp);
        let (driver_commands, renderer_commands) = commands(an);

        let parity = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parity");
        let include = inputs.path("include");
        let driver_dir = work.join("driver");
        let renderer_dir = work.join("renderer");
        let driver_source = work.join("parity_driver.c");
        let renderer_source = work.join("parity_renderer.c");
        let driver_lib = work.join(format!(
            "libparity_driver.{}",
            std::env::consts::DLL_EXTENSION
        ));
        let renderer_lib = work.join(format!(
            "libparity_renderer.{}",
            std::env::consts::DLL_EXTENSION
        ));

        std::fs::create_dir_all(work).map_err(io(format!("cannot create {}", work.display())))?;
        std::fs::write(
            &driver_source,
            DriverSource {
                commands: &driver_commands,
            }
            .render()?,
        )
        .map_err(io("cannot write the driver entry points"))?;
        std::fs::write(
            &renderer_source,
            RendererSource {
                commands: &renderer_commands,
            }
            .render()?,
        )
        .map_err(io("cannot write the renderer entry points"))?;

        // The generator runs for a minute per side and the compilers take
        // longer: both sides run at once.
        let side = |renderer: bool, dir: &Path, source: &Path, library: &Path| {
            if !current {
                generate(inputs, renderer, dir)?;
            }
            compile(source, &[&parity, &include, dir], library)
        };
        let side = &side;
        std::thread::scope(|scope| -> Result<(), ShimError> {
            let (dir, source, library) = (&driver_dir, &driver_source, &driver_lib);
            let driver = scope.spawn(move || side(false, dir, source, library));
            let (dir, source, library) = (&renderer_dir, &renderer_source, &renderer_lib);
            let renderer = scope.spawn(move || side(true, dir, source, library));
            driver.join().expect("the driver build does not panic")?;
            renderer.join().expect("the renderer build does not panic")
        })?;
        std::fs::write(&stamp_path, stamp).map_err(io("cannot write the stamp"))?;

        let load = |path: &Path| {
            // SAFETY: the library is the one just compiled from the generated
            // C; its initialisers do nothing.
            unsafe { Library::new(path) }.map_err(|source| ShimError::Load {
                path: path.to_owned(),
                source,
            })
        };
        Ok(Self {
            driver: load(&driver_lib)?,
            renderer: load(&renderer_lib)?,
        })
    }

    /// Sets the renderer protocol the driver encodes for.
    ///
    /// `enabled` is indexed by extension number and must outlive every later
    /// driver call.
    pub fn set_protocol(&self, api_version: u32, enabled: &[u8]) -> Result<(), ShimError> {
        type SetProtocol = unsafe extern "C" fn(u32, *const u8, u32);
        // SAFETY: the signature is the one of the C definition.
        let function: libloading::Symbol<'_, SetProtocol> = unsafe {
            self.driver
                .get(b"parity_set_protocol\0")
                .map_err(|source| ShimError::Symbol {
                    library: "driver",
                    symbol: "parity_set_protocol".into(),
                    source,
                })?
        };
        // SAFETY: as above; the caller keeps `enabled` alive.
        unsafe { function(api_version, enabled.as_ptr(), enabled.len() as u32) };
        Ok(())
    }

    pub fn driver_command(&self, name: &str) -> Result<DriverEntry, ShimError> {
        let symbol = |prefix: &str| format!("parity_{prefix}_{name}\0");
        macro_rules! get {
            ($prefix:literal, $ty:ty) => {{
                let symbol = symbol($prefix);
                // SAFETY: the signature is the one of the template's definition.
                let function: libloading::Symbol<'_, $ty> = unsafe {
                    self.driver.get(symbol.as_bytes())
                }
                .map_err(|source| ShimError::Symbol {
                    library: "driver",
                    symbol: symbol.trim_end_matches('\0').to_owned(),
                    source,
                })?;
                *function
            }};
        }
        Ok(DriverEntry {
            sizeof: get!("sizeof", unsafe extern "C" fn(*mut *mut c_void) -> usize),
            encode: get!(
                "encode",
                unsafe extern "C" fn(*mut u8, usize, u32, *mut *mut c_void, *mut i32) -> usize
            ),
            sizeof_reply: get!(
                "sizeof_reply",
                unsafe extern "C" fn(*mut *mut c_void) -> usize
            ),
        })
    }

    pub fn renderer_command(&self, name: &str) -> Result<RendererEntry, ShimError> {
        let symbol = format!("parity_encode_reply_{name}\0");
        // SAFETY: the signature is the one of the template's definition.
        let function: libloading::Symbol<
            '_,
            unsafe extern "C" fn(*mut u8, usize, u64, *mut *mut c_void, *mut i32) -> usize,
        > = unsafe { self.renderer.get(symbol.as_bytes()) }.map_err(|source| {
            ShimError::Symbol {
                library: "renderer",
                symbol: symbol.trim_end_matches('\0').to_owned(),
                source,
            }
        })?;
        Ok(RendererEntry {
            encode_reply: *function,
        })
    }
}

/// A driver command's entry points; valid while the [`Mesa`] lives.
pub struct DriverEntry {
    sizeof: unsafe extern "C" fn(*mut *mut c_void) -> usize,
    encode: unsafe extern "C" fn(*mut u8, usize, u32, *mut *mut c_void, *mut i32) -> usize,
    sizeof_reply: unsafe extern "C" fn(*mut *mut c_void) -> usize,
}

impl DriverEntry {
    /// # Safety
    /// `args` must point to the filled storage of every parameter.
    pub unsafe fn sizeof(&self, args: &mut [*mut c_void]) -> usize {
        // SAFETY: by the contract.
        unsafe { (self.sizeof)(args.as_mut_ptr()) }
    }

    /// Encodes into `buffer` and returns the length the generated code
    /// wrote and whether it wanted more than the buffer holds.
    ///
    /// # Safety
    /// As [`DriverEntry::sizeof`].
    pub unsafe fn encode(
        &self,
        buffer: &mut [u8],
        flags: u32,
        args: &mut [*mut c_void],
    ) -> (usize, bool) {
        let mut overflow = 0i32;
        // SAFETY: by the contract.
        let len = unsafe {
            (self.encode)(
                buffer.as_mut_ptr(),
                buffer.len(),
                flags,
                args.as_mut_ptr(),
                &mut overflow,
            )
        };
        (len, overflow != 0)
    }

    /// # Safety
    /// As [`DriverEntry::sizeof`].
    pub unsafe fn sizeof_reply(&self, args: &mut [*mut c_void]) -> usize {
        // SAFETY: by the contract.
        unsafe { (self.sizeof_reply)(args.as_mut_ptr()) }
    }
}

pub struct RendererEntry {
    encode_reply: unsafe extern "C" fn(*mut u8, usize, u64, *mut *mut c_void, *mut i32) -> usize,
}

impl RendererEntry {
    /// # Safety
    /// `args` must point to the filled storage of every parameter.
    pub unsafe fn encode_reply(
        &self,
        buffer: &mut [u8],
        ret: u64,
        args: &mut [*mut c_void],
    ) -> (usize, bool) {
        let mut overflow = 0i32;
        // SAFETY: by the contract.
        let len = unsafe {
            (self.encode_reply)(
                buffer.as_mut_ptr(),
                buffer.len(),
                ret,
                args.as_mut_ptr(),
                &mut overflow,
            )
        };
        (len, overflow != 0)
    }
}

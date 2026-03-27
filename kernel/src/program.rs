use helios_hal::resource::WasiRights;

/// Blueprint for a user-mode task, which has not been compiled to native code yet and is not ready to be executed.
pub struct Blueprint<'a> {
    binary: &'a [u8],
    rights: WasiRights,
    ty: BluePrintType,
}

pub struct ResourceTable {}

pub enum BluePrintType {
    Wasm,
    CraneliftAOT,
    CraneliftJIT,
}

impl<'a> Blueprint<'a> {
    pub fn new(binary: &'a [u8], rights: WasiRights, ty: BluePrintType) -> Self {
        Self { binary, rights, ty }
    }

    pub fn new_wasm(wasm: &'a [u8], rights: WasiRights) -> Self {
        Self::new(wasm, rights, BluePrintType::Wasm)
    }

    /// Lanuch kernel computing task to compile the blueprint to a user-mode task.
    pub async fn compile(self) -> Task {
        todo!()
    }
}

/// User-mode task, which has been compiled to native code and is ready to be executed.
pub struct Task {
    rights: WasiRights,
}

type ExitCode = u32;

impl Task {
    pub async fn run(self) -> ExitCode {
        todo!()
    }
}

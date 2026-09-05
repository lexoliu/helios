/// The guest vsock port the debugger serves inspector RPC on.
///
/// Both ends need the same number and neither can discover it: the guest
/// binds it before anything is listening for it, and the host connects
/// to it before the guest can announce anything. It lives here because
/// this crate is the one both ends already share.
pub const VSOCK_RPC_PORT: u32 = 1024;

pub mod debugger;
pub mod error;
mod wire;

pub mod system;
#[cfg(feature = "host")]
pub mod transport;

pub use error::DispatchError;
pub use error::RpcError;
#[cfg(feature = "host")]
pub use error::TransportError;

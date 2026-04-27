pub mod debugger;
pub mod error;
mod wire;

pub mod system;
#[cfg(feature = "host")]
pub mod transport;

pub use error::RpcError;
#[cfg(feature = "host")]
pub use error::TransportError;
#[cfg(feature = "guest")]
pub use error::DispatchError;

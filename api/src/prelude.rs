//! Extension traits programs usually want in scope; `use
//! helios_api::prelude::*;` brings the async read/write adapters in
//! without naming them.

#[cfg(feature = "io")]
pub use crate::io::{ReadExt, WriteExt};
#[cfg(feature = "io")]
pub use crate::{AsyncRead, AsyncWrite};

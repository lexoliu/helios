use std::future::Future;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::runtime;

const REMOTE_TIMEOUT: Duration = Duration::from_secs(180);

pub async fn call<T, E>(
    future: impl Future<Output = core::result::Result<T, E>>,
    waiting_for: &str,
) -> Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let value = runtime::timeout(REMOTE_TIMEOUT, future)
        .await
        .with_context(|| format!("timed out waiting for {waiting_for}"))?;
    value.with_context(|| format!("remote {waiting_for} failed"))
}

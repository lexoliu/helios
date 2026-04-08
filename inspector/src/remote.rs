use std::future::Future;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::runtime;

const REMOTE_TIMEOUT: Duration = Duration::from_secs(180);

pub async fn call<T>(future: impl Future<Output = Result<T>>, waiting_for: &str) -> Result<T> {
    runtime::timeout(REMOTE_TIMEOUT, future)
        .await
        .with_context(|| format!("timed out waiting for {waiting_for}"))?
}

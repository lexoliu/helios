use anyhow::Result;
use wrpc_transport::Invoke;

use super::Subscription;
use super::imports::helios::system::stats as raw;

pub use raw::{Memory, MemoryPressure, MonoNanos, Permille, Processor, Processors, Sample};

pub async fn snapshot<C>(wrpc: &C) -> Result<Sample>
where
    C: Invoke<Context = ()>,
{
    raw::snapshot(wrpc, ()).await
}

pub async fn subscribe<'a, C>(wrpc: &'a C, period: MonoNanos) -> Result<Subscription<'a, Sample>>
where
    C: Invoke<Context = ()> + 'a,
{
    let (stream, driver) = raw::subscribe(wrpc, (), period).await?;
    Ok(Subscription::new(stream, driver))
}

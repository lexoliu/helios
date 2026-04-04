use anyhow::Result;
use wrpc_transport::Invoke;

use super::Subscription;
use super::imports::helios::system::tracing as raw;

pub use raw::{Event, Field, Filter, Level, MonoNanos, Value};

pub async fn recent<C>(wrpc: &C, filter: &Filter, limit: u32) -> Result<Vec<Event>>
where
    C: Invoke<Context = ()>,
{
    raw::recent(wrpc, (), filter, limit).await
}

pub async fn subscribe<'a, C>(wrpc: &'a C, filter: &Filter) -> Result<Subscription<'a, Event>>
where
    C: Invoke<Context = ()> + 'a,
{
    let (stream, driver) = raw::subscribe(wrpc, (), filter).await?;
    Ok(Subscription::new(stream, driver))
}

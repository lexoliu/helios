use alloc::vec::Vec;

use crate::fs::IoResult;

type MacAddress = [u8; 6];

pub trait NetDriver: Send + Sync {
    fn mac_address(&self) -> MacAddress;

    fn transmit(&self, packet: &[u8]) -> impl Future<Output = IoResult<()>> + Send;

    fn receive(&self) -> impl Future<Output = IoResult<Vec<u8>>> + Send;
}

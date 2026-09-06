//! The network device the hosted machine does not have.
//!
//! Every backend names the device its kernel's network service is built
//! over, because the service is what the component host's socket
//! interfaces are typed against. The hosted backend runs the kernel on
//! the host OS and brings up no NIC of its own: it has no virtio
//! transport to attach one to, and the host's own stack is not a device
//! this kernel drives.
//!
//! So the slot is filled by an uninhabited type. "This machine has no
//! network interface" then holds by construction rather than by a
//! runtime check or a device that answers nothing: no value of
//! [`HostedNetworkDevice`] can be built, so
//! `Kernel::install_network_interface` cannot be called here and the
//! component host's socket paths answer the same "no network service"
//! error they answer on any machine that reached component start-up
//! without one.
//!
//! Concurrency contract: vacuous. Every method is reached only through a
//! value of the type, and there is none.

use core::future::Future;

use helios_hal::io::IoError;
use helios_kernel::{InterfaceEventMark, NetworkDevice, PacketBuffer, RxFrame};

/// The hosted machine's network interface, of which there is none.
#[derive(Clone)]
pub(crate) enum HostedNetworkDevice {}

#[expect(
    unreachable_code,
    reason = "every body here is unreachable because the interface is uninhabited, which is the whole point of the type: the compiler proves the hosted machine never reaches a network device method rather than a runtime check doing it"
)]
impl NetworkDevice for HostedNetworkDevice {
    fn mac_address(&self) -> [u8; 6] {
        match *self {}
    }

    fn max_frame_len(&self) -> usize {
        match *self {}
    }

    fn try_receive<'a>(
        &'a self,
        buffer: &'a mut PacketBuffer,
    ) -> impl Future<Output = Result<bool, IoError>> + Send + 'a {
        let _ = buffer;
        core::future::ready(match *self {})
    }

    fn try_receive_frame(&self) -> impl Future<Output = Result<Option<RxFrame>, IoError>> + Send {
        core::future::ready(match *self {})
    }

    fn repost_rx_frame<'a>(
        &'a self,
        frame: RxFrame,
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        let _ = frame;
        core::future::ready(match *self {})
    }

    fn event_mark(&self, queue_idx: usize) -> InterfaceEventMark {
        let _ = queue_idx;
        match *self {}
    }

    fn wait_for_event_since(
        &self,
        queue_idx: usize,
        mark: InterfaceEventMark,
    ) -> impl Future<Output = ()> + Send + '_ {
        let _ = (queue_idx, mark);
        core::future::ready(match *self {})
    }
}

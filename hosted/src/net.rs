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

use helios_kernel::{InterfaceEventMark, NetworkDevice, RxDrain, RxFrame};

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

    fn set_active_queue_pairs(&self, pairs: usize) -> helios_hal::io::IoResult<()> {
        let _ = pairs;
        match *self {}
    }

    fn steer_queue_pair(&self, pair: usize, processor: helios_hal::cpu::ProcessorId) {
        let _ = (pair, processor);
        match *self {}
    }

    fn try_receive_frames_immediate_on<Sink>(
        &self,
        queue_idx: usize,
        budget: usize,
        sink: Sink,
    ) -> Option<RxDrain>
    where
        Sink: FnMut(&RxFrame),
    {
        let _ = (queue_idx, budget, sink);
        match *self {}
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

//! The kernel's ownership of the machine's sound device.
//!
//! A sound device reports without being asked — a period elapsed, an
//! underrun, a plug pulled out of a jack — and a device nobody reads is
//! a device that stops reporting: its event ring is its whole buffer
//! pool, so once the guest has left every buffer full the host has
//! nowhere to put the next announcement, and on a transport whose
//! interrupt line is a function of a read-to-clear status register the
//! line never falls again either. The task here owns the device's event
//! ring and keeps it moving for as long as the machine runs, whether or
//! not anything is playing.
//!
//! What is *played* is not decided here. The device is handed on whole,
//! and the audio service that owns playback takes it over from this
//! task; until it exists the events go to the kernel log, which is what
//! makes a machine with no player readable.
//!
//! # SMP contract
//!
//! One task per device, local to the processor that brought the device
//! up, because that is the processor the device's interrupt is routed
//! to. It parks on the device's own notification and never polls, and it
//! is the device's single event reader — the contract's own rule. It
//! holds the same device handle the interrupt route holds; the trait
//! says every method takes `&self`, may be called from several tasks at
//! once, and that an implementation serialises access to its own rings.

use helios_hal::audio::{AudioEvent, PlaybackDevice};
use helios_hal::cpu::Cpu;
use helios_hal::watchdog::Watchdog;

use crate::Kernel;

/// Brings the machine's sound device under kernel ownership.
///
/// The device is never handed anywhere else by this call: what it starts
/// is the drain that keeps the device's event ring moving. A caller that
/// wants to play holds its own handle to the same device.
pub fn install_sound_device<CpuImpl, WatchdogImpl, Device>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    device: Device,
) where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: PlaybackDevice,
{
    kernel.spawn_local_detached(async move {
        drain_sound_events(&device).await;
    });
}

/// Keeps one device's event ring moving, for as long as the machine
/// runs.
///
/// Every announcement is logged rather than counted, because each of
/// them is rare and each of them means something a person debugging
/// audio wants to see: an underrun is a player that did not keep up, and
/// a jack change is somebody at the machine. A period elapsing is the
/// one frequent event, and it goes to the trace level so a running
/// stream does not fill the console.
async fn drain_sound_events<Device: PlaybackDevice>(device: &Device) {
    loop {
        let event = match device.next_event().await {
            Ok(event) => event,
            Err(error) => {
                // The device and its ring disagree about what is in it.
                // Asking again would fail the same way without ever
                // parking, so the drain stops here and says so; whoever
                // is playing sees a device that reports nothing more.
                tracing::error!(
                    target: "helios_kernel::sound",
                    %error,
                    "sound device faulted; its events are no longer being read"
                );
                return;
            }
        };
        match event {
            AudioEvent::PeriodElapsed(stream) => tracing::trace!(
                target: "helios_kernel::sound",
                stream = stream.index(),
                "sound period elapsed"
            ),
            AudioEvent::Underrun(stream) => tracing::warn!(
                target: "helios_kernel::sound",
                stream = stream.index(),
                "sound stream underran; the jack played a gap"
            ),
            AudioEvent::JackConnected(jack) => tracing::info!(
                target: "helios_kernel::sound",
                jack = jack.index(),
                "sound jack connected"
            ),
            AudioEvent::JackDisconnected(jack) => tracing::info!(
                target: "helios_kernel::sound",
                jack = jack.index(),
                "sound jack disconnected"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::pin::pin;

    use spin::Mutex;

    use futures_lite::future::{block_on, poll_once};
    use helios_hal::audio::{
        AudioError, AudioEvent, AudioResult, ChannelMapList, JackList, PcmParams, PlaybackDevice,
        StreamId, StreamList, XferStatus,
    };

    /// A device that hands out a canned run of events and then faults,
    /// which is the one thing the drain has to survive without spinning.
    struct ScriptedDevice {
        events: Mutex<Vec<AudioResult<AudioEvent>>>,
    }

    impl ScriptedDevice {
        fn new(mut events: Vec<AudioResult<AudioEvent>>) -> Self {
            events.reverse();
            Self {
                events: Mutex::new(events),
            }
        }
    }

    impl PlaybackDevice for ScriptedDevice {
        async fn streams(&self) -> AudioResult<StreamList> {
            Ok(StreamList::new())
        }

        async fn jacks(&self) -> AudioResult<JackList> {
            Ok(JackList::new())
        }

        async fn channel_maps(&self) -> AudioResult<ChannelMapList> {
            Ok(ChannelMapList::new())
        }

        async fn set_params(&self, _stream: StreamId, _params: PcmParams) -> AudioResult<()> {
            Ok(())
        }

        async fn prepare(&self, _stream: StreamId) -> AudioResult<()> {
            Ok(())
        }

        async fn start(&self, _stream: StreamId) -> AudioResult<()> {
            Ok(())
        }

        async fn stop(&self, _stream: StreamId) -> AudioResult<()> {
            Ok(())
        }

        async fn release(&self, _stream: StreamId) -> AudioResult<()> {
            Ok(())
        }

        async fn write(&self, _stream: StreamId, _period: &[u8]) -> AudioResult<XferStatus> {
            Ok(XferStatus::default())
        }

        async fn next_event(&self) -> AudioResult<AudioEvent> {
            self.events
                .lock()
                .pop()
                .expect("the drain asked for one event more than the script holds")
        }
    }

    /// The drain reads every event the device published and then stops
    /// on the fault, rather than asking again for an answer that cannot
    /// change and never parking.
    #[test]
    fn the_drain_reads_every_event_and_stops_on_a_fault() {
        let device = ScriptedDevice::new(alloc::vec![
            Ok(AudioEvent::PeriodElapsed(StreamId::new(0))),
            Ok(AudioEvent::Underrun(StreamId::new(0))),
            Err(AudioError::DeviceIo),
        ]);

        assert_eq!(
            block_on(poll_once(pin!(super::drain_sound_events(&device)))),
            Some(()),
            "a faulted device ends the drain instead of spinning on it"
        );
        assert!(device.events.lock().is_empty());
    }
}

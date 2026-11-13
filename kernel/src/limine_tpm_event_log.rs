//! The Limine boot protocol's TPM Event Log request, which both Limine
//! backends place in their image.
//!
//! The `limine` crate (0.6.5, its latest release) has no binding for
//! this feature, so the request is declared here once from the protocol
//! text (`PROTOCOL.md`, "TPM Event Log Feature", Limine 12.1.0 and
//! later): the shared header of every request, and a response that
//! names the log's format, its size and its address in the higher-half
//! direct map. Limine leaves the response pointer untouched when the
//! firmware has no TPM, implements no `EFI_TCG2_PROTOCOL`, or the log
//! could not be read.

use core::cell::UnsafeCell;

use helios_hal::boot::{MeasurementLog, MeasurementLogFormat};

/// `LIMINE_COMMON_MAGIC`, the first half of every request's identifier.
const COMMON_MAGIC: [u64; 2] = [0xc7b1_dd30_df4c_8b88, 0x0a82_e883_a194_f07b];
/// `LIMINE_TPM_EVENT_LOG_REQUEST_ID`'s own half.
const TPM_EVENT_LOG_ID: [u64; 2] = [0x98e0_94fc_7e76_e979, 0xee8d_8775_c54e_1d1f];
/// `LIMINE_TPM_EVENT_LOG_FORMAT_TCG_1_2` and `_TCG_2`.
const FORMAT_TCG_1_2: u64 = 1;
const FORMAT_TCG_2: u64 = 2;

/// `struct limine_tpm_event_log_response`.
#[repr(C)]
struct Response {
    /// The protocol's leading member. Revision 0 is the only one, and
    /// this kernel reads nothing a later one would add.
    revision: u64,
    format: u64,
    size: u64,
    address: u64,
}

/// `struct limine_tpm_event_log_request`, at revision 0.
///
/// A backend declares one as a `#[used]` static; Limine finds it in the
/// image by its identifier and writes the response pointer before the
/// kernel runs.
#[repr(C)]
pub struct LimineTpmEventLogRequest {
    id: [u64; 4],
    revision: u64,
    response: UnsafeCell<*const Response>,
}

// SAFETY: the bootloader writes the response pointer before the kernel
// runs and nothing writes it afterwards, so every processor reads the
// same value.
unsafe impl Sync for LimineTpmEventLogRequest {}

impl LimineTpmEventLogRequest {
    pub const fn new() -> Self {
        Self {
            id: [
                COMMON_MAGIC[0],
                COMMON_MAGIC[1],
                TPM_EVENT_LOG_ID[0],
                TPM_EVENT_LOG_ID[1],
            ],
            revision: 0,
            response: UnsafeCell::new(core::ptr::null()),
        }
    }

    /// The log Limine handed over, if it handed one over.
    pub fn measurement_log(&self) -> Option<MeasurementLog> {
        // SAFETY: the pointer is the bootloader's and is read as a
        // volatile value because the compiler never saw it written.
        let response = unsafe { self.response.get().read_volatile() };
        // SAFETY: a response Limine wrote lives in bootloader-reclaimable
        // memory, which this kernel never hands to its allocator, and is
        // reached through the direct map it points into.
        let response = unsafe { response.as_ref() }?;
        let format = match response.format {
            FORMAT_TCG_1_2 => MeasurementLogFormat::Tcg12,
            FORMAT_TCG_2 => MeasurementLogFormat::Tcg2,
            other => MeasurementLogFormat::Other(other),
        };
        let size = usize::try_from(response.size).unwrap_or_else(|_| {
            panic!(
                "Limine's TPM event log is {} bytes, more than this processor can address",
                response.size
            )
        });
        Some(MeasurementLog {
            format,
            address: response.address as usize,
            size,
        })
    }
}

impl Default for LimineTpmEventLogRequest {
    fn default() -> Self {
        Self::new()
    }
}

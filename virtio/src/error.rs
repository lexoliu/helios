use helios_hal::io::IoError;
use virtio_drivers::transport::mmio::MmioError;
use virtio_drivers::{Error as VirtioError, transport::DeviceTypeError};

pub(crate) fn map_virtio_error(error: VirtioError) -> IoError {
    match error {
        VirtioError::InvalidParam => IoError::InvalidBufferLength {
            required_multiple: 1,
            actual: 0,
        },
        VirtioError::Unsupported => IoError::Unsupported,
        VirtioError::IoError
        | VirtioError::NotReady
        | VirtioError::WrongToken
        | VirtioError::AlreadyUsed
        | VirtioError::DmaError
        | VirtioError::ConfigSpaceTooSmall
        | VirtioError::ConfigSpaceMissing
        | VirtioError::QueueFull
        | VirtioError::SocketDeviceError(_) => IoError::DeviceFault,
    }
}

pub(crate) fn map_mmio_error(error: MmioError) -> IoError {
    match error {
        MmioError::InvalidDeviceID(DeviceTypeError::InvalidDeviceType(_))
        | MmioError::UnsupportedVersion(_)
        | MmioError::BadMagic(_)
        | MmioError::MmioRegionTooSmall => IoError::Unsupported,
    }
}

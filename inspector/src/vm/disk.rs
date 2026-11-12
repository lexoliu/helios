//! The disk images `--disk` attaches beside the scratch disk.
//!
//! The guest kernel gives a disk its role by the serial it reports: the
//! scratch disk carries `helios-data`, the boot image carries none, and
//! every other serial names a disk the guest may mount. So a `--disk`
//! is an image and the serial it is found by, and nothing about its
//! place on the bus matters.

use std::path::PathBuf;
use std::str::FromStr;

use super::qemu::QemuOptions;

/// Longest serial a virtio-blk device reports: its identification
/// request answers `VIRTIO_BLK_ID_BYTES` (virtio 1.2 §5.2.6). QEMU cuts a
/// longer one short, which could make two serials the guest sees equal.
pub(crate) const MAX_DISK_SERIAL_BYTES: usize = 20;

const SPEC_SHAPE: &str = "<image>,serial=<name>[,readonly]";

/// One `--disk <image>,serial=<name>[,readonly]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VmDiskSpec {
    pub(crate) image: PathBuf,
    pub(crate) serial: String,
    pub(crate) readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DiskSpecError {
    #[error("--disk {spec:?} names no image; expected {SPEC_SHAPE}")]
    MissingImage { spec: String },
    #[error(
        "--disk {spec:?} names no serial; the guest finds a disk by its serial, so expected {SPEC_SHAPE}"
    )]
    MissingSerial { spec: String },
    #[error("--disk {spec:?} gives serial= more than once")]
    RepeatedSerial { spec: String },
    #[error("--disk {spec:?} has the unknown option {option:?}; expected {SPEC_SHAPE}")]
    UnknownOption { spec: String, option: String },
    #[error("--disk {spec:?} gives an empty serial")]
    EmptySerial { spec: String },
    #[error(
        "--disk serial {serial:?} is {len} bytes; a virtio-blk device reports at most {MAX_DISK_SERIAL_BYTES}"
    )]
    SerialTooLong { serial: String, len: usize },
    #[error("--disk serial {serial:?} is the scratch disk's, which the inspector attaches itself")]
    ReservedSerial { serial: String },
    #[error("two --disk images share the serial {serial:?}; the guest tells disks apart by serial")]
    DuplicateSerial { serial: String },
}

impl FromStr for VmDiskSpec {
    type Err = DiskSpecError;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let mut parts = spec.split(',');
        let image = parts
            .next()
            .filter(|image| !image.is_empty())
            .ok_or_else(|| DiskSpecError::MissingImage {
                spec: spec.to_owned(),
            })?;
        let mut serial = None;
        let mut readonly = false;
        for option in parts {
            if let Some(value) = option.strip_prefix("serial=") {
                if serial.replace(value).is_some() {
                    return Err(DiskSpecError::RepeatedSerial {
                        spec: spec.to_owned(),
                    });
                }
            } else if option == "readonly" {
                readonly = true;
            } else {
                return Err(DiskSpecError::UnknownOption {
                    spec: spec.to_owned(),
                    option: option.to_owned(),
                });
            }
        }
        let serial = serial.ok_or_else(|| DiskSpecError::MissingSerial {
            spec: spec.to_owned(),
        })?;
        if serial.is_empty() {
            return Err(DiskSpecError::EmptySerial {
                spec: spec.to_owned(),
            });
        }
        if serial.len() > MAX_DISK_SERIAL_BYTES {
            return Err(DiskSpecError::SerialTooLong {
                serial: serial.to_owned(),
                len: serial.len(),
            });
        }
        Ok(Self {
            image: PathBuf::from(image),
            serial: serial.to_owned(),
            readonly,
        })
    }
}

/// Refuses a set of disks the guest could not tell apart from each other
/// or from the scratch disk.
pub(crate) fn validate_disks(
    disks: &[VmDiskSpec],
    scratch_serial: &str,
) -> Result<(), DiskSpecError> {
    for (index, disk) in disks.iter().enumerate() {
        if disk.serial == scratch_serial {
            return Err(DiskSpecError::ReservedSerial {
                serial: disk.serial.clone(),
            });
        }
        if disks[..index]
            .iter()
            .any(|other| other.serial == disk.serial)
        {
            return Err(DiskSpecError::DuplicateSerial {
                serial: disk.serial.clone(),
            });
        }
    }
    Ok(())
}

impl VmDiskSpec {
    /// The `-drive` backing this disk, under `id`.
    pub(crate) fn drive(&self, id: &str) -> QemuOptions {
        let mut drive = QemuOptions::keyed();
        drive.set("if", "none");
        drive.set("format", "raw");
        drive.set("file", self.image.display());
        drive.set("id", id);
        if self.readonly {
            drive.set("readonly", "on");
        }
        drive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disk_is_an_image_and_a_serial() {
        assert_eq!(
            "images/rootfs.erofs,serial=rootfs,readonly".parse(),
            Ok(VmDiskSpec {
                image: PathBuf::from("images/rootfs.erofs"),
                serial: "rootfs".to_owned(),
                readonly: true,
            })
        );
        assert_eq!(
            "home.btrfs,serial=home".parse(),
            Ok(VmDiskSpec {
                image: PathBuf::from("home.btrfs"),
                serial: "home".to_owned(),
                readonly: false,
            })
        );
    }

    #[test]
    fn a_disk_without_a_serial_or_with_an_unknown_option_is_refused() {
        assert!(matches!(
            "home.btrfs".parse::<VmDiskSpec>(),
            Err(DiskSpecError::MissingSerial { .. })
        ));
        assert!(matches!(
            ",serial=home".parse::<VmDiskSpec>(),
            Err(DiskSpecError::MissingImage { .. })
        ));
        assert!(matches!(
            "home.btrfs,serial=home,cache=none".parse::<VmDiskSpec>(),
            Err(DiskSpecError::UnknownOption { option, .. }) if option == "cache=none"
        ));
        assert!(matches!(
            "home.btrfs,serial=a,serial=b".parse::<VmDiskSpec>(),
            Err(DiskSpecError::RepeatedSerial { .. })
        ));
        assert!(matches!(
            "home.btrfs,serial=".parse::<VmDiskSpec>(),
            Err(DiskSpecError::EmptySerial { .. })
        ));
    }

    #[test]
    fn a_serial_the_device_would_truncate_is_refused() {
        let serial = "a".repeat(MAX_DISK_SERIAL_BYTES + 1);

        assert_eq!(
            format!("disk.img,serial={serial}").parse::<VmDiskSpec>(),
            Err(DiskSpecError::SerialTooLong {
                serial,
                len: MAX_DISK_SERIAL_BYTES + 1
            })
        );
    }

    #[test]
    fn disks_the_guest_could_not_tell_apart_are_refused() {
        let disk = |serial: &str| VmDiskSpec {
            image: PathBuf::from("disk.img"),
            serial: serial.to_owned(),
            readonly: false,
        };

        assert_eq!(
            validate_disks(&[disk("helios-data")], "helios-data"),
            Err(DiskSpecError::ReservedSerial {
                serial: "helios-data".to_owned()
            })
        );
        assert_eq!(
            validate_disks(&[disk("home"), disk("home")], "helios-data"),
            Err(DiskSpecError::DuplicateSerial {
                serial: "home".to_owned()
            })
        );
        assert_eq!(
            validate_disks(&[disk("home"), disk("rootfs")], "helios-data"),
            Ok(())
        );
    }

    #[test]
    fn a_readonly_disk_is_attached_readonly() {
        let disk: VmDiskSpec = "/images/rootfs.erofs,serial=rootfs,readonly"
            .parse()
            .expect("a valid disk");

        assert_eq!(
            disk.drive("disk0").to_string(),
            "if=none,format=raw,file=/images/rootfs.erofs,id=disk0,readonly=on"
        );
    }
}

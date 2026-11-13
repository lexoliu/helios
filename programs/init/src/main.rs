//! Brings up a process tree from the boot image's configuration.
//!
//! Init mounts what the boot image's `/etc/fstab` names, lists the root
//! and every mount it made, prints `/init.txt`, and runs `/etc/rc` with
//! `/bin/dash` when the image carries one. Every program `/etc/rc` starts
//! descends from init, so it sees init's mounts.
//!
//! `/etc/fstab` holds one mount per line, `#` starting a comment:
//!
//! ```text
//! <source> <mount point> [ro|rw]
//! ```
//!
//! where the source is `memory`, `boot-image`, `host-share` or
//! `disk:<serial>`, the EROFS image on the disk that reported that
//! serial. The mode defaults to `rw`; the boot image and disks are
//! read-only whatever it says. A line init cannot parse, or a mount the
//! kernel refuses, fails init.

use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::Path;

use helios_api::mounts::{self, MountSource};
use helios_api::programs::{self, ExecRequest};

const FSTAB: &str = "/etc/fstab";
const RC: &str = "/etc/rc";
const SHELL: &str = "/bin/dash";

/// One line of `/etc/fstab`.
struct Mount {
    source: MountSource,
    label: String,
    point: String,
    read_only: bool,
}

fn invalid(line: usize, detail: impl std::fmt::Display) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, format!("{FSTAB}:{line}: {detail}"))
}

fn parse_fstab(text: &str) -> io::Result<Vec<Mount>> {
    let mut mounts = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (label, point, mode) = match fields.as_slice() {
            [label, point] => (*label, *point, "rw"),
            [label, point, mode] => (*label, *point, *mode),
            _ => return Err(invalid(number, "expected `<source> <mount point> [ro|rw]`")),
        };
        let source = match label {
            "memory" => MountSource::Memory,
            "boot-image" => MountSource::BootImage,
            "host-share" => MountSource::HostShare,
            _ => match label.strip_prefix("disk:") {
                Some(serial) if !serial.is_empty() => MountSource::Disk(serial.to_owned()),
                _ => return Err(invalid(number, format!("unknown source `{label}`"))),
            },
        };
        let read_only = match mode {
            "ro" => true,
            "rw" => false,
            _ => return Err(invalid(number, format!("unknown mode `{mode}`"))),
        };
        mounts.push(Mount {
            source,
            label: label.to_owned(),
            point: point.to_owned(),
            read_only,
        });
    }
    Ok(mounts)
}

/// The mounts `/etc/fstab` names; none when the image carries no fstab.
fn fstab() -> io::Result<Vec<Mount>> {
    match fs::read_to_string(FSTAB) {
        Ok(text) => parse_fstab(&text),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// Prints one line per entry of `directory`: its kind and name, sorted.
fn list(directory: &Path) -> io::Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry.file_type()?;
        let kind = if file_type.is_dir() {
            "dir"
        } else if file_type.is_file() {
            "file"
        } else {
            "other"
        };
        println!("{kind} {}", entry.file_name().to_string_lossy());
    }
    Ok(())
}

/// Runs `/etc/rc` under the shell, with init's authority over the tree.
async fn run_rc() -> io::Result<()> {
    let result = programs::exec(ExecRequest {
        name: "rc".to_owned(),
        args: vec![RC.to_owned()],
        env: vec![("PATH".to_owned(), "/bin".to_owned())],
        path: SHELL.to_owned(),
        stdin: Vec::new(),
        hint: None,
        capability_grants: vec![
            programs::root_directory_grant(),
            programs::root_process_grant(),
        ],
    })
    .await
    .map_err(|error| {
        io::Error::other(format!(
            "running {RC} with {SHELL}: {:?}: {}",
            error.kind, error.detail
        ))
    })?;
    io::stdout().write_all(&result.output.stdout)?;
    io::stderr().write_all(&result.output.stderr)?;
    if result.exit_code != 0 {
        return Err(io::Error::other(format!(
            "{RC} exited with status {}",
            result.exit_code
        )));
    }
    Ok(())
}

#[helios_api::main]
async fn main() -> io::Result<()> {
    let root = Path::new("/");
    println!("helios init: online root={}", root.display());

    let mounts = fstab()?;
    for mount in &mounts {
        mounts::mount(mount.source.clone(), mount.point.as_str(), mount.read_only)
            .await
            .map_err(|error| {
                io::Error::other(format!(
                    "mounting {} at {}: {:?}: {}",
                    mount.label, mount.point, error.kind, error.detail
                ))
            })?;
        println!("helios init: mounted {} at {}", mount.label, mount.point);
    }

    list(root)?;
    for mount in &mounts {
        println!("helios init: listing {}", mount.point);
        list(Path::new(&mount.point))?;
    }

    let text = fs::read_to_string("/init.txt")?;
    print!("{text}");

    if Path::new(RC).exists() {
        run_rc().await?;
    }

    Ok(())
}

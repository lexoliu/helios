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
//! <source> <mount point> <option>[,<option>]...
//! ```
//!
//! Sources are `memory`, `boot-image`, `host-share`, `disk:<serial>`
//! for EROFS, and `btrfs:<serial>` for btrfs. Disk sources use the serial
//! the device reported. Every line names `private` or `shared`, which the
//! mount is granted as for its whole life: a private mount belongs to one
//! container, which sees the Unix metadata its filesystem stores, and a
//! shared one may be seen by many, each of which sees it as WASI does. The
//! other options are `ro` or `rw` (default `rw`; the boot image and EROFS
//! disks remain read-only) and, for btrfs, Linux's `compress` (zlib) or
//! `compress=<algorithm>[:<level>]` and `commit=<seconds>`. A line init
//! cannot parse, or a mount the kernel refuses, fails init.

use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::Path;

use helios_api::mounts::{self, MountSharing, MountSource};
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
    sharing: MountSharing,
    /// The value of the `compress=` option, when the line carries one.
    compress: Option<String>,
    /// The value of the `commit=` option, in seconds.
    commit: Option<u32>,
}

/// A `commit=` value as Linux's `fsparam_u32` reads it: `kstrtouint` with
/// base 0, so `0x` starts hexadecimal, a leading `0` octal, and anything
/// else is decimal.
fn commit_seconds(value: &str) -> Option<u32> {
    let value = value.strip_prefix('+').unwrap_or(value);
    let (digits, radix) = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (hex, 16)
    } else if value.len() > 1 && value.starts_with('0') {
        (&value[1..], 8)
    } else {
        (value, 10)
    };
    if digits.is_empty() || digits.starts_with(['+', '-']) {
        return None;
    }
    u32::from_str_radix(digits, radix).ok()
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
        let [label, point, options] = fields.as_slice() else {
            return Err(invalid(
                number,
                "expected `<source> <mount point> <option>[,<option>]...`",
            ));
        };
        let (label, point, options) = (*label, *point, *options);
        let source = match label {
            "memory" => MountSource::Memory,
            "boot-image" => MountSource::BootImage,
            "host-share" => MountSource::HostShare,
            _ if label.starts_with("btrfs:") => {
                let serial = &label["btrfs:".len()..];
                if serial.is_empty() {
                    return Err(invalid(number, "empty btrfs disk serial"));
                }
                MountSource::Btrfs(serial.to_owned())
            }
            _ => match label.strip_prefix("disk:") {
                Some(serial) if !serial.is_empty() => MountSource::Disk(serial.to_owned()),
                _ => return Err(invalid(number, format!("unknown source `{label}`"))),
            },
        };
        let mut read_only = false;
        let mut sharing = None;
        let mut compress = None;
        let mut commit = None;
        for option in options.split(',') {
            match option {
                "ro" => read_only = true,
                "rw" => read_only = false,
                "private" | "shared" => {
                    let chosen = if option == "private" {
                        MountSharing::Private
                    } else {
                        MountSharing::Shared
                    };
                    if sharing.is_some_and(|earlier| earlier != chosen) {
                        return Err(invalid(
                            number,
                            "a mount is `private` or `shared`, not both",
                        ));
                    }
                    sharing = Some(chosen);
                }
                // Linux's bare `compress` selects zlib at its default level.
                "compress" => compress = Some("zlib".to_owned()),
                _ => {
                    if let Some(value) = option.strip_prefix("compress=") {
                        compress = Some(value.to_owned());
                    } else if let Some(value) = option.strip_prefix("commit=") {
                        commit = Some(commit_seconds(value).ok_or_else(|| {
                            invalid(number, format!("`{option}` is not a number of seconds"))
                        })?);
                    } else {
                        return Err(invalid(number, format!("unknown option `{option}`")));
                    }
                }
            }
        }
        let sharing = sharing.ok_or_else(|| {
            invalid(
                number,
                "the options name neither `private` nor `shared`, which every mount is granted as",
            )
        })?;
        mounts.push(Mount {
            source,
            label: label.to_owned(),
            point: point.to_owned(),
            read_only,
            sharing,
            compress,
            commit,
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
            programs::root_link_grant(),
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
        mounts::mount(
            mount.source.clone(),
            mount.point.as_str(),
            mount.read_only,
            mount.sharing,
            mount.compress.clone(),
            mount.commit,
        )
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

#[cfg(test)]
mod tests {
    use helios_api::mounts::MountSharing;

    use super::{commit_seconds, parse_fstab};

    #[test]
    fn commit_values_read_as_linux_reads_them() {
        for (value, seconds) in [
            ("5", 5),
            ("+5", 5),
            ("0", 0),
            ("010", 8),
            ("0x1e", 30),
            ("0X1E", 30),
            ("4294967295", u32::MAX),
        ] {
            assert_eq!(commit_seconds(value), Some(seconds), "{value}");
        }
        for value in ["", "+", "-1", "0x", "0x+5", "08", "4294967296", "5s", " 5"] {
            assert_eq!(commit_seconds(value), None, "{value}");
        }
    }

    #[test]
    fn an_fstab_line_carries_its_commit_interval() {
        let mounts =
            parse_fstab("btrfs:数据-データ-데이터 /btrfs rw,shared,compress=zstd:3,commit=120\n")
                .unwrap();
        assert_eq!(mounts[0].commit, Some(120));
        assert_eq!(mounts[0].compress.as_deref(), Some("zstd:3"));
        assert!(parse_fstab("btrfs:disk /btrfs shared,commit=soon\n").is_err());
        assert_eq!(parse_fstab("memory /tmp shared\n").unwrap()[0].commit, None);
    }

    #[test]
    fn every_fstab_line_names_whether_its_mount_is_private_or_shared() {
        let text = "\
btrfs:中文磁盘 /中文 private,rw
btrfs:日本語ディスク /日本語 shared
host-share /한국어 ro,shared
";
        let mounts = parse_fstab(text).unwrap();
        let granted: Vec<_> = mounts
            .iter()
            .map(|mount| (mount.point.as_str(), mount.sharing, mount.read_only))
            .collect();
        assert_eq!(
            granted,
            [
                ("/中文", MountSharing::Private, false),
                ("/日本語", MountSharing::Shared, false),
                ("/한국어", MountSharing::Shared, true),
            ]
        );
        // Neither, both, or no options at all: init refuses the line.
        for line in [
            "memory /tmp\n",
            "memory /tmp rw\n",
            "memory /tmp private,shared\n",
            "memory /tmp shared,private\n",
        ] {
            assert!(parse_fstab(line).is_err(), "{line:?}");
        }
        // Naming the same one twice is no contradiction.
        let twice = parse_fstab("memory /tmp shared,shared\n").unwrap();
        assert_eq!(twice[0].sharing, MountSharing::Shared);
    }
}

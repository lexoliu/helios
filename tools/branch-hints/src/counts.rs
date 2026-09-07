//! Reading the counter dump back out of a guest run.
//!
//! The channel is the program's own stdout, the one the inspector's
//! `vm … shell` command already brings back from a Helios guest. The
//! alternative — the LLVM raw profile export of #70 — carries the
//! *kernel's* `-C profile-generate` counters out of a kernel built for it;
//! it has no way to describe a counter array that belongs to a user-mode
//! wasm instance, so a branch profile would need a second export and a
//! second instrumented build to use it. stdout needs neither.
//!
//! The dump is framed by markers so the surrounding console traffic — boot
//! lines, the shell's own echo, the program's real output — is not mistaken
//! for counts, and the trailing marker is what proves the guest finished
//! writing.

use crate::{BEGIN_MARKER, END_MARKER, Error};

/// One site's counts as the guest reported them.
#[derive(Debug, Clone, Copy)]
pub struct Record {
    pub site: u64,
    pub taken: u64,
    pub not_taken: u64,
}

/// One complete dump.
#[derive(Debug, Clone)]
pub struct Counts {
    pub records: Vec<Record>,
}

impl Counts {
    /// Parses every dump in `output` and sums them.
    ///
    /// A run can produce more than one — a workload that spawns the
    /// program twice, say — and they are all the same module's counters.
    pub fn parse(output: &str) -> Result<Self, Error> {
        let mut records = Vec::new();
        let mut inside = false;
        let mut closed = false;
        let mut seen = false;
        for line in output.lines() {
            let line = line.trim_end_matches('\r');
            if line.ends_with(BEGIN_MARKER) {
                inside = true;
                seen = true;
                closed = false;
                continue;
            }
            if line.ends_with(END_MARKER) {
                inside = false;
                closed = true;
                continue;
            }
            if !inside {
                continue;
            }
            records.push(parse_record(line)?);
        }
        if !seen {
            return Err(Error::NoProfileMarker);
        }
        if !closed {
            return Err(Error::TruncatedProfile);
        }
        Ok(Counts { records })
    }
}

/// Each record is three 16-digit hex fields: site index, taken,
/// not taken. Fixed width because the writer is a few dozen wasm
/// instructions the instrumenter emits, and a fixed width needs no
/// separator logic there.
fn parse_record(line: &str) -> Result<Record, Error> {
    let malformed = || Error::MalformedRecord(line.to_owned());
    if line.len() != 48 {
        return Err(malformed());
    }
    let field = |range: std::ops::Range<usize>| {
        u64::from_str_radix(&line[range], 16).map_err(|_| malformed())
    };
    Ok(Record {
        site: field(0..16)?,
        taken: field(16..32)?,
        not_taken: field(32..48)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_framed_dump() {
        let output = format!(
            "boot noise\n\
             $ /bin/qjs -e ...\n\
             quickjs-loop:42\n\
             {BEGIN_MARKER}\n\
             {:016x}{:016x}{:016x}\n\
             {:016x}{:016x}{:016x}\n\
             {END_MARKER}\n\
             $ \n",
            0, 10, 1, 7, 0, 99
        );
        let counts = Counts::parse(&output).expect("a framed dump parses");
        assert_eq!(counts.records.len(), 2);
        assert_eq!(counts.records[0].site, 0);
        assert_eq!(counts.records[0].taken, 10);
        assert_eq!(counts.records[1].site, 7);
        assert_eq!(counts.records[1].not_taken, 99);
    }

    #[test]
    fn a_dump_without_its_end_marker_is_rejected() {
        let output = format!("{BEGIN_MARKER}\n{:016x}{:016x}{:016x}\n", 0, 1, 2);
        assert!(matches!(
            Counts::parse(&output),
            Err(Error::TruncatedProfile)
        ));
    }

    #[test]
    fn output_without_a_dump_is_rejected() {
        assert!(matches!(
            Counts::parse("quickjs-loop:42\n"),
            Err(Error::NoProfileMarker)
        ));
    }
}

//! Writing a profile's conclusions into the module as a
//! `metadata.code.branch_hint` custom section.
//!
//! The section goes immediately before the code section, where the
//! branch-hinting proposal puts it, and nothing else in the module moves:
//! the offsets it carries index the code section it precedes.

use std::collections::BTreeMap;

use wasm_encoder::{BranchHint, BranchHints, Encode};

use crate::{
    Error,
    component::{self, Location},
    module::{CODE_SECTION, CoreModule},
    profile::Profile,
};

/// The custom section id the branch hints ride in.
const CUSTOM_SECTION: u8 = 0;

/// What a hinting pass did, for the build log.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub functions: usize,
    pub hints: usize,
    pub taken: usize,
    pub not_taken: usize,
    /// Sites the profile recorded but whose evidence did not justify a
    /// hint.
    pub undecided: usize,
}

/// A module with its branch hints, and what was written.
pub struct Hinted {
    pub wasm: Vec<u8>,
    pub stats: Stats,
    pub location: Location,
}

/// Writes `profile`'s hints into `input`.
pub fn apply(input: &[u8], profile: &Profile) -> Result<Hinted, Error> {
    let location = component::locate(input)?;
    let module = component::core_module(input, &location)?;

    let actual = module.code_fingerprint();
    if actual != profile.code_fingerprint {
        return Err(Error::StaleFingerprint {
            recorded: profile.code_fingerprint.clone(),
            actual,
        });
    }

    let mut by_function: BTreeMap<u32, Vec<BranchHint>> = BTreeMap::new();
    let mut stats = Stats {
        undecided: profile.sites.len(),
        ..Stats::default()
    };
    for (site, taken) in profile.hints() {
        verify(&module, site.func, site.offset)?;
        by_function.entry(site.func).or_default().push(BranchHint {
            branch_func_offset: site.offset,
            branch_hint_value: u32::from(taken),
        });
        stats.hints += 1;
        stats.undecided -= 1;
        if taken {
            stats.taken += 1;
        } else {
            stats.not_taken += 1;
        }
    }

    let mut hints = BranchHints::new();
    for (func, mut function_hints) in by_function {
        // Both the proposal and `FuncEnvironment::take_branch_hint`'s
        // forward-only decoder require ascending offsets.
        function_hints.sort_by_key(|hint| hint.branch_func_offset);
        stats.functions += 1;
        hints.function_hints(func, function_hints);
    }

    // `BranchHints::encode` writes the custom section's length, name and
    // payload; the section id in front of them is the caller's.
    let mut section = vec![CUSTOM_SECTION];
    hints.encode(&mut section);

    let code = module.section(CODE_SECTION).ok_or(Error::NoCodeSection)?;
    let mut hinted = Vec::with_capacity(module.bytes.len() + section.len());
    hinted.extend_from_slice(&module.bytes[..code.header_start]);
    hinted.extend_from_slice(&section);
    hinted.extend_from_slice(&module.bytes[code.header_start..]);

    Ok(Hinted {
        wasm: component::splice(input, &location, &hinted),
        stats,
        location,
    })
}

/// A profile is only applicable to the build it was recorded from, and the
/// module's own operator stream is the check: an offset that no longer
/// lands on an `if` or a `br_if` is a stale profile, and a hint written at
/// it would be a silently wrong compilation.
fn verify(module: &CoreModule<'_>, func: u32, offset: u32) -> Result<(), Error> {
    let body = module.body(func)?;
    if body.branches.iter().any(|branch| branch.offset == offset) {
        return Ok(());
    }
    Err(Error::StaleSite { func, offset })
}

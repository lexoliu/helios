//! The two documents the loop passes between its steps.
//!
//! Both are JSON written by serde, so a renamed field fails the build
//! rather than producing a document the next step misreads.

use serde::{Deserialize, Serialize};

use crate::{Error, HINT_RATIO, MIN_OBSERVATIONS, counts::Counts};

/// One branch site, addressed the way the branch-hinting proposal
/// addresses it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Site {
    /// Index in the core module's function index space.
    pub func: u32,
    /// Offset of the branch opcode from the start of the function body.
    pub offset: u32,
}

/// What the instrumenter recorded about the module it rewrote: which
/// counter index means which branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteMap {
    pub version: u32,
    /// The input the map was taken from, for provenance in a build log.
    pub source: String,
    pub core_module_index: u32,
    pub code_fingerprint: String,
    pub sites: Vec<Site>,
}

/// One site's counts in a recorded profile.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SiteCounts {
    pub func: u32,
    pub offset: u32,
    pub taken: u64,
    pub not_taken: u64,
}

impl SiteCounts {
    pub fn total(&self) -> u64 {
        self.taken.saturating_add(self.not_taken)
    }

    /// The hint this site justifies, or `None` when the evidence is too
    /// thin or too even to be worth a layout decision.
    pub fn hint(&self, min_observations: u64, ratio: f64) -> Option<bool> {
        let total = self.total();
        if total < min_observations {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a ratio of two counters does not need 64 bits of mantissa"
        )]
        let share = |part: u64| part as f64 / total as f64;
        if share(self.taken) >= ratio {
            Some(true)
        } else if share(self.not_taken) >= ratio {
            Some(false)
        } else {
            None
        }
    }
}

/// What real runs recorded about a module's branches.
///
/// Sites below [`MIN_OBSERVATIONS`] are dropped when the profile is
/// written: they can never produce a hint, and keeping them would make the
/// committed profile an order of magnitude larger for no decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub version: u32,
    /// The module the profile applies to, as a repository-relative path.
    pub module: String,
    pub core_module_index: u32,
    pub code_fingerprint: String,
    /// The workloads whose runs are summed into this profile.
    pub runs: Vec<String>,
    /// The observation floor the sites below were filtered by.
    pub min_observations: u64,
    /// Sites with at least `min_observations` executions, in ascending
    /// function and offset order.
    pub sites: Vec<SiteCounts>,
}

impl Profile {
    /// Sums `counts` against `map` into a profile.
    pub fn record(
        map: &SiteMap,
        module: &str,
        runs: Vec<String>,
        counts: &[Counts],
    ) -> Result<Self, Error> {
        let mut totals = vec![(0u64, 0u64); map.sites.len()];
        for recorded in counts {
            for record in &recorded.records {
                let index = usize::try_from(record.site)
                    .ok()
                    .filter(|index| *index < totals.len())
                    .ok_or(Error::CountsOutOfRange {
                        sites: map.sites.len(),
                        index: record.site,
                    })?;
                totals[index].0 = totals[index].0.saturating_add(record.taken);
                totals[index].1 = totals[index].1.saturating_add(record.not_taken);
            }
        }

        let mut sites: Vec<SiteCounts> = map
            .sites
            .iter()
            .zip(totals)
            .map(|(site, (taken, not_taken))| SiteCounts {
                func: site.func,
                offset: site.offset,
                taken,
                not_taken,
            })
            .filter(|site| site.total() >= MIN_OBSERVATIONS)
            .collect();
        sites.sort_by_key(|site| (site.func, site.offset));

        Ok(Profile {
            version: 1,
            module: module.to_owned(),
            core_module_index: map.core_module_index,
            code_fingerprint: map.code_fingerprint.clone(),
            runs,
            min_observations: MIN_OBSERVATIONS,
            sites,
        })
    }

    /// The sites this profile hints, with the value each one gets.
    pub fn hints(&self) -> impl Iterator<Item = (Site, bool)> + '_ {
        self.sites.iter().filter_map(|site| {
            site.hint(self.min_observations, HINT_RATIO).map(|taken| {
                (
                    Site {
                        func: site.func,
                        offset: site.offset,
                    },
                    taken,
                )
            })
        })
    }
}

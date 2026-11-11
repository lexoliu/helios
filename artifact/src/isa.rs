//! The instruction-set levels a cwasm artifact can be compiled for.
//!
//! Every target defines a chain of nested levels, lowest first. A level adds
//! features on top of the one below it, so a processor that satisfies a level
//! satisfies every level beneath it, and an artifact compiled for a level runs
//! on every processor that satisfies it.
//!
//! The same table serves the host tools, which name a level on their command
//! line, and the kernel, which asks the backend's feature probe which level
//! the processor satisfies. Each feature carries two names: the Cranelift
//! flag that enables it in the compiler, and the name Wasmtime asks the probe
//! about when it checks an artifact's recorded flags. Wasmtime's own mapping
//! (`check_compatible_with_isa_flag` in `crates/wasmtime/src/engine.rs` of the
//! fork) is the authority for the second name.

use alloc::string::String;
use core::iter::FusedIterator;

use thiserror::Error;

/// A processor feature probe: the backend's answer to "does this processor
/// have the feature Wasmtime calls `name`?". `None` means the probe does not
/// know the name.
pub type NativeFeatureProbe = fn(&str) -> Option<bool>;

/// One processor feature a level requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IsaFeature {
    cranelift_flag: &'static str,
    probe_name: &'static str,
}

impl IsaFeature {
    const fn new(cranelift_flag: &'static str, probe_name: &'static str) -> Self {
        Self {
            cranelift_flag,
            probe_name,
        }
    }

    /// The Cranelift setting that enables the feature in the compiler.
    pub const fn cranelift_flag(&self) -> &'static str {
        self.cranelift_flag
    }

    /// The name the feature probe is asked about.
    pub const fn probe_name(&self) -> &'static str {
        self.probe_name
    }
}

/// One level of a target's chain.
#[derive(Debug, PartialEq, Eq)]
pub struct IsaLevel {
    name: &'static str,
    below: Option<&'static IsaLevel>,
    adds: &'static [IsaFeature],
}

impl IsaLevel {
    const fn floor(name: &'static str, adds: &'static [IsaFeature]) -> Self {
        Self {
            name,
            below: None,
            adds,
        }
    }

    const fn above(
        name: &'static str,
        below: &'static IsaLevel,
        adds: &'static [IsaFeature],
    ) -> Self {
        Self {
            name,
            below: Some(below),
            adds,
        }
    }

    /// The level's name, as the host tools take it on their command line.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The number of levels below this one: 0 for a floor.
    pub fn rank(&self) -> usize {
        let mut rank = 0;
        let mut level = self;
        while let Some(below) = level.below {
            rank += 1;
            level = below;
        }
        rank
    }

    /// Every feature the level requires, the floor's first.
    pub fn features(&'static self) -> impl Iterator<Item = &'static IsaFeature> {
        Features {
            top: self,
            top_rank: self.rank(),
            rank: 0,
            index: 0,
        }
    }

    /// Whether a processor satisfying `self` also satisfies `other`, that is
    /// whether `other` is this level or one beneath it.
    pub fn contains(&self, other: &IsaLevel) -> bool {
        let mut level = self;
        loop {
            if level.name == other.name {
                return true;
            }
            match level.below {
                Some(below) => level = below,
                None => return false,
            }
        }
    }

    fn at_rank(&'static self, rank: usize) -> &'static IsaLevel {
        let mut level = self;
        for _ in rank..self.rank() {
            level = level
                .below
                .unwrap_or_else(|| panic!("ISA level {} has no level at rank {rank}", self.name));
        }
        level
    }
}

struct Features {
    top: &'static IsaLevel,
    top_rank: usize,
    rank: usize,
    index: usize,
}

impl Iterator for Features {
    type Item = &'static IsaFeature;

    fn next(&mut self) -> Option<Self::Item> {
        while self.rank <= self.top_rank {
            let level = self.top.at_rank(self.rank);
            if let Some(feature) = level.adds.get(self.index) {
                self.index += 1;
                return Some(feature);
            }
            self.rank += 1;
            self.index = 0;
        }
        None
    }
}

impl FusedIterator for Features {}

const X86_64_V3: IsaLevel = IsaLevel::floor(
    "x86-64-v3",
    &[
        IsaFeature::new("has_cmpxchg16b", "cmpxchg16b"),
        IsaFeature::new("has_sse3", "sse3"),
        IsaFeature::new("has_ssse3", "ssse3"),
        IsaFeature::new("has_sse41", "sse4.1"),
        IsaFeature::new("has_sse42", "sse4.2"),
        IsaFeature::new("has_popcnt", "popcnt"),
        IsaFeature::new("has_bmi1", "bmi1"),
        IsaFeature::new("has_bmi2", "bmi2"),
        IsaFeature::new("has_lzcnt", "lzcnt"),
        IsaFeature::new("has_avx", "avx"),
        IsaFeature::new("has_avx2", "avx2"),
        IsaFeature::new("has_fma", "fma"),
    ],
);

// Cranelift's own `x86-64-v4` preset is v3 plus DQ and VL; F is added
// explicitly because the assembler gates its EVEX instructions on `avx512f`.
const X86_64_V4: IsaLevel = IsaLevel::above(
    "x86-64-v4",
    &X86_64_V3,
    &[
        IsaFeature::new("has_avx512f", "avx512f"),
        IsaFeature::new("has_avx512vl", "avx512vl"),
        IsaFeature::new("has_avx512dq", "avx512dq"),
    ],
);

const X86_64_V4_VBMI_BITALG: IsaLevel = IsaLevel::above(
    "x86-64-v4+vbmi+bitalg",
    &X86_64_V4,
    &[
        IsaFeature::new("has_avx512vbmi", "avx512vbmi"),
        IsaFeature::new("has_avx512bitalg", "avx512bitalg"),
    ],
);

const AARCH64_LSE_FP16: IsaLevel = IsaLevel::floor(
    "aarch64-lse-fp16",
    &[
        IsaFeature::new("has_lse", "lse"),
        IsaFeature::new("has_fp16", "fp16"),
    ],
);

const AARCH64_LSE_FP16_DOTPROD: IsaLevel = IsaLevel::above(
    "aarch64-lse-fp16-dotprod",
    &AARCH64_LSE_FP16,
    &[IsaFeature::new("has_dotprod", "dotprod")],
);

const RISCV64GC: IsaLevel = IsaLevel::floor("riscv64gc", &[]);

/// The chain of levels a target's cwasm artifacts can be compiled for.
#[derive(Clone, Copy, Debug)]
pub struct CwasmIsaLevels {
    top: &'static IsaLevel,
}

/// The levels defined for `target`.
///
/// # Panics
///
/// Panics when no level table exists for the target's architecture: a target
/// Helios has no cwasm support for is a build misconfiguration, not a case to
/// compile for a guess.
pub fn cwasm_target_isa_levels(target: &str) -> CwasmIsaLevels {
    let top = if target.starts_with("x86_64-") {
        &X86_64_V4_VBMI_BITALG
    } else if target.starts_with("aarch64-") {
        &AARCH64_LSE_FP16_DOTPROD
    } else if target.starts_with("riscv64") {
        &RISCV64GC
    } else {
        panic!("no cwasm ISA levels are defined for target {target}")
    };
    CwasmIsaLevels { top }
}

impl CwasmIsaLevels {
    /// The lowest level, which every processor the target supports satisfies.
    pub fn floor(&self) -> &'static IsaLevel {
        self.top.at_rank(0)
    }

    /// The target's levels, the floor first.
    pub fn iter(&self) -> impl Iterator<Item = &'static IsaLevel> + use<> {
        let top = self.top;
        (0..=top.rank()).map(move |rank| top.at_rank(rank))
    }

    /// The level called `name`.
    pub fn by_name(&self, name: &str) -> Result<&'static IsaLevel, IsaLevelError> {
        self.iter()
            .find(|level| level.name == name)
            .ok_or_else(|| IsaLevelError::UnknownLevel {
                name: String::from(name),
                defined: self.names(),
            })
    }

    /// The highest level whose features `probe` reports the processor has.
    ///
    /// Every feature of every level is asked about, so a probe that does not
    /// know one of them fails here instead of quietly selecting a lower level.
    /// A target none of whose levels has a feature needs no probe.
    pub fn select(
        &self,
        probe: Option<NativeFeatureProbe>,
    ) -> Result<&'static IsaLevel, IsaLevelError> {
        let Some(probe) = probe else {
            return if self.iter().all(|level| level.adds.is_empty()) {
                Ok(self.top)
            } else {
                Err(IsaLevelError::NoProbe)
            };
        };
        let mut selected = None;
        let mut unmet = None;
        for level in self.iter() {
            for feature in level.adds {
                let answer =
                    probe(feature.probe_name).ok_or(IsaLevelError::ProbeDoesNotAnswer {
                        cranelift_flag: feature.cranelift_flag,
                        probe_name: feature.probe_name,
                    })?;
                if !answer && unmet.is_none() {
                    unmet = Some((level, feature));
                }
            }
            if unmet.is_none() {
                selected = Some(level);
            }
        }
        selected.ok_or_else(|| {
            let (level, feature) =
                unmet.unwrap_or_else(|| panic!("a floor with no unmet feature is selected"));
            IsaLevelError::FloorNotSatisfied {
                level: level.name,
                cranelift_flag: feature.cranelift_flag,
                probe_name: feature.probe_name,
            }
        })
    }

    /// What `probe` reports for every feature of every level, to compare two
    /// processors' answers without caring which level each would select.
    ///
    /// Fails like [`select`](Self::select) when the probe does not know a
    /// feature.
    pub fn probe_answers(&self, probe: NativeFeatureProbe) -> Result<ProbeAnswers, IsaLevelError> {
        let mut bits = 0_u64;
        for (index, feature) in self.iter().flat_map(IsaLevel::features).enumerate() {
            let answer = probe(feature.probe_name).ok_or(IsaLevelError::ProbeDoesNotAnswer {
                cranelift_flag: feature.cranelift_flag,
                probe_name: feature.probe_name,
            })?;
            assert!(
                index < u64::BITS as usize,
                "ISA levels define more than 64 features"
            );
            bits |= u64::from(answer) << index;
        }
        Ok(ProbeAnswers(bits))
    }

    fn names(&self) -> String {
        let mut names = String::new();
        for level in self.iter() {
            if !names.is_empty() {
                names.push_str(", ");
            }
            names.push_str(level.name);
        }
        names
    }
}

/// One processor's answers to every feature of a target's levels, in table
/// order. Two processors that run the same guest code must produce equal
/// values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeAnswers(u64);

/// A target triple together with a level proven to be one of its own.
#[derive(Clone, Copy, Debug)]
pub struct CwasmTarget<'a> {
    triple: &'a str,
    level: &'static IsaLevel,
}

impl<'a> CwasmTarget<'a> {
    /// Names the level `level` of `triple`'s chain.
    pub fn new(triple: &'a str, level: &str) -> Result<Self, IsaLevelError> {
        let level = cwasm_target_isa_levels(triple).by_name(level)?;
        Ok(Self { triple, level })
    }

    pub const fn triple(&self) -> &'a str {
        self.triple
    }

    pub const fn level(&self) -> &'static IsaLevel {
        self.level
    }

    /// The Cranelift flags that compile for the level, the floor's first.
    pub fn cranelift_flags(&self) -> impl Iterator<Item = &'static str> + use<> {
        self.level.features().map(IsaFeature::cranelift_flag)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IsaLevelError {
    #[error("no ISA level is named {name}; the target defines {defined}")]
    UnknownLevel { name: String, defined: String },
    #[error("the processor has no native feature probe, but the target's ISA levels need one")]
    NoProbe,
    #[error("the native feature probe does not answer {probe_name} for {cranelift_flag}")]
    ProbeDoesNotAnswer {
        cranelift_flag: &'static str,
        probe_name: &'static str,
    },
    #[error(
        "the processor does not satisfy the floor ISA level {level}: the probe reports \
         {probe_name} for {cranelift_flag} absent"
    )]
    FloorNotSatisfied {
        level: &'static str,
        cranelift_flag: &'static str,
        probe_name: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    const X86: &str = "x86_64-unknown-none";
    const AARCH64: &str = "aarch64-unknown-none";
    const RISCV: &str = "riscv64gc-unknown-none-elf";

    fn names(target: &str) -> Vec<&'static str> {
        cwasm_target_isa_levels(target)
            .iter()
            .map(IsaLevel::name)
            .collect()
    }

    #[test]
    fn chains_list_the_floor_first() {
        assert_eq!(
            names(X86),
            ["x86-64-v3", "x86-64-v4", "x86-64-v4+vbmi+bitalg"]
        );
        assert_eq!(
            names(AARCH64),
            ["aarch64-lse-fp16", "aarch64-lse-fp16-dotprod"]
        );
        assert_eq!(names(RISCV), ["riscv64gc"]);
        assert_eq!(
            cwasm_target_isa_levels("x86_64-unknown-linux-gnu")
                .floor()
                .name(),
            "x86-64-v3"
        );
    }

    #[test]
    fn each_level_nests_the_one_below() {
        for target in [X86, AARCH64, RISCV] {
            let levels = cwasm_target_isa_levels(target);
            let all: Vec<_> = levels.iter().collect();
            for (rank, level) in all.iter().enumerate() {
                assert_eq!(level.rank(), rank, "{}", level.name());
                for (other_rank, other) in all.iter().enumerate() {
                    assert_eq!(level.contains(other), other_rank <= rank);
                }
                if rank > 0 {
                    let below: Vec<_> = all[rank - 1].features().collect();
                    let own: Vec<_> = level.features().collect();
                    assert!(own.len() > below.len());
                    assert_eq!(own[..below.len()], below[..], "{}", level.name());
                }
            }
        }
    }

    #[test]
    fn floors_carry_the_flags_compilation_has_always_used() {
        const X86_FLOOR: &[&str] = &[
            "has_cmpxchg16b",
            "has_sse3",
            "has_ssse3",
            "has_sse41",
            "has_sse42",
            "has_popcnt",
            "has_bmi1",
            "has_bmi2",
            "has_lzcnt",
            "has_avx",
            "has_avx2",
            "has_fma",
        ];
        let expected: [(&str, &[&str]); 4] = [
            (X86, X86_FLOOR),
            ("x86_64-unknown-linux-gnu", X86_FLOOR),
            (AARCH64, &["has_lse", "has_fp16"]),
            (RISCV, &[]),
        ];
        for (target, flags) in expected {
            let floor = CwasmTarget::new(target, cwasm_target_isa_levels(target).floor().name())
                .expect("a floor belongs to its own target");
            assert_eq!(
                floor.cranelift_flags().collect::<Vec<_>>(),
                flags,
                "{target}"
            );
        }
    }

    #[test]
    fn top_levels_add_the_flags_in_order() {
        let x86 = CwasmTarget::new(X86, "x86-64-v4+vbmi+bitalg").unwrap();
        let flags: Vec<_> = x86.cranelift_flags().collect();
        assert_eq!(flags.len(), 17);
        assert_eq!(
            flags[12..],
            [
                "has_avx512f",
                "has_avx512vl",
                "has_avx512dq",
                "has_avx512vbmi",
                "has_avx512bitalg"
            ]
        );
        let aarch64 = CwasmTarget::new(AARCH64, "aarch64-lse-fp16-dotprod").unwrap();
        assert_eq!(
            aarch64.cranelift_flags().collect::<Vec<_>>(),
            ["has_lse", "has_fp16", "has_dotprod"]
        );
    }

    #[test]
    fn by_name_finds_a_level_or_lists_the_defined_ones() {
        let levels = cwasm_target_isa_levels(X86);
        assert_eq!(levels.by_name("x86-64-v4").unwrap().name(), "x86-64-v4");
        let error = levels.by_name("aarch64-lse-fp16").unwrap_err();
        assert_eq!(
            error,
            IsaLevelError::UnknownLevel {
                name: "aarch64-lse-fp16".into(),
                defined: "x86-64-v3, x86-64-v4, x86-64-v4+vbmi+bitalg".into(),
            }
        );
    }

    #[test]
    fn a_target_refuses_a_level_of_another_target() {
        assert!(matches!(
            CwasmTarget::new(X86, "aarch64-lse-fp16"),
            Err(IsaLevelError::UnknownLevel { .. })
        ));
        assert!(matches!(
            CwasmTarget::new(AARCH64, "x86-64-v3"),
            Err(IsaLevelError::UnknownLevel { .. })
        ));
        let target = CwasmTarget::new(X86, "x86-64-v4").unwrap();
        assert_eq!(target.triple(), X86);
        assert_eq!(target.level().name(), "x86-64-v4");
    }

    fn x86_all(_: &str) -> Option<bool> {
        Some(true)
    }

    fn x86_without_avx512(name: &str) -> Option<bool> {
        Some(!name.starts_with("avx512"))
    }

    fn x86_without_vbmi(name: &str) -> Option<bool> {
        Some(name != "avx512vbmi" && name != "avx512bitalg")
    }

    fn x86_without_avx2(name: &str) -> Option<bool> {
        Some(name != "avx2")
    }

    fn x86_without_bitalg_answer(name: &str) -> Option<bool> {
        (name != "avx512bitalg").then_some(true)
    }

    #[test]
    fn select_picks_the_highest_level_the_probe_satisfies() {
        let levels = cwasm_target_isa_levels(X86);
        assert_eq!(
            levels.select(Some(x86_all)).unwrap().name(),
            "x86-64-v4+vbmi+bitalg"
        );
        assert_eq!(
            levels.select(Some(x86_without_vbmi)).unwrap().name(),
            "x86-64-v4"
        );
        assert_eq!(
            levels.select(Some(x86_without_avx512)).unwrap().name(),
            "x86-64-v3"
        );
    }

    #[test]
    fn select_refuses_a_probe_that_does_not_answer_a_feature() {
        let levels = cwasm_target_isa_levels(X86);
        assert_eq!(
            levels.select(Some(x86_without_bitalg_answer)),
            Err(IsaLevelError::ProbeDoesNotAnswer {
                cranelift_flag: "has_avx512bitalg",
                probe_name: "avx512bitalg",
            })
        );
        assert_eq!(levels.select(None), Err(IsaLevelError::NoProbe));
    }

    #[test]
    fn select_names_the_missing_floor_feature() {
        assert_eq!(
            cwasm_target_isa_levels(X86).select(Some(x86_without_avx2)),
            Err(IsaLevelError::FloorNotSatisfied {
                level: "x86-64-v3",
                cranelift_flag: "has_avx2",
                probe_name: "avx2",
            })
        );
    }

    #[test]
    fn select_on_a_featureless_target_needs_no_probe() {
        let levels = cwasm_target_isa_levels(RISCV);
        assert_eq!(levels.select(None).unwrap().name(), "riscv64gc");
        assert_eq!(levels.select(Some(x86_all)).unwrap().name(), "riscv64gc");
    }

    #[test]
    fn aarch64_select_follows_dotprod() {
        fn without_dotprod(name: &str) -> Option<bool> {
            Some(name != "dotprod")
        }
        let levels = cwasm_target_isa_levels(AARCH64);
        assert_eq!(
            levels.select(Some(without_dotprod)).unwrap().name(),
            "aarch64-lse-fp16"
        );
        assert_eq!(
            levels.select(Some(x86_all)).unwrap().name(),
            "aarch64-lse-fp16-dotprod"
        );
    }

    #[test]
    fn probe_answers_differ_when_any_feature_differs() {
        let levels = cwasm_target_isa_levels(X86);
        let all = levels.probe_answers(x86_all).unwrap();
        assert_eq!(all, levels.probe_answers(x86_all).unwrap());
        assert_ne!(all, levels.probe_answers(x86_without_vbmi).unwrap());
        assert_ne!(
            levels.probe_answers(x86_without_vbmi).unwrap(),
            levels.probe_answers(x86_without_avx512).unwrap()
        );
        assert_eq!(
            levels.probe_answers(x86_without_bitalg_answer),
            Err(IsaLevelError::ProbeDoesNotAnswer {
                cranelift_flag: "has_avx512bitalg",
                probe_name: "avx512bitalg",
            })
        );
    }

    #[test]
    #[should_panic(expected = "no cwasm ISA levels are defined for target")]
    fn an_unsupported_target_has_no_levels() {
        cwasm_target_isa_levels("s390x-unknown-linux-gnu");
    }
}

//! NUMA locality from the ACPI System Resource Affinity Table (SRAT) and
//! System Locality Information Table (SLIT), ACPI 6.5 §5.2.16 and §5.2.17.
//!
//! Concurrency contract: construction reads the tables once on the
//! calling processor and keeps the SRAT mapping, a domain list and a
//! distance matrix; every method of the view afterwards only reads
//! those, so it is lock-free and callable from any processor.

use acpi::sdt::madt::{Madt, MadtEntry};
use acpi::sdt::slit::Slit;
use acpi::sdt::srat::{
    LocalApicAffinityFlags, MemoryAffinity as SratMemory, MemoryAffinityFlags, Srat, SratEntry,
    SratEntryIter,
};
use acpi::{AcpiTables, Handler, PhysicalMapping};
use arrayvec::ArrayVec;
use core::ops::Range;
use core::pin::Pin;
use helios_hal::cpu::ProcessorId;
use helios_hal::numa::{MAX_NODES, MemoryAffinity, NodeDistance, NodeId, NumaTopology};

use crate::ProcessorIdentities;
use crate::error::FirmwareNumaError;

/// The affinity fields of `MPIDR_EL1`: Aff0..Aff2 in bits 0..24 and Aff3
/// in bits 32..40. The MADT GICC entry reports a processor's MPIDR in the
/// same shape (ACPI 6.5, GICC Structure), but firmware is not required to zero
/// the bits outside them, and the register itself also holds the U and MT
/// bits; a processor is identified by these fields alone.
pub const MPIDR_AFFINITY_MASK: u64 = 0x0000_00ff_00ff_ffff;

/// GICC Affinity Structure flags bit 0, Enabled (ACPI 6.5, GICC Affinity
/// Flags).
const GICC_AFFINITY_ENABLED: u32 = 1 << 0;

/// An enabled SRAT processor entry: the key it names its processor by and
/// the proximity domain it places that processor in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SratProcessor {
    pub key: u64,
    pub domain: u32,
}

/// How an architecture's SRAT names a processor.
pub trait SratProcessorKey {
    /// The key `processor`'s SRAT entry names it by, from the hardware id
    /// the backend reports for it.
    fn srat_key(&self, processor: ProcessorId, hardware_id: u64) -> Result<u64, FirmwareNumaError>;

    /// `entry`, when it is an enabled processor entry of the kind this
    /// architecture's SRAT uses.
    fn processor_entry(entry: &SratEntry<'_>) -> Option<SratProcessor>;
}

/// x86: a processor is named by its local APIC id, in a Processor Local
/// APIC Affinity entry when the id fits in eight bits and a Processor
/// Local x2APIC Affinity entry otherwise. The backend's hardware ids are
/// APIC ids, so the key is the hardware id.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalApicKey;

impl SratProcessorKey for LocalApicKey {
    fn srat_key(
        &self,
        _processor: ProcessorId,
        hardware_id: u64,
    ) -> Result<u64, FirmwareNumaError> {
        Ok(hardware_id)
    }

    fn processor_entry(entry: &SratEntry<'_>) -> Option<SratProcessor> {
        match entry {
            SratEntry::LocalApicAffinity(apic)
                if { apic.flags }.contains(LocalApicAffinityFlags::ENABLED) =>
            {
                Some(SratProcessor {
                    key: u64::from(apic.apic_id),
                    domain: apic.proximity_domain(),
                })
            }
            SratEntry::LocalApicX2Affinity(apic)
                if { apic.flags }.contains(LocalApicAffinityFlags::ENABLED) =>
            {
                Some(SratProcessor {
                    key: u64::from(apic.x2apic_id),
                    domain: apic.proximity_domain,
                })
            }
            _ => None,
        }
    }
}

/// aarch64: a GICC Affinity entry names a processor by its ACPI processor
/// UID, which the MADT's GICC entry for that processor pairs with its
/// MPIDR. The backend's hardware ids are MPIDR affinity bits.
pub struct GiccUidKey<H: Handler> {
    madt: PhysicalMapping<H, Madt>,
}

impl<H: Handler> GiccUidKey<H> {
    pub fn new(tables: &AcpiTables<H>) -> Result<Self, FirmwareNumaError> {
        let madt = tables
            .find_table::<Madt>()
            .ok_or(FirmwareNumaError::MadtMissing)?;
        Ok(Self { madt })
    }
}

impl<H: Handler> SratProcessorKey for GiccUidKey<H> {
    fn srat_key(&self, processor: ProcessorId, hardware_id: u64) -> Result<u64, FirmwareNumaError> {
        self.madt
            .get()
            .entries()
            .find_map(|entry| match entry {
                MadtEntry::Gicc(gicc) if gicc.mpidr & MPIDR_AFFINITY_MASK == hardware_id => {
                    Some(u64::from(gicc.processor_uid))
                }
                _ => None,
            })
            .ok_or(FirmwareNumaError::ProcessorWithoutGicc {
                processor: processor.id(),
                mpidr: hardware_id,
            })
    }

    fn processor_entry(entry: &SratEntry<'_>) -> Option<SratProcessor> {
        match entry {
            SratEntry::GiccAffinity(gicc) if gicc.flags & GICC_AFFINITY_ENABLED != 0 => {
                Some(SratProcessor {
                    key: u64::from(gicc.acpi_processor_uid),
                    domain: gicc.proximity_domain,
                })
            }
            _ => None,
        }
    }
}

/// A machine's SRAT and SLIT, validated at construction so that no method
/// of the view can fail.
///
/// Without an SRAT, or with one that has no enabled processor or memory
/// entry, the machine has one node and answers as `SingleNode` does.
/// Without a SLIT, distinct nodes are `NodeDistance::UNIFORM_REMOTE`
/// apart.
pub struct AcpiNuma<H: Handler, P: ProcessorIdentities, K: SratProcessorKey> {
    srat: Option<PhysicalMapping<H, Srat>>,
    identities: P,
    key: K,
    /// The distinct proximity domains of the enabled processor and memory
    /// entries, ascending; index is the `NodeId`. Empty when there are
    /// none.
    domains: ArrayVec<u32, MAX_NODES>,
    /// Row-major distances by `NodeId`; zero is an entry no SLIT gave.
    distances: [[u8; MAX_NODES]; MAX_NODES],
}

impl<H: Handler, P: ProcessorIdentities, K: SratProcessorKey> AcpiNuma<H, P, K> {
    pub fn new(tables: &AcpiTables<H>, identities: P, key: K) -> Result<Self, FirmwareNumaError> {
        if identities.processor_count() == 0 {
            return Err(FirmwareNumaError::NoProcessors);
        }
        let mut this = Self {
            srat: tables.find_table::<Srat>(),
            identities,
            key,
            domains: ArrayVec::new(),
            distances: [[0; MAX_NODES]; MAX_NODES],
        };
        this.collect_domains()?;
        if this.domains.is_empty() {
            return Ok(this);
        }
        this.validate_memory()?;
        if let Some(slit) = tables.find_table::<Slit>() {
            this.read_distances(slit.get())?;
        }
        this.validate_processors()?;
        Ok(this)
    }

    fn entries(&self) -> impl Iterator<Item = SratEntry<'_>> {
        self.srat.iter().flat_map(|srat| srat.get().entries())
    }

    fn collect_domains(&mut self) -> Result<(), FirmwareNumaError> {
        let srat = self.srat.as_ref().map(PhysicalMapping::get);
        let entries = srat.into_iter().flat_map(Srat::entries);
        let domains = entries.filter_map(|entry| match K::processor_entry(&entry) {
            Some(processor) => Some(processor.domain),
            None => enabled_memory(&entry).map(|memory| memory.proximity_domain),
        });
        for domain in domains {
            if let Err(position) = self.domains.binary_search(&domain) {
                self.domains.try_insert(position, domain).map_err(|_| {
                    FirmwareNumaError::TooManyProximityDomains {
                        domain,
                        limit: MAX_NODES,
                    }
                })?;
            }
        }
        Ok(())
    }

    fn validate_memory(&self) -> Result<(), FirmwareNumaError> {
        for memory in self.entries().filter_map(|entry| enabled_memory(&entry)) {
            range_of(memory)?;
        }
        Ok(())
    }

    /// Validates the whole matrix as Linux's `slit_valid()` does, failing
    /// where Linux would ignore the table, and keeps the rows and columns
    /// of the SRAT's domains.
    fn read_distances(&mut self, slit: Pin<&Slit>) -> Result<(), FirmwareNumaError> {
        let localities = slit.num_proximity_domains;
        let truncated = FirmwareNumaError::SlitTruncated {
            localities,
            length: slit.header.length,
        };
        // A count whose square does not fit has no matrix either: the
        // crate answers it with the same empty one.
        let cells = localities.checked_mul(localities).ok_or(truncated)?;
        let matrix = slit.matrix();
        // The crate answers a table too short for its count with an empty
        // matrix; that is a malformed table, not a machine without
        // distances.
        if matrix.matrix.len() as u64 != cells {
            return Err(truncated);
        }
        // A matrix that fits in a table whose length is a `u32` has fewer
        // than 2^16 localities per side.
        let side = u32::try_from(localities).map_err(|_| truncated)?;
        let distance = |from: u32, to: u32| {
            matrix
                .distance(from, to)
                .expect("both localities are below the matrix side")
        };
        let local = NodeDistance::LOCAL.get();
        for from in 0..side {
            for to in 0..side {
                let value = distance(from, to);
                if from == to && value != local {
                    return Err(FirmwareNumaError::SlitDiagonal {
                        locality: from,
                        distance: value,
                    });
                }
                if from != to && value <= local {
                    return Err(FirmwareNumaError::SlitNotRemote {
                        from,
                        to,
                        distance: value,
                    });
                }
            }
        }
        if let Some(&domain) = self.domains.iter().find(|&&domain| domain >= side) {
            return Err(FirmwareNumaError::SlitMissingDomain { domain, localities });
        }
        for (row, &from) in self.domains.iter().enumerate() {
            for (column, &to) in self.domains.iter().enumerate() {
                self.distances[row][column] = distance(from, to);
            }
        }
        Ok(())
    }

    fn validate_processors(&self) -> Result<(), FirmwareNumaError> {
        for index in 0..self.identities.processor_count() {
            self.processor_domain(ProcessorId::new(index as u16))?;
        }
        Ok(())
    }

    /// The one proximity domain the SRAT's enabled entries give
    /// `processor`.
    fn processor_domain(&self, processor: ProcessorId) -> Result<u32, FirmwareNumaError> {
        let hardware_id = self.identities.hardware_id(processor);
        let key = self.key.srat_key(processor, hardware_id)?;
        let mut domains = self
            .entries()
            .filter_map(|entry| K::processor_entry(&entry))
            .filter(|entry| entry.key == key)
            .map(|entry| entry.domain);
        let first = domains
            .next()
            .ok_or(FirmwareNumaError::ProcessorWithoutAffinity {
                processor: processor.id(),
                hardware_id,
                key,
            })?;
        match domains.find(|&domain| domain != first) {
            Some(second) => Err(FirmwareNumaError::ProcessorAffinityConflict {
                processor: processor.id(),
                key,
                first,
                second,
            }),
            None => Ok(first),
        }
    }
}

impl<H: Handler, P: ProcessorIdentities, K: SratProcessorKey> NumaTopology for AcpiNuma<H, P, K> {
    type MemoryAffinities<'a>
        = AcpiMemory<'a>
    where
        Self: 'a;

    fn node_count(&self) -> usize {
        self.domains.len().max(1)
    }

    fn processor_count(&self) -> usize {
        self.identities.processor_count()
    }

    fn processor_node(&self, processor: ProcessorId) -> NodeId {
        if self.domains.is_empty() {
            return NodeId::FIRST;
        }
        let domain = self
            .processor_domain(processor)
            .expect("construction checked every processor has one enabled SRAT entry");
        node_of(&self.domains, domain)
    }

    fn distance(&self, from: NodeId, to: NodeId) -> NodeDistance {
        debug_assert!(
            from.index() < self.node_count() && to.index() < self.node_count(),
            "nodes {} and {} are beyond the {} the platform has",
            from.index(),
            to.index(),
            self.node_count()
        );
        match self.distances[from.index()][to.index()] {
            0 if from == to => NodeDistance::LOCAL,
            0 => NodeDistance::UNIFORM_REMOTE,
            raw => NodeDistance::new(raw),
        }
    }

    fn memory_affinity(&self) -> Self::MemoryAffinities<'_> {
        if self.domains.is_empty() {
            return AcpiMemory {
                entries: None,
                domains: &self.domains,
                whole: true,
            };
        }
        AcpiMemory {
            entries: self.srat.as_ref().map(|srat| srat.get().entries()),
            domains: &self.domains,
            whole: false,
        }
    }
}

/// Every enabled, non-empty SRAT Memory Affinity range with its node.
/// Hot-pluggable and non-volatile ranges are included: they place only
/// the bytes the boot memory map also lists as usable.
pub struct AcpiMemory<'a> {
    entries: Option<SratEntryIter<'a>>,
    domains: &'a ArrayVec<u32, MAX_NODES>,
    /// `SingleNode`'s answer is still to be given: all of physical memory
    /// on the one node.
    whole: bool,
}

impl Iterator for AcpiMemory<'_> {
    type Item = MemoryAffinity;

    fn next(&mut self) -> Option<MemoryAffinity> {
        if self.whole {
            self.whole = false;
            return Some(MemoryAffinity {
                physical: 0..u64::MAX,
                node: NodeId::FIRST,
            });
        }
        for entry in self.entries.as_mut()? {
            let Some(memory) = enabled_memory(&entry) else {
                continue;
            };
            let physical = range_of(memory).expect("construction checked every memory range");
            if physical.is_empty() {
                continue;
            }
            return Some(MemoryAffinity {
                physical,
                node: node_of(self.domains, memory.proximity_domain),
            });
        }
        None
    }
}

fn enabled_memory<'a>(entry: &SratEntry<'a>) -> Option<&'a SratMemory> {
    match entry {
        SratEntry::MemoryAffinity(memory)
            if { memory.flags }.contains(MemoryAffinityFlags::ENABLED) =>
        {
            Some(memory)
        }
        _ => None,
    }
}

fn range_of(memory: &SratMemory) -> Result<Range<u64>, FirmwareNumaError> {
    let (base, length) = (memory.base_address(), memory.length());
    let end = base
        .checked_add(length)
        .ok_or(FirmwareNumaError::SratMemoryRangeOverflow {
            domain: memory.proximity_domain,
            base,
            length,
        })?;
    Ok(base..end)
}

fn node_of(domains: &[u32], domain: u32) -> NodeId {
    let index = domains
        .binary_search(&domain)
        .expect("construction collected every enabled entry's proximity domain");
    NodeId::new(index)
}

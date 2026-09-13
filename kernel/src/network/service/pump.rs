use super::*;

#[derive(Clone, Copy)]
pub(super) struct NetworkPollBudget {
    pub(super) rx_frames: usize,
    pub(super) tx_completions: usize,
    pub(super) tx_frames: usize,
}

#[derive(Clone, Copy)]
pub(super) struct NetworkPollProgress {
    pub(super) received_frames: usize,
    pub(super) reclaimed_tx: usize,
    pub(super) transmitted_frames: usize,
}

#[derive(Clone, Copy)]
pub(super) struct NetworkTcpReadProbe {
    pub(super) stream: TcpStreamId,
    pub(super) max_bytes: usize,
    pub(super) profile_prefix: TcpReadPhasePrefix,
}

pub(super) struct NetworkPollOutcome {
    pub(super) progress: NetworkPollProgress,
    pub(super) budget: NetworkPollBudget,
    pub(super) tcp_read: Option<Result<TcpReadProgress, TcpError>>,
}

#[derive(Clone, Copy)]
pub(super) enum NetworkTransmitStop {
    Drained,
    Budget,
    RingFull,
}

/// The queue pair one processor's packet pump owns, decided once from
/// the topology rather than by whichever task happens to run first.
///
/// `pair` names everything the pump covers: the queue pair it drains
/// and submits on, and the shard whose arrival it parks on —
/// `shard == pair` in the shard-per-processor layout the service
/// builds. `interface_wide` marks the pump on the bootstrap
/// processor's pair: the only one that also carries the duties no
/// pair owns.
#[derive(Clone, Copy)]
pub struct NetworkPumpAssignment {
    pub(crate) pair: usize,
    pub(crate) interface_wide: bool,
}

#[derive(Clone, Copy)]
pub(super) enum NetworkPollSource {
    Pump,
    Ping,
    Dns,
    Tcp,
    Udp,
    Configuration,
}

/// What of the interface one poll round serves.
///
/// The split is between the packet pump and everyone else. A pump is
/// one queue pair's owner and works only what that pair carries; a
/// socket operation that polls is the stack's whole driver while it
/// runs, so it works everything.
#[derive(Clone, Copy)]
pub(super) enum NetworkPollScope {
    /// Every queue pair's rings and every shard's timers and egress,
    /// plus the interface-wide duties — what an operation driving the
    /// stack itself needs.
    Interface,
    /// One queue pair: its rings, and the shards whose egress submits
    /// on it. `interface_wide` is set on the pump that owns the
    /// bootstrap processor's pair alone — link and configuration
    /// events and control-plane publication belong to it, because
    /// they arrive on no pair at all.
    Pair { pair: usize, interface_wide: bool },
}

impl NetworkPollScope {
    /// Whether this poll acts on link and configuration state and
    /// republishes the control plane — true for operations, which keep
    /// their whole-interface shape, and for the bootstrap processor's
    /// pump, which carries the duties no pair owns.
    pub(super) const fn interface_wide(self) -> bool {
        match self {
            Self::Interface => true,
            Self::Pair { interface_wide, .. } => interface_wide,
        }
    }

    /// Whether this poll drives `shard_idx`'s timers and submits its
    /// egress. A pump serves the shards that submit on its pair — the
    /// ones whose index folds onto it — so with more processors than
    /// pairs the pair's pump still covers every shard the pair carries.
    pub(super) fn serves_shard(self, shard_idx: usize, pair_count: usize) -> bool {
        match self {
            Self::Interface => true,
            Self::Pair { pair, .. } => shard_idx % pair_count == pair,
        }
    }

    /// The queue pairs this poll drains and reclaims: an operation
    /// sweeps every pair starting at its own; a pump visits only the
    /// pair it owns.
    pub(super) fn pairs(self, local_pair: usize, pair_count: usize) -> NetworkPollPairs {
        match self {
            Self::Interface => NetworkPollPairs::Sweep(receive_pair_order(local_pair, pair_count)),
            Self::Pair { pair, .. } => NetworkPollPairs::One(core::iter::once(pair)),
        }
    }

    /// Whether the pair-agnostic async receive fallback is this scope's
    /// to use. The fallback drains pair zero under its async lock, so
    /// it belongs to the scope that owns pair zero — every operation,
    /// and the pump on pair zero — not to a pump whose pair somebody
    /// else is already draining.
    pub(super) const fn covers_pair_zero(self) -> bool {
        match self {
            Self::Interface => true,
            Self::Pair { pair, .. } => pair == 0,
        }
    }

    /// Whether this poll owes the interface's configuration state
    /// machines — the DHCP exchange and router solicitation — a drive.
    /// An operation that is configuring drives them itself through
    /// `drive_ipv4_configuration`; the duty otherwise belongs to the
    /// bootstrap processor's pump, which is the only task still
    /// advancing the interface when no socket is polling it.
    pub(super) const fn drives_configuration(self) -> bool {
        matches!(
            self,
            Self::Pair {
                interface_wide: true,
                ..
            }
        )
    }
}

/// The pair walk [`NetworkPollScope::pairs`] returns: either the
/// operation's sweep of every pair starting at its own, or the pump's
/// single pair.
pub(super) enum NetworkPollPairs {
    Sweep(ReceivePairOrder),
    One(core::iter::Once<usize>),
}

impl Iterator for NetworkPollPairs {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        match self {
            Self::Sweep(pairs) => pairs.next(),
            Self::One(pair) => pair.next(),
        }
    }
}

impl NetworkPollProgress {
    pub(super) const fn is_idle(self) -> bool {
        self.received_frames == 0 && self.reclaimed_tx == 0 && self.transmitted_frames == 0
    }

    pub(super) const fn receive_saturated(self, budget: NetworkPollBudget) -> bool {
        budget.rx_frames != 0 && self.received_frames >= budget.rx_frames
    }

    pub(super) const fn saturated(self, budget: NetworkPollBudget) -> bool {
        self.receive_saturated(budget)
            || self.reclaimed_tx >= budget.tx_completions
            || self.transmitted_frames >= budget.tx_frames
    }
}

impl NetworkTransmitStop {
    pub(super) const fn profile_phase(self, source: NetworkPollSource) -> &'static str {
        match (self, source) {
            (Self::Drained, NetworkPollSource::Pump) => "tx-submit-drained-pump",
            (Self::Drained, NetworkPollSource::Ping) => "tx-submit-drained-ping",
            (Self::Drained, NetworkPollSource::Dns) => "tx-submit-drained-dns",
            (Self::Drained, NetworkPollSource::Tcp) => "tx-submit-drained-tcp",
            (Self::Drained, NetworkPollSource::Udp) => "tx-submit-drained-udp",
            (Self::Drained, NetworkPollSource::Configuration) => "tx-submit-drained-configuration",
            (Self::Budget, NetworkPollSource::Pump) => "tx-submit-budget-pump",
            (Self::Budget, NetworkPollSource::Ping) => "tx-submit-budget-ping",
            (Self::Budget, NetworkPollSource::Dns) => "tx-submit-budget-dns",
            (Self::Budget, NetworkPollSource::Tcp) => "tx-submit-budget-tcp",
            (Self::Budget, NetworkPollSource::Udp) => "tx-submit-budget-udp",
            (Self::Budget, NetworkPollSource::Configuration) => "tx-submit-budget-configuration",
            (Self::RingFull, NetworkPollSource::Pump) => "tx-submit-ring-full-pump",
            (Self::RingFull, NetworkPollSource::Ping) => "tx-submit-ring-full-ping",
            (Self::RingFull, NetworkPollSource::Dns) => "tx-submit-ring-full-dns",
            (Self::RingFull, NetworkPollSource::Tcp) => "tx-submit-ring-full-tcp",
            (Self::RingFull, NetworkPollSource::Udp) => "tx-submit-ring-full-udp",
            (Self::RingFull, NetworkPollSource::Configuration) => {
                "tx-submit-ring-full-configuration"
            }
        }
    }
}

impl NetworkPollSource {
    pub(super) const fn tx_immediate_phase(self) -> &'static str {
        match self {
            Self::Pump => "tx-submit-immediate-pump",
            Self::Ping => "tx-submit-immediate-ping",
            Self::Dns => "tx-submit-immediate-dns",
            Self::Tcp => "tx-submit-immediate-tcp",
            Self::Udp => "tx-submit-immediate-udp",
            Self::Configuration => "tx-submit-immediate-configuration",
        }
    }

    pub(super) const fn tx_immediate_device_phase(self) -> &'static str {
        match self {
            Self::Pump => "tx-submit-immediate-device-pump",
            Self::Ping => "tx-submit-immediate-device-ping",
            Self::Dns => "tx-submit-immediate-device-dns",
            Self::Tcp => "tx-submit-immediate-device-tcp",
            Self::Udp => "tx-submit-immediate-device-udp",
            Self::Configuration => "tx-submit-immediate-device-configuration",
        }
    }

    pub(super) const fn rx_drain_phase(self) -> &'static str {
        match self {
            Self::Pump => "rx-drain-pump",
            Self::Ping => "rx-drain-ping",
            Self::Dns => "rx-drain-dns",
            Self::Tcp => "rx-drain-tcp",
            Self::Udp => "rx-drain-udp",
            Self::Configuration => "rx-drain-configuration",
        }
    }

    pub(super) const fn tx_reclaim_phase(self) -> &'static str {
        match self {
            Self::Pump => "tx-reclaim-pump",
            Self::Ping => "tx-reclaim-ping",
            Self::Dns => "tx-reclaim-dns",
            Self::Tcp => "tx-reclaim-tcp",
            Self::Udp => "tx-reclaim-udp",
            Self::Configuration => "tx-reclaim-configuration",
        }
    }

    pub(super) const fn tcp_drive_phase(self) -> &'static str {
        match self {
            Self::Pump => "tcp-drive-pump",
            Self::Ping => "tcp-drive-ping",
            Self::Dns => "tcp-drive-dns",
            Self::Tcp => "tcp-drive-tcp",
            Self::Udp => "tcp-drive-udp",
            Self::Configuration => "tcp-drive-configuration",
        }
    }
}

/// Adaptive poll budget. The base values are constants from device
/// capabilities; the live budgets are tuned in `complete()` based on
/// per-cycle progress so a saturated stack widens its budget while
/// an idle stack contracts it.
///
/// Reads happen on every network poll iteration. Storing the live
/// fields as `AtomicUsize` keeps that read off the lock — pre-Phase
/// 4.x the read had to acquire the global `SpinMutex<NetworkShard>`
/// just to copy three integers. Writes only happen in `complete()`,
/// which is racy by design (concurrent shards complete and update
/// the same atomic) but the outcome is a heuristic so the natural
/// last-writer-wins is acceptable.
pub(super) struct NetworkPollState {
    pub(super) base_rx_budget: usize,
    pub(super) base_tx_completion_budget: usize,
    pub(super) base_tx_frame_budget: usize,
    pub(super) rx_budget: AtomicUsize,
    pub(super) tx_completion_budget: AtomicUsize,
    pub(super) tx_frame_budget: AtomicUsize,
}

pub(super) struct NetworkPumpCadence {
    pub(super) busy_rounds: usize,
}

#[derive(Clone, Copy)]
pub(super) struct NetworkPerfStart {
    pub(super) nanos: u64,
    pub(super) counters: HardwarePerfCounters,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NetworkPumpAction {
    Continue,
    Yield,
    Wait,
}

impl<CpuImpl, DeviceImpl> NetworkService<CpuImpl, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    DeviceImpl: NetworkDevice,
{
    /// The queue pair a processor's packet pump owns, if it runs one.
    ///
    /// Pair `p` belongs to the lowest processor whose shard folds onto
    /// it: processor `i`'s shard is `i % shard_count` and a shard
    /// submits on pair `shard % pair_count`, so pair `p`'s candidate
    /// owners are processors `p`, `p + pair_count`, … and `p` itself is
    /// the smallest. With more processors than pairs the rest run no
    /// pump; with more pairs than processors the pairs past the shard
    /// set have no pump for the same reason `wake_queue_owners` gives
    /// them no IPI — the shard set is sized to the processors the
    /// machine has.
    ///
    /// The bootstrap processor's pump is identified through its pair:
    /// it is the pump on the pair the bootstrap processor's shard
    /// submits on, which is the bootstrap processor's own pump whenever
    /// it owns one.
    pub(crate) fn pump_assignment_for(
        &self,
        processor: helios_hal::cpu::ProcessorId,
    ) -> Option<NetworkPumpAssignment> {
        let pair_count = self.inner.device.queue_pair_count().max(1);
        let shard = self.inner.state.shard_idx_for_processor(processor);
        let pair = shard % pair_count;
        if usize::from(processor.id()) != pair {
            // This processor's shard submits on a pair a lower
            // processor already pumps.
            return None;
        }
        let bootstrap = self.inner.cpu.bootstrap_processor();
        let bootstrap_pair = self.inner.state.shard_idx_for_processor(bootstrap) % pair_count;
        Some(NetworkPumpAssignment {
            pair,
            interface_wide: pair == bootstrap_pair,
        })
    }

    /// Drives one queue pair for as long as the interface lives.
    ///
    /// There is one of these per queue pair, pinned by
    /// `spawn_local_detached` to the processor that owns the pair
    /// ([`Self::pump_assignment_for`]). The pump parks on
    /// `shard_wait(pair)`: the pair's event mark for work the device
    /// reports there, and the arrival of the shard its index names —
    /// the signal raised for a frame another processor drained into it
    /// and for work queued on any shard the pair serves. On a wake it
    /// drains its own pair only, hands each frame to the shard its
    /// flow hashes to through the existing demux, reclaims the pair's
    /// transmit completions, drives the TCP timers of the shards the
    /// pair serves, and submits their egress back onto the pair. It
    /// never sweeps the other pairs — that is the whole point (#378):
    /// a completion the host steered to pair `q` wakes pump `q` and
    /// nobody else.
    ///
    /// The pump on the bootstrap processor's pair additionally carries
    /// the duties no pair has — link and configuration events and
    /// control-plane publication — and is the only one that arms the
    /// device-wide channel to learn of them.
    pub async fn run_packet_pump(&self, assignment: NetworkPumpAssignment) -> ! {
        let NetworkPumpAssignment {
            pair,
            interface_wide,
        } = assignment;
        let scope = NetworkPollScope::Pair {
            pair,
            interface_wide,
        };
        let mut cadence = NetworkPumpCadence::new();
        // The adaptive budget is the pump's own: a pair running hot
        // widens its own drain rather than every pair's.
        let poll = NetworkPollState::new(
            self.inner.poll.base_rx_budget,
            self.inner.poll.base_tx_completion_budget,
            self.inner.poll.base_tx_frame_budget,
        );
        loop {
            // Like every other waiter, the pump samples the mark
            // *before* the poll that decides whether to park, so
            // progress made in between is not slept through.
            let wait = self.pump_shard_wait(pair, interface_wide);
            match self
                .poll_network_once_with_tcp_read(NetworkPollSource::Pump, None, true, scope, &poll)
                .await
            {
                Ok(outcome) => match cadence.complete(outcome.progress, outcome.budget) {
                    NetworkPumpAction::Continue => {}
                    NetworkPumpAction::Yield => crate::yield_now().await,
                    NetworkPumpAction::Wait => {
                        self.wait_for_shard_progress(wait, self.pump_wait(pair))
                            .await;
                    }
                },
                Err(error) => {
                    cadence.reset();
                    tracing::debug!(?error, "network packet pump failed to drive device");
                    self.wait_for_shard_progress(wait, self.pump_wait(pair))
                        .await;
                }
            }
        }
    }

    pub(super) async fn poll_network_once(
        &self,
        source: NetworkPollSource,
    ) -> Result<(NetworkPollProgress, NetworkPollBudget), IoError> {
        let outcome = self
            .poll_network_once_with_tcp_read(
                source,
                None,
                true,
                NetworkPollScope::Interface,
                &self.inner.poll,
            )
            .await?;
        Ok((outcome.progress, outcome.budget))
    }

    pub(super) async fn poll_network_receive_once(
        &self,
        source: NetworkPollSource,
    ) -> Result<(NetworkPollProgress, NetworkPollBudget), IoError> {
        let outcome = self
            .poll_network_once_with_tcp_read(
                source,
                None,
                false,
                NetworkPollScope::Interface,
                &self.inner.poll,
            )
            .await?;
        Ok((outcome.progress, outcome.budget))
    }

    /// Drains the egress this poll scope owns onto its ring and rings
    /// the doorbell: every shard's for an operation driving the
    /// interface, the shards that submit on the pump's pair for a
    /// pump.
    ///
    /// Synchronous because nothing in it awaits: the stack's outbound
    /// drain and the device's `try_lock` submit both complete where they
    /// are called, which is what lets a non-blocking socket `write`
    /// publish its own segment on the caller's task.
    pub(super) fn submit_network_transmit(
        &self,
        source: NetworkPollSource,
        budget: NetworkPollBudget,
        scope: NetworkPollScope,
    ) -> Result<(usize, usize), IoError> {
        let mut transmitted = 0usize;
        let mut transmitted_bytes = 0usize;
        let transmit_started = self.profile_start();
        let mut transmit_stop = NetworkTransmitStop::Drained;
        let shard_count = self.inner.state.shard_count();
        let pair_count = self.inner.device.queue_pair_count().max(1);
        while transmitted < budget.tx_frames {
            let mut immediate_submitted = false;
            for shard_idx in 0..shard_count {
                if !scope.serves_shard(shard_idx, pair_count) {
                    continue;
                }
                if transmitted >= budget.tx_frames {
                    break;
                }
                let remaining_budget = budget.tx_frames - transmitted;
                let immediate_started = self.profile_start();
                let mut immediate_device_started = None;
                let mut immediate_device_finished = None;
                let immediate = {
                    let mut state = self.inner.state.shard_at(shard_idx).lock();
                    state.stack.try_submit_outbound_slices(
                        remaining_budget.min(NETWORK_TX_BATCH_FRAMES),
                        |frames| {
                            immediate_device_started = self.profile_start();
                            // Zero-copy: only the header prefix is
                            // copied into the device's slot, and the
                            // device keeps each accepted payload's
                            // handle until its descriptor completes, so
                            // releasing the outbound slot here is safe.
                            // A shard submits on the pair its index
                            // folds onto, which is also how the scope's
                            // `serves_shard` picked this shard.
                            let result = self
                                .inner
                                .device
                                .try_transmit_scatter_immediate_on(shard_idx % pair_count, frames);
                            immediate_device_finished = self.profile_start();
                            result
                        },
                    )
                }?;
                match immediate {
                    // `Deferred`: another processor holds this queue
                    // pair's ring and is already draining the same
                    // frames. Nothing to do but come back.
                    OutboundBatchStatus::Empty | OutboundBatchStatus::Deferred => {}
                    OutboundBatchStatus::Submitted {
                        offered,
                        accepted,
                        accepted_bytes,
                    } => {
                        immediate_submitted = true;
                        self.record_network_profile_events_bytes_between(
                            source.tx_immediate_device_phase(),
                            immediate_device_started,
                            immediate_device_finished,
                            accepted,
                            accepted_bytes,
                        );
                        self.record_network_profile_events_bytes(
                            source.tx_immediate_phase(),
                            immediate_started,
                            accepted,
                            accepted_bytes,
                        );
                        self.inner.state.record_transmitted(shard_idx, accepted);
                        transmitted += accepted;
                        transmitted_bytes = transmitted_bytes.saturating_add(accepted_bytes);
                        if accepted < offered {
                            transmit_stop = NetworkTransmitStop::RingFull;
                            break;
                        }
                    }
                }
            }
            if matches!(transmit_stop, NetworkTransmitStop::RingFull) {
                break;
            }
            if !immediate_submitted {
                break;
            }
        }
        if transmitted >= budget.tx_frames {
            transmit_stop = NetworkTransmitStop::Budget;
        }
        if transmitted != 0 {
            self.record_network_profile_events_bytes(
                transmit_stop.profile_phase(source),
                transmit_started,
                transmitted,
                transmitted_bytes,
            );
        }
        Ok((transmitted, transmitted_bytes))
    }
}

impl<CpuImpl, DeviceImpl> crate::NetworkPacketPump for NetworkService<CpuImpl, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    DeviceImpl: NetworkDevice,
{
    async fn run_packet_pump(&self, processor: helios_hal::cpu::ProcessorId) {
        let Some(assignment) = self.pump_assignment_for(processor) else {
            return;
        };
        self.run_packet_pump(assignment).await;
    }
}

impl NetworkShard {
    pub(super) fn retransmit_dhcp(&mut self, now: StackInstant) -> Result<(), NetworkControlError> {
        match self.dhcp {
            DhcpClientState::Selecting {
                transaction_id,
                last_sent,
            } if now.nanos().saturating_sub(last_sent.nanos()) >= DHCP_RETRANSMIT_NANOS => {
                self.send_dhcp_discover(transaction_id, now)?;
                self.dhcp = DhcpClientState::Selecting {
                    transaction_id,
                    last_sent: now,
                };
            }
            DhcpClientState::Requesting {
                transaction_id,
                requested_ip,
                server_identifier,
                last_sent,
            } if now.nanos().saturating_sub(last_sent.nanos()) >= DHCP_RETRANSMIT_NANOS => {
                self.send_dhcp_request(transaction_id, requested_ip, server_identifier, now)?;
                self.dhcp = DhcpClientState::Requesting {
                    transaction_id,
                    requested_ip,
                    server_identifier,
                    last_sent: now,
                };
            }
            _ => {}
        }
        Ok(())
    }
}

impl NetworkPollState {
    pub(super) fn new(
        rx_budget: usize,
        tx_completion_budget: usize,
        tx_frame_budget: usize,
    ) -> Self {
        let rx_budget = clamp_poll_budget(rx_budget);
        let tx_completion_budget = clamp_poll_budget(tx_completion_budget);
        let tx_frame_budget = clamp_poll_budget(tx_frame_budget);
        Self {
            base_rx_budget: rx_budget,
            base_tx_completion_budget: tx_completion_budget,
            base_tx_frame_budget: tx_frame_budget,
            rx_budget: AtomicUsize::new(rx_budget),
            tx_completion_budget: AtomicUsize::new(tx_completion_budget),
            tx_frame_budget: AtomicUsize::new(tx_frame_budget),
        }
    }

    pub(super) fn budget(&self) -> NetworkPollBudget {
        NetworkPollBudget {
            rx_frames: self.rx_budget.load(AtomicOrdering::Relaxed),
            tx_completions: self.tx_completion_budget.load(AtomicOrdering::Relaxed),
            tx_frames: self.tx_frame_budget.load(AtomicOrdering::Relaxed),
        }
    }

    pub(super) fn complete(&self, progress: NetworkPollProgress) {
        let rx_current = self.rx_budget.load(AtomicOrdering::Relaxed);
        self.rx_budget.store(
            adjust_poll_budget(
                rx_current,
                self.base_rx_budget,
                progress.received_frames >= rx_current,
                progress.is_idle(),
            ),
            AtomicOrdering::Relaxed,
        );
        let tx_completion_current = self.tx_completion_budget.load(AtomicOrdering::Relaxed);
        self.tx_completion_budget.store(
            adjust_poll_budget(
                tx_completion_current,
                self.base_tx_completion_budget,
                progress.reclaimed_tx >= tx_completion_current,
                progress.is_idle(),
            ),
            AtomicOrdering::Relaxed,
        );
        let tx_frame_current = self.tx_frame_budget.load(AtomicOrdering::Relaxed);
        self.tx_frame_budget.store(
            adjust_poll_budget(
                tx_frame_current,
                self.base_tx_frame_budget,
                progress.transmitted_frames >= tx_frame_current,
                progress.is_idle(),
            ),
            AtomicOrdering::Relaxed,
        );
    }
}

impl NetworkPumpCadence {
    pub(super) const fn new() -> Self {
        Self { busy_rounds: 0 }
    }

    pub(super) fn complete(
        &mut self,
        progress: NetworkPollProgress,
        budget: NetworkPollBudget,
    ) -> NetworkPumpAction {
        if progress.is_idle() {
            self.reset();
            return NetworkPumpAction::Wait;
        }

        self.busy_rounds = self.busy_rounds.saturating_add(1);
        if self.busy_rounds >= NETWORK_BUSY_POLL_ROUNDS {
            self.reset();
            return NetworkPumpAction::Yield;
        }

        if progress.saturated(budget) {
            return NetworkPumpAction::Continue;
        }

        NetworkPumpAction::Continue
    }

    pub(super) fn reset(&mut self) {
        self.busy_rounds = 0;
    }
}

pub(super) fn adjust_poll_budget(
    current: usize,
    base: usize,
    saturated: bool,
    idle: bool,
) -> usize {
    if saturated {
        return clamp_poll_budget(current.saturating_mul(2));
    }
    if idle && current > base {
        return current / 2;
    }
    current
}

/// TCP read call sites that record profile phases. Typed so a new read
/// path cannot reach profiling without extending the exhaustive phase
/// table below; the old stringly table panicked at runtime when the
/// registered-buffer reads introduced prefixes it never listed.
#[derive(Clone, Copy, Debug)]
pub(super) enum TcpReadPhasePrefix {
    Initial,
    AfterDrive,
    Polling,
    IntoInitial,
    IntoAfterDrive,
    IntoPolling,
}

/// Read progress outcome folded into the recorded phase name.
#[derive(Clone, Copy, Debug)]
pub(super) enum TcpReadPhaseOutcome {
    Pending,
    Ready,
    Eof,
}

pub(super) const fn tcp_read_profile_phase(
    prefix: TcpReadPhasePrefix,
    outcome: TcpReadPhaseOutcome,
) -> &'static str {
    use TcpReadPhaseOutcome as Outcome;
    use TcpReadPhasePrefix as Prefix;
    match (prefix, outcome) {
        (Prefix::Initial, Outcome::Pending) => "tcp-read-initial-pending",
        (Prefix::Initial, Outcome::Ready) => "tcp-read-initial-ready",
        (Prefix::Initial, Outcome::Eof) => "tcp-read-initial-eof",
        (Prefix::AfterDrive, Outcome::Pending) => "tcp-read-after-drive-pending",
        (Prefix::AfterDrive, Outcome::Ready) => "tcp-read-after-drive-ready",
        (Prefix::AfterDrive, Outcome::Eof) => "tcp-read-after-drive-eof",
        (Prefix::Polling, Outcome::Pending) => "tcp-read-polling-pending",
        (Prefix::Polling, Outcome::Ready) => "tcp-read-polling-ready",
        (Prefix::Polling, Outcome::Eof) => "tcp-read-polling-eof",
        (Prefix::IntoInitial, Outcome::Pending) => "tcp-read-into-initial-pending",
        (Prefix::IntoInitial, Outcome::Ready) => "tcp-read-into-initial-ready",
        (Prefix::IntoInitial, Outcome::Eof) => "tcp-read-into-initial-eof",
        (Prefix::IntoAfterDrive, Outcome::Pending) => "tcp-read-into-after-drive-pending",
        (Prefix::IntoAfterDrive, Outcome::Ready) => "tcp-read-into-after-drive-ready",
        (Prefix::IntoAfterDrive, Outcome::Eof) => "tcp-read-into-after-drive-eof",
        (Prefix::IntoPolling, Outcome::Pending) => "tcp-read-into-polling-pending",
        (Prefix::IntoPolling, Outcome::Ready) => "tcp-read-into-polling-ready",
        (Prefix::IntoPolling, Outcome::Eof) => "tcp-read-into-polling-eof",
    }
}

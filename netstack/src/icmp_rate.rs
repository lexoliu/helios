//! The rate limit on ICMP and ICMPv6 messages this stack originates.

use crate::StackInstant;

/// Messages per second one bucket refills at.
///
/// FreeBSD's `net.inet.icmp.icmplim` default (`ip_icmp.c`), which
/// `badport_bandlim` applies to each class of response on its own. No
/// per-peer limit is kept: the stack holds no per-peer state, and RFC
/// 4443 section 2.4 (f) and RFC 1812 section 4.3.2.8 both allow a limit
/// that bounds the total.
pub(crate) const ICMP_MESSAGES_PER_SECOND: u32 = 200;

/// Messages one burst may send back to back: a whole second's worth,
/// since FreeBSD counts each class over a one-second window and lets
/// the window's allowance go at once.
pub(crate) const ICMP_BURST_MESSAGES: u32 = ICMP_MESSAGES_PER_SECOND;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Time one token takes to accrue.
const NANOS_PER_TOKEN: u64 = NANOS_PER_SECOND / ICMP_MESSAGES_PER_SECOND as u64;

const _: () = assert!(
    NANOS_PER_SECOND.is_multiple_of(ICMP_MESSAGES_PER_SECOND as u64),
    "the refill rate must give a whole number of nanoseconds per token"
);

/// A token bucket for one class of ICMP and ICMPv6 message the stack
/// sends of its own accord. A stack keeps one for the errors it
/// originates (RFC 4443 section 2.4 (f), RFC 1812 section 4.3.2.8) and
/// one for the echo replies it gives, as FreeBSD keeps a counter per
/// class (`BANDLIM_ICMP_UNREACH`, `BANDLIM_ICMP_ECHO`), so a flood of
/// echo requests never silences the errors and the other way round.
///
/// Concurrency: a plain value owned by one [`Stack`](crate::Stack), so by
/// exactly one network shard, and mutated behind `&mut self`. Each shard
/// has a bucket of its own; nothing is shared, atomic or padded.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IcmpRateLimit {
    tokens: u32,
    /// The instant up to which elapsed time has been turned into tokens.
    refilled_at: u64,
}

impl IcmpRateLimit {
    /// A full bucket.
    pub(crate) const fn new() -> Self {
        Self {
            tokens: ICMP_BURST_MESSAGES,
            refilled_at: 0,
        }
    }

    /// Takes one token for a message sent at `now`, or reports that the
    /// message must not be sent.
    pub(crate) fn try_acquire(&mut self, now: StackInstant) -> bool {
        self.refill(now);
        let Some(tokens) = self.tokens.checked_sub(1) else {
            return false;
        };
        self.tokens = tokens;
        true
    }

    fn refill(&mut self, now: StackInstant) {
        let now = now.nanos();
        let accrued = now.saturating_sub(self.refilled_at) / NANOS_PER_TOKEN;
        let tokens = u64::from(self.tokens).saturating_add(accrued);
        if tokens >= u64::from(ICMP_BURST_MESSAGES) {
            self.tokens = ICMP_BURST_MESSAGES;
            self.refilled_at = self.refilled_at.max(now);
        } else {
            // Below the cap `accrued` is under the burst size, so the
            // narrowing and the product cannot overflow.
            self.tokens = tokens as u32;
            self.refilled_at += accrued * NANOS_PER_TOKEN;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(nanos: u64) -> StackInstant {
        StackInstant::from_nanos(nanos)
    }

    #[test]
    fn a_burst_is_allowed_and_the_next_message_is_not() {
        let mut limit = IcmpRateLimit::new();
        for _ in 0..ICMP_BURST_MESSAGES {
            assert!(limit.try_acquire(at(5)));
        }
        assert!(!limit.try_acquire(at(5)));
    }

    #[test]
    fn tokens_accrue_at_the_configured_rate() {
        let mut limit = IcmpRateLimit::new();
        for _ in 0..ICMP_BURST_MESSAGES {
            assert!(limit.try_acquire(at(0)));
        }
        assert!(!limit.try_acquire(at(NANOS_PER_TOKEN - 1)));
        assert!(limit.try_acquire(at(NANOS_PER_TOKEN)));
        assert!(!limit.try_acquire(at(NANOS_PER_TOKEN)));
        // A whole second refills as many tokens as the rate says, not
        // more than the bucket holds.
        let mut allowed = 0;
        while limit.try_acquire(at(NANOS_PER_TOKEN + NANOS_PER_SECOND)) {
            allowed += 1;
        }
        assert_eq!(allowed, ICMP_BURST_MESSAGES);
    }

    #[test]
    fn a_partial_interval_is_not_lost() {
        let mut limit = IcmpRateLimit::new();
        for _ in 0..ICMP_BURST_MESSAGES {
            assert!(limit.try_acquire(at(0)));
        }
        // Half an interval accrues nothing, and what it accrued is kept
        // toward the token that the whole interval completes.
        assert!(!limit.try_acquire(at(NANOS_PER_TOKEN / 2)));
        assert!(limit.try_acquire(at(NANOS_PER_TOKEN)));
        assert!(!limit.try_acquire(at(NANOS_PER_TOKEN + NANOS_PER_TOKEN / 2)));
        assert!(limit.try_acquire(at(2 * NANOS_PER_TOKEN)));
    }

    #[test]
    fn a_clock_that_steps_back_changes_nothing() {
        let mut limit = IcmpRateLimit::new();
        for _ in 0..ICMP_BURST_MESSAGES {
            assert!(limit.try_acquire(at(10 * NANOS_PER_SECOND)));
        }
        assert!(!limit.try_acquire(at(NANOS_PER_SECOND)));
    }
}

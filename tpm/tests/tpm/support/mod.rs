//! What the transport and device tests share: a clock the test drives,
//! an executor that counts suspensions, register models of both
//! interfaces that fail the test on any access out of protocol order,
//! and the fixture recorded from `swtpm`.

pub mod crb;
pub mod tis;

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use helios_hal::cpu::{Instant, Timebase};
use helios_tpm::{Clock, Transport, TransportError};
use serde::{Deserialize, Serialize};

/// A clock that advances one millisecond every time it is read, so a
/// wait that never succeeds reaches its deadline after as many reads as
/// its timeout has milliseconds.
#[derive(Default)]
pub struct TestClock {
    ticks: AtomicU64,
}

/// One tick per millisecond.
const TEST_CLOCK_HZ: u64 = 1_000;

impl Clock for TestClock {
    fn now(&self) -> Instant {
        Instant::new(self.ticks.fetch_add(1, Ordering::Relaxed))
    }

    fn timebase(&self) -> Timebase {
        Timebase::new(TEST_CLOCK_HZ)
    }
}

/// Runs `future` to completion and reports how many times it
/// suspended. Every suspension in this crate is a yield that wakes
/// itself, so polling again at once is what any executor would do.
pub fn run<F: Future>(future: F) -> (F::Output, usize) {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    let mut suspensions = 0;
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return (output, suspensions),
            Poll::Pending => suspensions += 1,
        }
    }
}

pub fn complete<F: Future>(future: F) -> F::Output {
    run(future).0
}

/// What answers a command once an interface model has carried it.
pub trait Core: Send {
    fn execute(&mut self, command: &[u8]) -> Vec<u8>;
}

/// Answers every command with its own bytes: a transport test checks
/// framing and sequencing, and a command frame carries the same
/// header-size field a response does.
pub struct Loopback;

impl Core for Loopback {
    fn execute(&mut self, command: &[u8]) -> Vec<u8> {
        command.to_vec()
    }
}

/// One command and the response `swtpm` gave it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Exchange {
    #[serde(with = "hex::serde")]
    pub command: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub response: Vec<u8>,
}

/// The exchanges of one phase of the recorded scenario.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Phase {
    pub name: String,
    pub exchanges: Vec<Exchange>,
}

/// The values the scenario was recorded with.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inputs {
    #[serde(with = "hex::serde")]
    pub first_secret: [u8; 32],
    #[serde(with = "hex::serde")]
    pub second_secret: [u8; 32],
    #[serde(with = "hex::serde")]
    pub forged_secret: [u8; 32],
    #[serde(with = "hex::serde")]
    pub third_secret: [u8; 32],
    #[serde(with = "hex::serde")]
    pub extend_digest: [u8; 32],
}

/// The fixture `record_swtpm_fixtures` writes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recording {
    /// `swtpm --version`, for provenance.
    pub swtpm: String,
    pub inputs: Inputs,
    pub phases: Vec<Phase>,
}

pub fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/swtpm.json")
}

pub fn load_recording() -> Recording {
    let path = fixture_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the swtpm fixture {} cannot be read ({error}); record it with \
             `HELIOS_SWTPM=$(command -v swtpm) cargo nextest run -p helios-tpm \
             --run-ignored only -E 'test(=record_swtpm_fixtures)'`",
            path.display()
        )
    });
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not a recording: {error}", path.display()))
}

impl Recording {
    pub fn phase(&self, name: &str) -> Vec<Exchange> {
        self.phases
            .iter()
            .find(|phase| phase.name == name)
            .unwrap_or_else(|| panic!("the recording has no phase {name:?}"))
            .exchanges
            .clone()
    }
}

/// Plays a recorded phase back: each command must be byte-for-byte the
/// one recorded, and is answered with the recorded response.
#[derive(Clone)]
pub struct Replay {
    remaining: Arc<Mutex<VecDeque<Exchange>>>,
}

impl Replay {
    pub fn new(exchanges: Vec<Exchange>) -> Self {
        Self {
            remaining: Arc::new(Mutex::new(exchanges.into())),
        }
    }

    pub fn answer(&self, command: &[u8]) -> Vec<u8> {
        let mut remaining = self.remaining.lock().unwrap();
        let expected = remaining.pop_front().unwrap_or_else(|| {
            panic!(
                "command {} was sent after the recording ended",
                hex::encode(command)
            )
        });
        assert_eq!(
            hex::encode(command),
            hex::encode(&expected.command),
            "the command differs from the one recorded"
        );
        expected.response
    }

    /// Fails the test if any recorded exchange was not replayed.
    pub fn finish(&self) {
        let remaining = self.remaining.lock().unwrap();
        assert!(
            remaining.is_empty(),
            "{} recorded exchanges were never sent; the next is {}",
            remaining.len(),
            remaining
                .front()
                .map(|exchange| hex::encode(&exchange.command))
                .unwrap_or_default()
        );
    }
}

impl Core for Replay {
    fn execute(&mut self, command: &[u8]) -> Vec<u8> {
        self.answer(command)
    }
}

impl Transport for Replay {
    async fn transact(
        &mut self,
        command: &[u8],
        response: &mut [u8],
    ) -> Result<usize, TransportError> {
        let answer = self.answer(command);
        response[..answer.len()].copy_from_slice(&answer);
        Ok(answer.len())
    }
}

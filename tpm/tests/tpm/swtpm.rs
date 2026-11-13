//! The measured-boot scenario against a real TPM.
//!
//! `record_swtpm_fixtures` runs the scenario against `swtpm` and writes
//! every command and response to `tests/fixtures/swtpm.json`, asserting
//! each outcome as the TPM gives it. The other tests replay that file:
//! every command the crate builds must equal the recorded one byte for
//! byte, and every recorded response must parse into the outcome the
//! TPM gave when it was recorded.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use helios_hal::measured_boot::{
    MeasuredBoot, MeasurementRegister, MeasurementSet, SealedSecret, Sha256Digest, UnsealRefused,
};
use helios_tpm::{Crb, SLOT_INDEX, Tis, Tpm, Transport, TransportError};
use tpm2_protocol::TpmWriter;
use tpm2_protocol::basic::{TpmBuffer, TpmHandle, TpmUint16};
use tpm2_protocol::data::{
    Tpm2bNvPublic, TpmAlgId, TpmRc, TpmRcBase, TpmRh, TpmSt, TpmaNv, TpmaSession, TpmsAuthCommand,
    TpmsNvPublic,
};
use tpm2_protocol::frame::{
    TpmFrame, TpmNvDefineSpaceCommand, TpmNvUndefineSpaceCommand, TpmNvWriteCommand, TpmResponse,
    tpm_marshal_command,
};

use crate::support::crb::CrbModel;
use crate::support::tis::TisModel;
use crate::support::{
    Exchange, Inputs, Phase, Recording, Replay, TestClock, complete, fixture_path, load_recording,
};

const PCR4: MeasurementRegister = MeasurementRegister::new(4);
const PCR9: MeasurementRegister = MeasurementRegister::new(9);
/// Firmware measures the loader into PCR 4 and the loader measures the
/// kernel into PCR 9: the set the slot is sealed to.
const KERNEL_IMAGE: MeasurementSet = MeasurementSet::of(&[PCR4, PCR9]);

const SEAL: &str = "seal";
const PLANT_FOREIGN: &str = "plant-foreign";
const REPLACE_FOREIGN: &str = "replace-foreign";

/// Seals, reads back, changes a register, finds the secret refused,
/// and seals again in the new state.
async fn seal<T: Transport + 'static>(transport: T, inputs: &Inputs) {
    let mut tpm = Tpm::open(transport, KERNEL_IMAGE)
        .await
        .expect("the TPM keeps PCRs 4 and 9 in a SHA-256 bank");
    assert_eq!(
        tpm.load_sealed(KERNEL_IMAGE).await.unwrap_err(),
        UnsealRefused::Empty
    );

    tpm.store_sealed(KERNEL_IMAGE, &SealedSecret::new(inputs.first_secret))
        .await
        .unwrap();
    let loaded = tpm.load_sealed(KERNEL_IMAGE).await.unwrap();
    assert_eq!(loaded.expose(), &inputs.first_secret);

    tpm.extend(PCR9, &Sha256Digest(inputs.extend_digest))
        .await
        .unwrap();
    assert_eq!(
        tpm.load_sealed(KERNEL_IMAGE).await.unwrap_err(),
        UnsealRefused::StateChanged
    );

    tpm.store_sealed(KERNEL_IMAGE, &SealedSecret::new(inputs.second_secret))
        .await
        .unwrap();
    let loaded = tpm.load_sealed(KERNEL_IMAGE).await.unwrap();
    assert_eq!(loaded.expose(), &inputs.second_secret);
}

/// Sends one command with the owner's empty password and requires
/// success.
async fn owner_command<T: Transport, C: TpmFrame>(transport: &mut T, command: &C) {
    let session = TpmsAuthCommand {
        session_handle: TpmHandle::new(TpmRh::Pw.value()),
        nonce: TpmBuffer::new(),
        session_attributes: TpmaSession::CONTINUE_SESSION,
        hmac: TpmBuffer::new(),
    };
    let mut frame = [0; 512];
    let mut writer = TpmWriter::new(&mut frame);
    tpm_marshal_command(command, TpmSt::Sessions, &[session], &mut writer).unwrap();
    let len = writer.len();
    let mut response = [0; 512];
    let received = transport
        .transact(&frame[..len], &mut response)
        .await
        .unwrap();
    let response = TpmResponse::cast(&response[..received]).unwrap();
    assert_eq!(response.rc().unwrap(), TpmRc::Fmt0(TpmRcBase::Success));
}

/// Replaces the slot with a look-alike a forger can write with the
/// owner's empty authorization, and writes a forged secret into it.
async fn plant_foreign<T: Transport>(mut transport: T, inputs: &Inputs) {
    let owner = TpmHandle::new(TpmRh::Owner.value());
    let index = TpmHandle::new(SLOT_INDEX);
    owner_command(
        &mut transport,
        &TpmNvUndefineSpaceCommand {
            handles: [owner, index],
        },
    )
    .await;
    owner_command(
        &mut transport,
        &TpmNvDefineSpaceCommand {
            handles: [owner],
            auth: TpmBuffer::new(),
            public_info: Tpm2bNvPublic {
                inner: TpmsNvPublic {
                    nv_index: index,
                    name_alg: TpmAlgId::Sha256,
                    attributes: TpmaNv::OWNERREAD | TpmaNv::OWNERWRITE | TpmaNv::NO_DA,
                    auth_policy: TpmBuffer::new(),
                    data_size: TpmUint16::new(SealedSecret::LEN as u16),
                },
            },
        },
    )
    .await;
    owner_command(
        &mut transport,
        &TpmNvWriteCommand {
            handles: [owner, index],
            data: TpmBuffer::try_from(&inputs.forged_secret[..]).unwrap(),
            offset: TpmUint16::new(0),
        },
    )
    .await;
}

/// Finds the look-alike refused and replaces it with a slot of its own.
async fn replace_foreign<T: Transport + 'static>(transport: T, inputs: &Inputs) {
    let mut tpm = Tpm::open(transport, KERNEL_IMAGE).await.unwrap();
    assert_eq!(
        tpm.load_sealed(KERNEL_IMAGE).await.unwrap_err(),
        UnsealRefused::ForeignSlot
    );
    tpm.store_sealed(KERNEL_IMAGE, &SealedSecret::new(inputs.third_secret))
        .await
        .unwrap();
    let loaded = tpm.load_sealed(KERNEL_IMAGE).await.unwrap();
    assert_eq!(loaded.expose(), &inputs.third_secret);
}

/// A `swtpm` data socket that keeps every exchange it carries.
#[derive(Clone)]
struct Recorder {
    stream: Arc<Mutex<UnixStream>>,
    exchanges: Arc<Mutex<Vec<Exchange>>>,
}

impl Recorder {
    fn take(&self) -> Vec<Exchange> {
        std::mem::take(&mut self.exchanges.lock().unwrap())
    }
}

impl Transport for Recorder {
    async fn transact(
        &mut self,
        command: &[u8],
        response: &mut [u8],
    ) -> Result<usize, TransportError> {
        let mut stream = self.stream.lock().unwrap();
        stream.write_all(command).unwrap();
        let mut header = [0; 10];
        stream.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
        response[..10].copy_from_slice(&header);
        stream.read_exact(&mut response[10..len]).unwrap();
        self.exchanges.lock().unwrap().push(Exchange {
            command: command.to_vec(),
            response: response[..len].to_vec(),
        });
        Ok(len)
    }
}

fn random_32() -> [u8; 32] {
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .expect("the host's /dev/urandom must be readable");
    bytes
}

/// Kills the `swtpm` this test started however the test ends.
struct Swtpm(Child);

impl Drop for Swtpm {
    fn drop(&mut self) {
        // The process may already have exited; either way it is reaped.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect(socket: &Path) -> UnixStream {
    // swtpm creates the socket after it has parsed its arguments and
    // loaded the state; a second is far more than that takes.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return stream,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => panic!("swtpm never opened {}: {error}", socket.display()),
        }
    }
}

#[test]
#[ignore = "drives a real swtpm; HELIOS_SWTPM names the binary, and the run rewrites tests/fixtures/swtpm.json"]
fn record_swtpm_fixtures() {
    let swtpm = std::env::var_os("HELIOS_SWTPM")
        .expect("HELIOS_SWTPM must name the swtpm binary to record against");
    let version = Command::new(&swtpm)
        .arg("--version")
        .output()
        .expect("swtpm --version must run");
    let version = String::from_utf8(version.stdout).unwrap().trim().to_owned();

    let state = tempfile::tempdir().unwrap();
    let data = state.path().join("data.sock");
    let control = state.path().join("control.sock");
    let _swtpm = Swtpm(
        Command::new(&swtpm)
            .arg("socket")
            .arg("--tpm2")
            .arg("--tpmstate")
            .arg(format!("dir={}", state.path().display()))
            .arg("--server")
            .arg(format!("type=unixio,path={}", data.display()))
            .arg("--ctrl")
            .arg(format!("type=unixio,path={}", control.display()))
            // Firmware sends TPM2_Startup before the kernel runs; swtpm
            // does it itself here.
            .arg("--flags")
            .arg("not-need-init,startup-clear")
            .spawn()
            .expect("swtpm must start"),
    );
    let recorder = Recorder {
        stream: Arc::new(Mutex::new(connect(&data))),
        exchanges: Arc::default(),
    };

    let inputs = Inputs {
        first_secret: random_32(),
        second_secret: random_32(),
        forged_secret: random_32(),
        third_secret: random_32(),
        extend_digest: random_32(),
    };
    let mut phases = Vec::new();
    complete(seal(recorder.clone(), &inputs));
    phases.push(Phase {
        name: SEAL.to_owned(),
        exchanges: recorder.take(),
    });
    complete(plant_foreign(recorder.clone(), &inputs));
    phases.push(Phase {
        name: PLANT_FOREIGN.to_owned(),
        exchanges: recorder.take(),
    });
    complete(replace_foreign(recorder.clone(), &inputs));
    phases.push(Phase {
        name: REPLACE_FOREIGN.to_owned(),
        exchanges: recorder.take(),
    });

    let recording = Recording {
        swtpm: version,
        inputs,
        phases,
    };
    let mut text = serde_json::to_string_pretty(&recording).unwrap();
    text.push('\n');
    let path = fixture_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn sealing_replays_byte_for_byte() {
    let recording = load_recording();
    let replay = Replay::new(recording.phase(SEAL));
    complete(seal(replay.clone(), &recording.inputs));
    replay.finish();
}

#[test]
fn a_foreign_slot_is_refused_and_replaced() {
    let recording = load_recording();
    let planting = Replay::new(recording.phase(PLANT_FOREIGN));
    complete(plant_foreign(planting.clone(), &recording.inputs));
    planting.finish();

    let replacing = Replay::new(recording.phase(REPLACE_FOREIGN));
    complete(replace_foreign(replacing.clone(), &recording.inputs));
    replacing.finish();
}

#[test]
fn sealing_replays_through_the_fifo_interface() {
    let recording = load_recording();
    let replay = Replay::new(recording.phase(SEAL));
    let tis = Tis::open(TisModel::new(replay.clone(), 2, 16), TestClock::default()).unwrap();
    complete(seal(tis, &recording.inputs));
    replay.finish();
}

#[test]
fn sealing_replays_through_the_command_response_buffer() {
    /// Where QEMU maps the CRB on `q35` (`TPM_CRB_ADDR_BASE`).
    const CRB_BASE: u64 = 0xfed4_0000;
    let recording = load_recording();
    let replay = Replay::new(recording.phase(SEAL));
    let crb = complete(Crb::open(
        CrbModel::new(replay.clone(), 2, CRB_BASE),
        CRB_BASE,
        TestClock::default(),
    ))
    .unwrap();
    complete(seal(crb, &recording.inputs));
    replay.finish();
}

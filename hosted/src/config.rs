use std::env;
use std::path::{Path, PathBuf};
use std::thread;

use helios_hal::cpu::ProcessorId;

const DEFAULT_HEAP_BYTES: usize = 128 * 1024 * 1024;

/// Hosted runtime configuration sourced from the process environment.
///
/// Keeping this in one place makes the hosted backend deterministic and easy to
/// mirror in tests without hardcoding settings throughout the runtime.
#[derive(Clone)]
pub struct HostedConfig {
    processor_count: usize,
    bootstrap_processor: ProcessorId,
    heap_bytes: usize,
    init_args: Vec<String>,
    init_env: Vec<(String, String)>,
    init_wasi_root: Option<PathBuf>,
}

impl HostedConfig {
    pub fn from_env() -> Self {
        let processor_count =
            parse_env_usize("HELIOS_HOSTED_PROCESSORS").unwrap_or_else(default_processor_count);
        assert!(
            processor_count != 0,
            "HELIOS_HOSTED_PROCESSORS must be greater than zero"
        );

        let bootstrap_processor = parse_env_usize("HELIOS_HOSTED_BOOTSTRAP_PROCESSOR").unwrap_or(0);
        assert!(
            bootstrap_processor < processor_count,
            "HELIOS_HOSTED_BOOTSTRAP_PROCESSOR={} is out of range for {} processors",
            bootstrap_processor,
            processor_count
        );

        let heap_bytes = parse_env_usize("HELIOS_HOSTED_HEAP_BYTES").unwrap_or(DEFAULT_HEAP_BYTES);
        assert!(
            heap_bytes != 0,
            "HELIOS_HOSTED_HEAP_BYTES must be greater than zero"
        );

        let init_args = parse_env_shell_words("HELIOS_HOSTED_INIT_ARGS").unwrap_or_default();
        let init_env = parse_env_key_value_words("HELIOS_HOSTED_INIT_ENV").unwrap_or_default();
        let init_wasi_root = env::var_os("HELIOS_HOSTED_INIT_WASI_ROOT").map(PathBuf::from);

        Self {
            processor_count,
            bootstrap_processor: ProcessorId::new(bootstrap_processor as u16),
            heap_bytes,
            init_args,
            init_env,
            init_wasi_root,
        }
    }

    pub fn processor_count(&self) -> usize {
        self.processor_count
    }

    pub fn bootstrap_processor(&self) -> ProcessorId {
        self.bootstrap_processor
    }

    pub fn heap_bytes(&self) -> usize {
        self.heap_bytes
    }

    pub fn init_args(&self) -> &[String] {
        &self.init_args
    }

    pub fn init_env(&self) -> &[(String, String)] {
        &self.init_env
    }

    pub fn init_wasi_root(&self) -> Option<&Path> {
        self.init_wasi_root.as_deref()
    }
}

fn default_processor_count() -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

fn parse_env_usize(name: &str) -> Option<usize> {
    match env::var(name) {
        Ok(value) => Some(
            value
                .parse()
                .unwrap_or_else(|err| panic!("failed to parse {name}={value:?} as usize: {err}")),
        ),
        Err(env::VarError::NotPresent) => None,
        Err(err) => panic!("failed to read environment variable {name}: {err}"),
    }
}

fn parse_env_string(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(err) => panic!("failed to read environment variable {name}: {err}"),
    }
}

fn parse_env_shell_words(name: &str) -> Option<Vec<String>> {
    let value = parse_env_string(name)?;
    Some(
        shell_words::split(&value).unwrap_or_else(|error| {
            panic!("failed to parse {name}={value:?} as shell words: {error}")
        }),
    )
}

fn parse_env_key_value_words(name: &str) -> Option<Vec<(String, String)>> {
    let words = parse_env_shell_words(name)?;
    Some(
        words
            .into_iter()
            .map(|word| {
                let (key, value) = word
                    .split_once('=')
                    .unwrap_or_else(|| panic!("{name} entry {word:?} must have KEY=VALUE form"));
                (key.to_owned(), value.to_owned())
            })
            .collect(),
    )
}

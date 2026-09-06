//! `helios-branch-hints`: the three steps of the branch-hint feedback
//! loop, one subcommand each. `docs/pgo.md` section (b) describes how they
//! fit together and where the profiles come from.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use helios_branch_hints::{
    Error, HINT_RATIO, MIN_OBSERVATIONS, component,
    counts::Counts,
    hint, instrument,
    profile::{Profile, SiteMap},
};

#[derive(Debug, Parser)]
#[command(
    name = "helios-branch-hints",
    about = "Record branch counts from a real run and write wasm branch hints from them"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Rewrite a module so a run records which way each branch went.
    Instrument(InstrumentArgs),
    /// Turn one or more recorded runs into a profile.
    Record(RecordArgs),
    /// Write a profile's hints into the original, uninstrumented module.
    Hint(HintArgs),
}

#[derive(Debug, Args)]
struct InstrumentArgs {
    /// The module or component to instrument.
    #[arg(long)]
    input: PathBuf,
    /// Where to write the instrumented module.
    #[arg(long)]
    output: PathBuf,
    /// Where to write the map from counter index to branch site.
    #[arg(long)]
    sites: PathBuf,
}

#[derive(Debug, Args)]
struct RecordArgs {
    /// The site map the instrumenter wrote.
    #[arg(long)]
    sites: PathBuf,
    /// A captured guest run, one per workload; repeat the flag to sum
    /// several.
    #[arg(long = "counts", required = true)]
    counts: Vec<PathBuf>,
    /// Names of the workloads the runs came from, in the same order.
    #[arg(long = "run")]
    runs: Vec<String>,
    /// The module the profile applies to, as a repository-relative path.
    #[arg(long)]
    module: String,
    /// Where to write the profile.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct HintArgs {
    /// The original, uninstrumented module or component.
    #[arg(long)]
    input: PathBuf,
    /// The profile recorded for it.
    #[arg(long)]
    profile: PathBuf,
    /// Where to write the hinted module.
    #[arg(long)]
    output: PathBuf,
}

fn main() -> Result<(), Error> {
    match Cli::parse().command {
        Command::Instrument(args) => run_instrument(&args),
        Command::Record(args) => run_record(&args),
        Command::Hint(args) => run_hint(&args),
    }
}

fn run_instrument(args: &InstrumentArgs) -> Result<(), Error> {
    let input = read(&args.input)?;
    let source = args.input.display().to_string();
    let instrumented = instrument::instrument(&input, &source)?;
    println!(
        "instrumented {} branch sites in {} of {}; \
         {} pages of its memory reserved for the counters",
        instrumented.sites.sites.len(),
        component::describe(&instrumented.location),
        source,
        instrumented.reserved_pages,
    );
    write(&args.output, &instrumented.wasm)?;
    write_json(&args.sites, &instrumented.sites)
}

fn run_record(args: &RecordArgs) -> Result<(), Error> {
    let map: SiteMap = read_json(&args.sites)?;
    let mut recorded = Vec::with_capacity(args.counts.len());
    for path in &args.counts {
        let output = String::from_utf8_lossy(&read(path)?).into_owned();
        recorded.push(Counts::parse(&output)?);
    }
    let runs = if args.runs.is_empty() {
        args.counts
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    } else {
        args.runs.clone()
    };
    let profile = Profile::record(&map, &args.module, runs, &recorded)?;
    let hinted = profile.hints().count();
    println!(
        "recorded {} sites executed at least {MIN_OBSERVATIONS} times out of {}; \
         {hinted} of them are biased at least {:.0}% one way",
        profile.sites.len(),
        map.sites.len(),
        HINT_RATIO * 100.0,
    );
    write_json(&args.output, &profile)
}

fn run_hint(args: &HintArgs) -> Result<(), Error> {
    let input = read(&args.input)?;
    let profile: Profile = read_json(&args.profile)?;
    let hinted = hint::apply(&input, &profile)?;
    println!(
        "hinted {} branches ({} taken, {} not taken) across {} functions in {} of {}, \
         from {} recorded by {}",
        hinted.stats.hints,
        hinted.stats.taken,
        hinted.stats.not_taken,
        hinted.stats.functions,
        component::describe(&hinted.location),
        args.input.display(),
        args.profile.display(),
        profile.runs.join(", "),
    );
    write(&args.output, &hinted.wasm)
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(Error::io(path))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::write(path, bytes).map_err(Error::io(path))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let bytes = read(path)?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Json {
        path: path.display().to_string(),
        source,
    })
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::Json {
        path: path.display().to_string(),
        source,
    })?;
    bytes.push(b'\n');
    write(path, &bytes)
}

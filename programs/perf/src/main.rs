use std::env;
use std::fmt::{self, Write as _};
use std::io::{self, Write as _};

use helios_api::profiling::{self, Filter, MetricFilter, Scope};

const DEFAULT_LIMIT: u32 = 40;

#[derive(Clone, Copy)]
enum Command {
    Enable,
    Disable,
    Clear,
    Folded,
    Metrics,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
    Folded,
}

struct Options {
    command: Command,
    format: OutputFormat,
    scope: Option<Scope>,
    limit: u32,
    prefixes: Vec<String>,
}

#[derive(Debug)]
enum PerfError {
    MissingCommand,
    UnknownCommand(String),
    UnknownFlag(String),
    MissingValue(&'static str),
    InvalidFormat(String),
    InvalidScope(String),
    InvalidLimit(String),
    FoldedMetrics,
    Io(io::Error),
    Fmt(fmt::Error),
}

impl fmt::Display for PerfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => f.write_str("usage: perf <enable|disable|clear|folded|metrics> [--format text|json|folded] [--scope kernel|user] [--prefix value] [--limit n]"),
            Self::UnknownCommand(command) => write!(f, "unknown perf command: {command}"),
            Self::UnknownFlag(flag) => write!(f, "unknown perf flag: {flag}"),
            Self::MissingValue(flag) => write!(f, "missing value for {flag}"),
            Self::InvalidFormat(format) => write!(f, "invalid perf output format: {format}"),
            Self::InvalidScope(scope) => write!(f, "invalid perf scope: {scope}"),
            Self::InvalidLimit(limit) => write!(f, "invalid perf limit: {limit}"),
            Self::FoldedMetrics => f.write_str("metrics output does not support folded format"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Fmt(error) => write!(f, "{error}"),
        }
    }
}

impl From<io::Error> for PerfError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<fmt::Error> for PerfError {
    fn from(error: fmt::Error) -> Self {
        Self::Fmt(error)
    }
}

#[helios_api::main]
async fn main() -> Result<(), PerfError> {
    let options = Options::parse(env::args().skip(1))?;
    match options.command {
        Command::Enable => {
            profiling::set_enabled(true);
            print_status("enabled")?;
        }
        Command::Disable => {
            profiling::set_enabled(false);
            print_status("disabled")?;
        }
        Command::Clear => {
            profiling::clear();
            print_status("cleared")?;
        }
        Command::Folded => write_folded(&options)?,
        Command::Metrics => write_metrics(&options)?,
    }
    Ok(())
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, PerfError> {
        let mut args = args.into_iter();
        let command = match args.next() {
            Some(command) => parse_command(command)?,
            None => return Err(PerfError::MissingCommand),
        };
        let mut format = OutputFormat::Text;
        let mut scope = None;
        let mut limit = DEFAULT_LIMIT;
        let mut prefixes = Vec::new();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--format" => format = parse_format(next_value(&mut args, "--format")?)?,
                "--scope" => scope = Some(parse_scope(next_value(&mut args, "--scope")?)?),
                "--prefix" => prefixes.push(next_value(&mut args, "--prefix")?),
                "--limit" => limit = parse_limit(next_value(&mut args, "--limit")?)?,
                flag => return Err(PerfError::UnknownFlag(flag.to_owned())),
            }
        }

        Ok(Self {
            command,
            format,
            scope,
            limit,
            prefixes,
        })
    }
}

fn parse_command(command: String) -> Result<Command, PerfError> {
    Ok(match command.as_str() {
        "enable" => Command::Enable,
        "disable" => Command::Disable,
        "clear" => Command::Clear,
        "folded" => Command::Folded,
        "metrics" => Command::Metrics,
        _ => return Err(PerfError::UnknownCommand(command)),
    })
}

fn parse_format(format: String) -> Result<OutputFormat, PerfError> {
    Ok(match format.as_str() {
        "text" => OutputFormat::Text,
        "json" => OutputFormat::Json,
        "folded" => OutputFormat::Folded,
        _ => return Err(PerfError::InvalidFormat(format)),
    })
}

fn parse_scope(scope: String) -> Result<Scope, PerfError> {
    Ok(match scope.as_str() {
        "kernel" => Scope::Kernel,
        "user" => Scope::User,
        _ => return Err(PerfError::InvalidScope(scope)),
    })
}

fn parse_limit(limit: String) -> Result<u32, PerfError> {
    let limit = limit
        .parse::<u32>()
        .map_err(|_| PerfError::InvalidLimit(limit))?;
    Ok(limit)
}

fn next_value(
    args: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<String, PerfError> {
    args.next().ok_or(PerfError::MissingValue(flag))
}

fn write_folded(options: &Options) -> Result<(), PerfError> {
    let samples = profiling::folded(
        &Filter {
            scope: options.scope,
            stack_prefixes: options.prefixes.clone(),
        },
        options.limit,
    );
    let mut output = String::new();
    match options.format {
        OutputFormat::Text => {
            for sample in &samples {
                writeln!(
                    output,
                    "{:<6} {:>14} {}",
                    scope_name(sample.scope),
                    sample.weight,
                    sample.stack
                )?;
            }
        }
        OutputFormat::Json => {
            output.push('[');
            for (index, sample) in samples.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write!(
                    output,
                    "{{\"scope\":\"{}\",\"stack\":\"{}\",\"weight\":{}}}",
                    scope_name(sample.scope),
                    JsonStr(&sample.stack),
                    sample.weight
                )?;
            }
            output.push(']');
            output.push('\n');
        }
        OutputFormat::Folded => {
            for sample in &samples {
                writeln!(output, "{} {}", sample.stack, sample.weight)?;
            }
        }
    }
    write_stdout(output.as_bytes())?;
    Ok(())
}

fn write_metrics(options: &Options) -> Result<(), PerfError> {
    if options.format == OutputFormat::Folded {
        return Err(PerfError::FoldedMetrics);
    }
    let samples = profiling::metrics(
        &MetricFilter {
            name_prefixes: options.prefixes.clone(),
        },
        options.limit,
    );
    let mut output = String::new();
    match options.format {
        OutputFormat::Text => {
            writeln!(
                output,
                "{:<6} {:>8} {:>14} {:>14} {:>14} {:>14} {:>14} {:>14} {:>14} name",
                "scope",
                "count",
                "events",
                "total_ns",
                "min_ns",
                "max_ns",
                "cycles",
                "bytes",
                "B/s"
            )?;
            for sample in &samples {
                writeln!(
                    output,
                    "{:<6} {:>8} {:>14} {:>14} {:>14} {:>14} {:>14} {:>14} {:>14} {}",
                    scope_name(sample.scope),
                    sample.count,
                    sample.total_events,
                    sample.total_nanos,
                    sample.min_nanos,
                    sample.max_nanos,
                    sample.total_cpu_cycles,
                    sample.total_bytes,
                    bytes_per_second(sample.total_bytes, sample.total_nanos),
                    sample.name
                )?;
            }
        }
        OutputFormat::Json => {
            output.push('[');
            for (index, sample) in samples.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write!(
                    output,
                    "{{\"scope\":\"{}\",\"name\":\"{}\",\"count\":{},\"total_events\":{},\"total_nanos\":{},\"min_nanos\":{},\"max_nanos\":{},\"total_bytes\":{},\"bytes_per_second\":{},\"total_reference_cycles\":{},\"total_cpu_cycles\":{},\"total_instructions_retired\":{}}}",
                    scope_name(sample.scope),
                    JsonStr(&sample.name),
                    sample.count,
                    sample.total_events,
                    sample.total_nanos,
                    sample.min_nanos,
                    sample.max_nanos,
                    sample.total_bytes,
                    bytes_per_second(sample.total_bytes, sample.total_nanos),
                    sample.total_reference_cycles,
                    sample.total_cpu_cycles,
                    sample.total_instructions_retired
                )?;
            }
            output.push(']');
            output.push('\n');
        }
        OutputFormat::Folded => return Err(PerfError::FoldedMetrics),
    }
    write_stdout(output.as_bytes())?;
    Ok(())
}

fn print_status(status: &str) -> Result<(), PerfError> {
    let mut output = String::new();
    writeln!(output, "perf {status}")?;
    write_stdout(output.as_bytes())
}

fn write_stdout(bytes: &[u8]) -> Result<(), PerfError> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()?;
    Ok(())
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Kernel => "kernel",
        Scope::User => "user",
    }
}

fn bytes_per_second(total_bytes: u64, total_nanos: u64) -> u64 {
    if total_bytes == 0 || total_nanos == 0 {
        return 0;
    }
    let bytes_per_second = (u128::from(total_bytes) * 1_000_000_000) / u128::from(total_nanos);
    bytes_per_second.min(u128::from(u64::MAX)) as u64
}

struct JsonStr<'a>(&'a str);

impl fmt::Display for JsonStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for ch in self.0.chars() {
            match ch {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                '\t' => f.write_str("\\t")?,
                ch if ch.is_control() => write!(f, "\\u{:04x}", ch as u32)?,
                ch => f.write_char(ch)?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::bytes_per_second;

    #[test]
    fn byte_rate_uses_nanosecond_duration() {
        assert_eq!(bytes_per_second(1_500, 1_000), 1_500_000_000);
        assert_eq!(bytes_per_second(0, 1_000), 0);
        assert_eq!(bytes_per_second(1_500, 0), 0);
    }
}

//! `sysctl`: lists, reads and writes the machine's settings.
//!
//! ```text
//! sysctl -a              every setting: name: value
//! sysctl -a -d           every setting: name: description
//! sysctl -a -t           every setting: name: unit
//! sysctl NAME...         each named setting: name: value   (-d, -t as above)
//! sysctl NAME=VALUE...   write; prints  name: old -> new
//! ```
//!
//! Arguments run in order and the first failure ends the run with status 1.
//! Values are decimal integers in the setting's unit.

use std::env;

use helios_api::settings::{self, Setting, Unit, Value, WriteError};
use thiserror::Error;

/// Which field of a setting a read prints after its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Value,
    Description,
    Unit,
}

/// One named argument, in command-line order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    Read(String),
    Write { name: String, value: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    /// `-a`: every setting.
    All,
    /// Named settings, read or written as each argument says.
    Items(Vec<Item>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Invocation {
    view: View,
    request: Request,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("invalid {unit}: {text}", unit = unit_name(*unit))]
struct ValueError {
    unit: Unit,
    text: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
enum SysctlError {
    #[error("usage: sysctl [-d|-t] -a | sysctl [-d|-t] name... | sysctl name=value...")]
    Usage,
    #[error("sysctl: unknown flag: {0}")]
    UnknownFlag(String),
    #[error("sysctl: -d and -t cannot be combined")]
    ConflictingViews,
    #[error("sysctl: -a takes no setting names")]
    AllWithNames,
    #[error("sysctl: -d and -t apply to reads, not to {name}={value}")]
    ViewWithWrite { name: String, value: String },
    #[error("sysctl: unknown setting: {0}")]
    UnknownSetting(String),
    #[error("sysctl: {name}: {source}")]
    InvalidValue {
        name: String,
        #[source]
        source: ValueError,
    },
    #[error("sysctl: {0}: not changed: this program was not granted the right to write settings")]
    Denied(String),
    #[error("sysctl: {name}: not changed: the setting holds {holds}", holds = unit_noun(*unit))]
    UnitMismatch { name: String, unit: Unit },
}

#[helios_api::main]
async fn main() -> Result<(), SysctlError> {
    let invocation = Invocation::parse(env::args().skip(1))?;
    match invocation.request {
        Request::All => {
            for setting in settings::list() {
                println!("{}", read_line(&setting, invocation.view));
            }
        }
        Request::Items(items) => {
            for item in items {
                match item {
                    Item::Read(name) => {
                        let setting =
                            settings::read(&name).ok_or(SysctlError::UnknownSetting(name))?;
                        println!("{}", read_line(&setting, invocation.view));
                    }
                    Item::Write { name, value } => {
                        println!("{}", write(&name, &value)?);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Reads the setting to learn its unit, parses `text` in that unit, writes
/// it, and returns the line that reports the change.
fn write(name: &str, text: &str) -> Result<String, SysctlError> {
    let current =
        settings::read(name).ok_or_else(|| SysctlError::UnknownSetting(name.to_owned()))?;
    let value = parse_value(value_unit(current.value), text).map_err(|source| {
        SysctlError::InvalidValue {
            name: name.to_owned(),
            source,
        }
    })?;
    let old = settings::write(name, value).map_err(|error| match error {
        WriteError::Denied => SysctlError::Denied(name.to_owned()),
        WriteError::Unknown => SysctlError::UnknownSetting(name.to_owned()),
        WriteError::UnitMismatch(unit) => SysctlError::UnitMismatch {
            name: name.to_owned(),
            unit,
        },
    })?;
    Ok(write_line(name, old, value))
}

impl Invocation {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, SysctlError> {
        let mut all = false;
        let mut view = None;
        let mut items = Vec::new();
        for arg in args {
            match arg.as_str() {
                "-a" => all = true,
                "-d" => set_view(&mut view, View::Description)?,
                "-t" => set_view(&mut view, View::Unit)?,
                flag if flag.starts_with('-') => {
                    return Err(SysctlError::UnknownFlag(flag.to_owned()));
                }
                _ => items.push(match arg.split_once('=') {
                    Some((name, value)) => Item::Write {
                        name: name.to_owned(),
                        value: value.to_owned(),
                    },
                    None => Item::Read(arg),
                }),
            }
        }
        if view.is_some()
            && let Some(Item::Write { name, value }) =
                items.iter().find(|item| matches!(item, Item::Write { .. }))
        {
            return Err(SysctlError::ViewWithWrite {
                name: name.clone(),
                value: value.clone(),
            });
        }
        let request = match (all, items.is_empty()) {
            (true, true) => Request::All,
            (true, false) => return Err(SysctlError::AllWithNames),
            (false, true) => return Err(SysctlError::Usage),
            (false, false) => Request::Items(items),
        };
        Ok(Self {
            view: view.unwrap_or(View::Value),
            request,
        })
    }
}

fn set_view(slot: &mut Option<View>, view: View) -> Result<(), SysctlError> {
    match slot {
        Some(existing) if *existing != view => Err(SysctlError::ConflictingViews),
        _ => {
            *slot = Some(view);
            Ok(())
        }
    }
}

/// Parses `text` as a decimal integer in `unit`.
fn parse_value(unit: Unit, text: &str) -> Result<Value, ValueError> {
    // `u64::from_str` also accepts a leading `+`; the format is digits only.
    let number = if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
        text.parse::<u64>().ok()
    } else {
        None
    };
    match (unit, number) {
        (Unit::Count, Some(number)) => Ok(Value::Count(number)),
        (Unit::Bytes, Some(number)) => Ok(Value::Bytes(number)),
        (unit, None) => Err(ValueError {
            unit,
            text: text.to_owned(),
        }),
    }
}

fn value_unit(value: Value) -> Unit {
    match value {
        Value::Count(_) => Unit::Count,
        Value::Bytes(_) => Unit::Bytes,
    }
}

fn value_number(value: Value) -> u64 {
    match value {
        Value::Count(number) | Value::Bytes(number) => number,
    }
}

/// The name `-t` prints.
fn unit_name(unit: Unit) -> &'static str {
    match unit {
        Unit::Count => "count",
        Unit::Bytes => "bytes",
    }
}

/// What a setting "holds", for the unit-mismatch message.
fn unit_noun(unit: Unit) -> &'static str {
    match unit {
        Unit::Count => "a count",
        Unit::Bytes => "bytes",
    }
}

/// `name: field` for a read, the field chosen by `view`.
fn read_line(setting: &Setting, view: View) -> String {
    match view {
        View::Value => format!("{}: {}", setting.name, value_number(setting.value)),
        View::Description => format!("{}: {}", setting.name, setting.description),
        View::Unit => format!("{}: {}", setting.name, unit_name(value_unit(setting.value))),
    }
}

/// `name: old -> new` for a write.
fn write_line(name: &str, old: Value, new: Value) -> String {
    format!("{name}: {} -> {}", value_number(old), value_number(new))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Result<Invocation, SysctlError> {
        Invocation::parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    fn parsed(unit: Unit, text: &str) -> Result<(Unit, u64), ValueError> {
        parse_value(unit, text).map(|value| (value_unit(value), value_number(value)))
    }

    fn setting(value: Value) -> Setting {
        Setting {
            name: "process.descriptor-limit".to_owned(),
            description: "Descriptor numbers a new process may hold.".to_owned(),
            value,
        }
    }

    #[test]
    fn dash_a_lists_every_setting_by_value() {
        assert_eq!(
            args(&["-a"]),
            Ok(Invocation {
                view: View::Value,
                request: Request::All
            })
        );
    }

    #[test]
    fn view_flags_select_the_field_in_either_order() {
        for flags in [["-a", "-d"], ["-d", "-a"]] {
            assert_eq!(args(&flags).unwrap().view, View::Description);
        }
        for flags in [["-a", "-t"], ["-t", "-a"]] {
            assert_eq!(args(&flags).unwrap().view, View::Unit);
        }
    }

    #[test]
    fn names_and_assignments_keep_command_line_order() {
        assert_eq!(
            args(&["a.b", "c.d=5", "e.f"]).unwrap().request,
            Request::Items(vec![
                Item::Read("a.b".to_owned()),
                Item::Write {
                    name: "c.d".to_owned(),
                    value: "5".to_owned()
                },
                Item::Read("e.f".to_owned()),
            ])
        );
    }

    #[test]
    fn an_assignment_splits_at_the_first_equals_sign() {
        assert_eq!(
            args(&["a.b=c=d"]).unwrap().request,
            Request::Items(vec![Item::Write {
                name: "a.b".to_owned(),
                value: "c=d".to_owned()
            }])
        );
    }

    #[test]
    fn named_reads_take_view_flags() {
        let invocation = args(&["-t", "a.b"]).unwrap();
        assert_eq!(invocation.view, View::Unit);
        assert_eq!(
            invocation.request,
            Request::Items(vec![Item::Read("a.b".to_owned())])
        );
    }

    #[test]
    fn no_arguments_and_view_flags_alone_are_usage_errors() {
        assert_eq!(args(&[]), Err(SysctlError::Usage));
        assert_eq!(args(&["-d"]), Err(SysctlError::Usage));
    }

    #[test]
    fn an_unknown_flag_is_named() {
        assert_eq!(
            args(&["-x"]),
            Err(SysctlError::UnknownFlag("-x".to_owned()))
        );
        assert_eq!(
            args(&["-a", "--all"]),
            Err(SysctlError::UnknownFlag("--all".to_owned()))
        );
    }

    #[test]
    fn dash_a_refuses_names_and_view_flags_refuse_each_other_and_writes() {
        assert_eq!(args(&["-a", "a.b"]), Err(SysctlError::AllWithNames));
        assert_eq!(
            args(&["-d", "-t", "a.b"]),
            Err(SysctlError::ConflictingViews)
        );
        assert_eq!(
            args(&["-t", "a.b=1"]),
            Err(SysctlError::ViewWithWrite {
                name: "a.b".to_owned(),
                value: "1".to_owned()
            })
        );
    }

    #[test]
    fn a_count_parses_as_decimal_in_its_unit() {
        assert_eq!(parsed(Unit::Count, "64"), Ok((Unit::Count, 64)));
        assert_eq!(parsed(Unit::Count, "0"), Ok((Unit::Count, 0)));
        assert_eq!(
            parsed(Unit::Count, "18446744073709551615"),
            Ok((Unit::Count, u64::MAX))
        );
    }

    #[test]
    fn bytes_parse_into_the_bytes_variant() {
        assert_eq!(parsed(Unit::Bytes, "4096"), Ok((Unit::Bytes, 4096)));
    }

    #[test]
    fn a_value_that_is_not_a_decimal_integer_is_refused() {
        for text in [
            "",
            "abc",
            "-1",
            "+1",
            "1.5",
            "0x10",
            " 1",
            "1k",
            "18446744073709551616",
        ] {
            assert_eq!(
                parsed(Unit::Count, text),
                Err(ValueError {
                    unit: Unit::Count,
                    text: text.to_owned()
                }),
                "{text:?}"
            );
        }
    }

    #[test]
    fn the_value_error_names_the_unit_and_the_text() {
        let error = SysctlError::InvalidValue {
            name: "process.descriptor-limit".to_owned(),
            source: parse_value(Unit::Count, "abc").unwrap_err(),
        };
        assert_eq!(
            error.to_string(),
            "sysctl: process.descriptor-limit: invalid count: abc"
        );
        assert_eq!(
            parse_value(Unit::Bytes, "x").unwrap_err().to_string(),
            "invalid bytes: x"
        );
    }

    #[test]
    fn a_read_line_prints_the_field_the_view_selects() {
        let setting = setting(Value::Count(1024));
        assert_eq!(
            read_line(&setting, View::Value),
            "process.descriptor-limit: 1024"
        );
        assert_eq!(
            read_line(&setting, View::Description),
            "process.descriptor-limit: Descriptor numbers a new process may hold."
        );
        assert_eq!(
            read_line(&setting, View::Unit),
            "process.descriptor-limit: count"
        );
        assert_eq!(
            read_line(&self::setting(Value::Bytes(7)), View::Unit),
            "process.descriptor-limit: bytes"
        );
    }

    #[test]
    fn a_write_line_shows_the_old_and_new_values() {
        assert_eq!(
            write_line(
                "process.descriptor-limit",
                Value::Count(1024),
                Value::Count(64)
            ),
            "process.descriptor-limit: 1024 -> 64"
        );
    }

    #[test]
    fn the_failure_messages_are_the_documented_ones() {
        assert_eq!(
            SysctlError::Usage.to_string(),
            "usage: sysctl [-d|-t] -a | sysctl [-d|-t] name... | sysctl name=value..."
        );
        assert_eq!(
            SysctlError::UnknownFlag("-x".to_owned()).to_string(),
            "sysctl: unknown flag: -x"
        );
        assert_eq!(
            SysctlError::UnknownSetting("foo.bar".to_owned()).to_string(),
            "sysctl: unknown setting: foo.bar"
        );
        assert_eq!(
            SysctlError::Denied("process.descriptor-limit".to_owned()).to_string(),
            "sysctl: process.descriptor-limit: not changed: this program was not granted the \
             right to write settings"
        );
        assert_eq!(
            SysctlError::UnitMismatch {
                name: "process.descriptor-limit".to_owned(),
                unit: Unit::Count
            }
            .to_string(),
            "sysctl: process.descriptor-limit: not changed: the setting holds a count"
        );
    }
}

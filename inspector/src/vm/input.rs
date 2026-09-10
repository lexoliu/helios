//! The input script the `input` session action drives the desktop with.
//!
//! A script is one statement per line, each a verb and its arguments:
//!
//! ```text
//! abs 16384 16384   # move the tablet to the middle of the screen
//! btn left down
//! btn left up
//! key ret           # press and release one key
//! rel 40 -20        # nudge the relative pointer
//! ```
//!
//! Every token becomes a typed value before anything is sent: the verb
//! and the enumerated arguments through serde, the numbers through their
//! own parsers, the key names and coordinates through the constructors
//! that hold QEMU's invariants. Nothing downstream sees a string, so a
//! script that names a button QEMU does not have fails while it is being
//! read rather than half-way through a session.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde::de::IntoDeserializer as _;
use serde::de::value::{Error as TokenError, StrDeserializer};

use super::qmp::{
    AbsCoordinate, InputEvent, InputValueError, KeyValue, PointerAxis, PointerButton, QKeyCode,
};

/// Why an input script did not produce a list of events.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InputScriptError {
    #[error("failed to read the input script {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the input script {path} names no events")]
    Empty { path: String },
    #[error("{path}:{line}: {source}")]
    Statement {
        path: String,
        line: usize,
        #[source]
        source: StatementError,
    },
}

/// Why one line of a script is not a statement.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StatementError {
    #[error("{token:?} is not one of key, abs, rel or btn")]
    UnknownVerb {
        token: String,
        #[source]
        source: TokenError,
    },
    #[error("{verb} takes {expected} argument(s), and this line has {actual}")]
    Arity {
        verb: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("{token:?} is not a button QEMU has")]
    UnknownButton {
        token: String,
        #[source]
        source: TokenError,
    },
    #[error("{token:?} is not `down` or `up`")]
    UnknownButtonState {
        token: String,
        #[source]
        source: TokenError,
    },
    #[error("{token:?} is not a whole number")]
    NotANumber {
        token: String,
        #[source]
        source: core::num::ParseIntError,
    },
    #[error("{0}")]
    Value(#[from] InputValueError),
}

/// The verb a statement opens with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Verb {
    Key,
    Abs,
    Rel,
    Btn,
}

impl Verb {
    /// How many arguments this verb takes, and the name its arity error
    /// reports.
    const fn arity(self) -> (&'static str, usize) {
        match self {
            Self::Key => ("key", 1),
            Self::Abs => ("abs", 2),
            Self::Rel => ("rel", 2),
            Self::Btn => ("btn", 2),
        }
    }
}

/// Whether a button statement presses or releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ButtonState {
    Down,
    Up,
}

impl ButtonState {
    const fn down(self) -> bool {
        matches!(self, Self::Down)
    }
}

/// One statement of a script, with every argument already typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputStatement {
    /// One keystroke: the key goes down and comes back up.
    Key(QKeyCode),
    /// The absolute pointer moves to a position on both axes.
    Abs { x: AbsCoordinate, y: AbsCoordinate },
    /// The relative pointer moves by an offset on both axes.
    Rel { dx: i32, dy: i32 },
    /// A pointer button changes state.
    Btn {
        button: PointerButton,
        state: ButtonState,
    },
}

impl InputStatement {
    /// The event batches this statement sends.
    ///
    /// A batch is what QEMU delivers to the guest between two syncs, so
    /// the two axes of a move share one batch and land as a single
    /// position, while a keystroke's press and release are two: a guest
    /// that saw them in one batch would have no interval to observe the
    /// key held down in.
    pub(crate) fn batches(&self) -> Vec<Vec<InputEvent>> {
        match self {
            Self::Key(qcode) => [true, false]
                .into_iter()
                .map(|down| {
                    vec![InputEvent::Key {
                        down,
                        key: KeyValue::QCode(qcode.clone()),
                    }]
                })
                .collect(),
            Self::Abs { x, y } => vec![vec![
                InputEvent::Abs {
                    axis: PointerAxis::X,
                    value: *x,
                },
                InputEvent::Abs {
                    axis: PointerAxis::Y,
                    value: *y,
                },
            ]],
            Self::Rel { dx, dy } => vec![vec![
                InputEvent::Rel {
                    axis: PointerAxis::X,
                    value: *dx,
                },
                InputEvent::Rel {
                    axis: PointerAxis::Y,
                    value: *dy,
                },
            ]],
            Self::Btn { button, state } => vec![vec![InputEvent::Btn {
                down: state.down(),
                button: *button,
            }]],
        }
    }
}

/// A whole script, in the order its statements run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputScript {
    statements: Vec<InputStatement>,
}

impl InputScript {
    /// Reads and parses the script at `path`.
    pub(crate) fn read(path: &Path) -> Result<Self, InputScriptError> {
        let text = fs::read_to_string(path).map_err(|source| InputScriptError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text, &path.display().to_string())
    }

    /// Parses `text`, naming it `origin` in any failure.
    fn parse(text: &str, origin: &str) -> Result<Self, InputScriptError> {
        let mut statements = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or(line).trim();
            if line.is_empty() {
                continue;
            }
            let statement =
                parse_statement(line).map_err(|source| InputScriptError::Statement {
                    path: origin.to_owned(),
                    line: index + 1,
                    source,
                })?;
            statements.push(statement);
        }
        if statements.is_empty() {
            return Err(InputScriptError::Empty {
                path: origin.to_owned(),
            });
        }
        Ok(Self { statements })
    }

    pub(crate) fn statements(&self) -> &[InputStatement] {
        &self.statements
    }
}

/// Deserialises one token into the enumerated value it names.
fn enumerated<'a, T: Deserialize<'a>>(token: &'a str) -> Result<T, TokenError> {
    let deserializer: StrDeserializer<'a, TokenError> = token.into_deserializer();
    T::deserialize(deserializer)
}

fn parse_number<T: core::str::FromStr<Err = core::num::ParseIntError>>(
    token: &str,
) -> Result<T, StatementError> {
    token.parse().map_err(|source| StatementError::NotANumber {
        token: token.to_owned(),
        source,
    })
}

fn parse_statement(line: &str) -> Result<InputStatement, StatementError> {
    let mut tokens = line.split_whitespace();
    let verb_token = tokens
        .next()
        .unwrap_or_else(|| unreachable!("a blank line never reaches a statement"));
    let verb: Verb = enumerated(verb_token).map_err(|source| StatementError::UnknownVerb {
        token: verb_token.to_owned(),
        source,
    })?;
    let arguments: Vec<&str> = tokens.collect();
    let (name, expected) = verb.arity();
    if arguments.len() != expected {
        return Err(StatementError::Arity {
            verb: name,
            expected,
            actual: arguments.len(),
        });
    }
    match verb {
        Verb::Key => Ok(InputStatement::Key(QKeyCode::new(arguments[0])?)),
        Verb::Abs => Ok(InputStatement::Abs {
            x: AbsCoordinate::new(parse_number(arguments[0])?)?,
            y: AbsCoordinate::new(parse_number(arguments[1])?)?,
        }),
        Verb::Rel => Ok(InputStatement::Rel {
            dx: parse_number(arguments[0])?,
            dy: parse_number(arguments[1])?,
        }),
        Verb::Btn => Ok(InputStatement::Btn {
            button: enumerated(arguments[0]).map_err(|source| StatementError::UnknownButton {
                token: arguments[0].to_owned(),
                source,
            })?,
            state: enumerated(arguments[1]).map_err(|source| {
                StatementError::UnknownButtonState {
                    token: arguments[1].to_owned(),
                    source,
                }
            })?,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{ButtonState, InputScript, InputStatement};
    use crate::vm::qmp::{
        AbsCoordinate, InputEvent, KeyValue, PointerAxis, PointerButton, QKeyCode,
    };

    fn script(text: &str) -> InputScript {
        InputScript::parse(text, "<test>").expect("the script parses")
    }

    #[test]
    fn every_verb_becomes_a_typed_statement() {
        let parsed = script(
            "# a click in the middle of the screen\n\
             abs 16384 16384\n\
             btn left down\n\
             btn left up\n\
             key ret\n\
             \n\
             rel 40 -20   # and a nudge\n",
        );
        assert_eq!(
            parsed.statements(),
            [
                InputStatement::Abs {
                    x: AbsCoordinate::new(16_384).expect("inside the axis range"),
                    y: AbsCoordinate::new(16_384).expect("inside the axis range"),
                },
                InputStatement::Btn {
                    button: PointerButton::Left,
                    state: ButtonState::Down,
                },
                InputStatement::Btn {
                    button: PointerButton::Left,
                    state: ButtonState::Up,
                },
                InputStatement::Key(QKeyCode::new("ret").expect("a QEMU key name")),
                InputStatement::Rel { dx: 40, dy: -20 },
            ]
        );
    }

    #[test]
    fn a_keystroke_presses_and_releases_in_two_batches() {
        let statement = InputStatement::Key(QKeyCode::new("a").expect("a QEMU key name"));
        let batches = statement.batches();
        assert_eq!(
            batches,
            [
                vec![InputEvent::Key {
                    down: true,
                    key: KeyValue::QCode(QKeyCode::new("a").expect("a QEMU key name")),
                }],
                vec![InputEvent::Key {
                    down: false,
                    key: KeyValue::QCode(QKeyCode::new("a").expect("a QEMU key name")),
                }],
            ]
        );
    }

    #[test]
    fn a_pointer_move_puts_both_axes_in_one_batch() {
        let statement = InputStatement::Rel { dx: 3, dy: -4 };
        assert_eq!(
            statement.batches(),
            [vec![
                InputEvent::Rel {
                    axis: PointerAxis::X,
                    value: 3,
                },
                InputEvent::Rel {
                    axis: PointerAxis::Y,
                    value: -4,
                },
            ]]
        );
    }

    #[test]
    fn a_script_that_names_no_events_is_refused() {
        InputScript::parse("# nothing but a comment\n\n", "<test>")
            .expect_err("a script has to name at least one event");
    }

    #[test]
    fn a_statement_qemu_could_not_run_is_refused_while_it_is_read() {
        for line in [
            "wiggle 1 2",          // not a verb
            "key",                 // no key named
            "key ret spc",         // two keys on one line
            "key Return",          // not the shape of a QEMU key name
            "abs 40000 10",        // outside QEMU's axis range
            "abs left 10",         // not a number
            "btn middle sideways", // not a button state
            "btn scroll down",     // not a button
        ] {
            InputScript::parse(line, "<test>").unwrap_err();
        }
    }
}

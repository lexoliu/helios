use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellOption {
    Nounset,
    AllExport,
    NoClobber,
    NoGlob,
}

impl ShellOption {
    pub(crate) fn short_flag(self) -> char {
        match self {
            Self::Nounset => 'u',
            Self::AllExport => 'a',
            Self::NoClobber => 'C',
            Self::NoGlob => 'f',
        }
    }

    pub(crate) fn long_name(self) -> &'static str {
        match self {
            Self::Nounset => "nounset",
            Self::AllExport => "allexport",
            Self::NoClobber => "noclobber",
            Self::NoGlob => "noglob",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShellOptions {
    nounset: bool,
    allexport: bool,
    noclobber: bool,
    noglob: bool,
}

impl ShellOptions {
    pub(crate) fn is_enabled(self, option: ShellOption) -> bool {
        match option {
            ShellOption::Nounset => self.nounset,
            ShellOption::AllExport => self.allexport,
            ShellOption::NoClobber => self.noclobber,
            ShellOption::NoGlob => self.noglob,
        }
    }

    pub(crate) fn set(&mut self, option: ShellOption, enabled: bool) {
        match option {
            ShellOption::Nounset => self.nounset = enabled,
            ShellOption::AllExport => self.allexport = enabled,
            ShellOption::NoClobber => self.noclobber = enabled,
            ShellOption::NoGlob => self.noglob = enabled,
        }
    }

    pub(crate) fn nounset(self) -> bool {
        self.nounset
    }

    pub(crate) fn allexport(self) -> bool {
        self.allexport
    }

    pub(crate) fn noclobber(self) -> bool {
        self.noclobber
    }

    pub(crate) fn noglob(self) -> bool {
        self.noglob
    }

    pub(crate) fn current_option_flags(self) -> String {
        option_flag_order()
            .iter()
            .filter(|option| self.is_enabled(**option))
            .map(|option| option.short_flag())
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OptionMode {
    Enable,
    Disable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OptionUpdate {
    pub(crate) option: ShellOption,
    pub(crate) mode: OptionMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetAction {
    Update {
        updates: Vec<OptionUpdate>,
        positional_parameters: Option<Vec<String>>,
    },
    PrintNamedOptions,
    PrintReusableCommands,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum SetOptionError {
    #[error("Illegal option -{0}")]
    IllegalShort(char),
    #[error("Illegal option -o {0}")]
    IllegalLong(String),
}

pub(crate) fn parse_set_arguments(
    args: &[String],
) -> std::result::Result<SetAction, SetOptionError> {
    let mut updates = Vec::new();
    let mut index = 0;

    while index < args.len() {
        let token = &args[index];
        if token == "--" {
            return Ok(SetAction::Update {
                updates,
                positional_parameters: Some(args[index + 1..].to_vec()),
            });
        }

        let Some((mode, flags)) = parse_option_token(token) else {
            return Ok(SetAction::Update {
                updates,
                positional_parameters: Some(args[index..].to_vec()),
            });
        };

        if flags == "o" {
            let Some(name) = args.get(index + 1) else {
                return Ok(match mode {
                    OptionMode::Enable => SetAction::PrintNamedOptions,
                    OptionMode::Disable => SetAction::PrintReusableCommands,
                });
            };
            let option = parse_long_option(name)?;
            updates.push(OptionUpdate { option, mode });
            index += 2;
            continue;
        }

        for flag in flags.chars() {
            let option = parse_short_option(flag)?;
            updates.push(OptionUpdate { option, mode });
        }
        index += 1;
    }

    Ok(SetAction::Update {
        updates,
        positional_parameters: None,
    })
}

pub(crate) fn format_named_option_settings(options: ShellOptions) -> String {
    let mut output = String::from("Current option settings\n");
    for option in named_option_order() {
        let state = if options.is_enabled(option) {
            "on"
        } else {
            "off"
        };
        output.push_str(&format!("{:<15} {state}\n", option.long_name()));
    }
    output
}

pub(crate) fn format_reusable_option_commands(options: ShellOptions) -> String {
    let mut output = String::new();
    for option in named_option_order() {
        let mode = if options.is_enabled(option) { '-' } else { '+' };
        output.push_str(&format!("set {mode}o {}\n", option.long_name()));
    }
    output
}

fn parse_option_token(token: &str) -> Option<(OptionMode, &str)> {
    let (mode, flags) = if let Some(flags) = token.strip_prefix('-') {
        (OptionMode::Enable, flags)
    } else if let Some(flags) = token.strip_prefix('+') {
        (OptionMode::Disable, flags)
    } else {
        return None;
    };

    if flags.is_empty() {
        None
    } else {
        Some((mode, flags))
    }
}

fn parse_short_option(flag: char) -> std::result::Result<ShellOption, SetOptionError> {
    match flag {
        'u' => Ok(ShellOption::Nounset),
        'a' => Ok(ShellOption::AllExport),
        'C' => Ok(ShellOption::NoClobber),
        'f' => Ok(ShellOption::NoGlob),
        other => Err(SetOptionError::IllegalShort(other)),
    }
}

fn parse_long_option(name: &str) -> std::result::Result<ShellOption, SetOptionError> {
    match name {
        "nounset" => Ok(ShellOption::Nounset),
        "allexport" => Ok(ShellOption::AllExport),
        "noclobber" => Ok(ShellOption::NoClobber),
        "noglob" => Ok(ShellOption::NoGlob),
        other => Err(SetOptionError::IllegalLong(other.to_owned())),
    }
}

fn option_flag_order() -> [ShellOption; 4] {
    [
        ShellOption::Nounset,
        ShellOption::AllExport,
        ShellOption::NoClobber,
        ShellOption::NoGlob,
    ]
}

fn named_option_order() -> [ShellOption; 4] {
    [
        ShellOption::NoGlob,
        ShellOption::NoClobber,
        ShellOption::AllExport,
        ShellOption::Nounset,
    ]
}

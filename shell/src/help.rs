use std::fmt::Write as _;

use clap::{ColorChoice, Command, CommandFactory};

use crate::repl::{HelpTopic, ReplCli};

pub(crate) fn render(topic: HelpTopic) -> String {
    let mut root = ReplCli::command().color(ColorChoice::Always);
    let mut output = match topic {
        HelpTopic::Overview => render_command_help(&mut root),
        HelpTopic::Stats => render_named_help(&mut root, "stats"),
        HelpTopic::Tracing => render_named_help(&mut root, "tracing"),
        HelpTopic::Rpc => render_named_help(&mut root, "rpc"),
    };

    let appendix = match topic {
        HelpTopic::Overview => render_sections(&[
            Section::new("Examples", OVERVIEW_EXAMPLES),
            Section::new("Shell Language", OVERVIEW_LANGUAGE),
        ]),
        HelpTopic::Stats => render_sections(&[
            Section::new("Live View", STATS_VIEW),
            Section::new("Examples", STATS_EXAMPLES),
        ]),
        HelpTopic::Tracing => render_sections(&[Section::new("Examples", TRACING_EXAMPLES)]),
        HelpTopic::Rpc => render_sections(&[
            Section::new("Examples", RPC_EXAMPLES),
            Section::new("Notes", RPC_NOTES),
        ]),
    };

    if !appendix.is_empty() {
        output.push('\n');
        output.push_str(&appendix);
    }

    if !output.ends_with('\n') {
        output.push('\n');
    }

    output
}

fn render_named_help(root: &mut Command, name: &str) -> String {
    render_command_help(
        root.find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("missing shell help command `{name}`")),
    )
}

fn render_command_help(command: &mut Command) -> String {
    command.render_long_help().ansi().to_string()
}

fn render_sections(sections: &[Section]) -> String {
    let mut output = String::new();
    for (index, section) in sections.iter().enumerate() {
        if section.lines.is_empty() {
            continue;
        }
        if index != 0 {
            output.push('\n');
        }
        writeln!(&mut output, "{}", section.title).expect("writing help into String must succeed");
        for line in section.lines {
            writeln!(&mut output, "  {line}")
                .expect("writing help into String must succeed");
        }
    }
    output
}

struct Section {
    title: &'static str,
    lines: &'static [&'static str],
}

impl Section {
    const fn new(title: &'static str, lines: &'static [&'static str]) -> Self {
        Self { title, lines }
    }
}

const OVERVIEW_EXAMPLES: &[&str] = &[
    "pwd",
    "ls /boot",
    "echo hello > /tmp/note.txt",
    "cat /tmp/note.txt",
    "run ./target/wasm32-wasip2/debug/helios_ping.wasm google.com",
    "if test -e /tmp/note.txt",
    "    cat /tmp/note.txt",
    "  else",
    "    echo missing",
    "  end",
    "stats",
    "tracing",
    "rpc instance helios:system/stats@0.1.0",
    "rpc func snapshot",
    "rpc call",
];

const OVERVIEW_LANGUAGE: &[&str] = &[
    "Fish-style blocks use `if ... end` with optional `else` and `else if`.",
    "`source <path>` runs a host-local shell script against the current debugger session.",
    "`ls **.jpg` and similar recursive globs are expanded against the remote debugger filesystem.",
    "Right Arrow accepts autosuggestions, and `Ctrl+F` mirrors fish-style hint acceptance.",
    "Full-screen commands keep the shell line-oriented; `stats` and `edit` temporarily take over the terminal.",
];

const STATS_VIEW: &[&str] = &[
    "The live view reports uptime, per-core utilization, memory pressure, and tracked wasm instances.",
    "Press `q`, `Esc`, or `Ctrl+C` to leave the live view.",
];

const STATS_EXAMPLES: &[&str] = &["stats"];

const TRACING_EXAMPLES: &[&str] = &[
    "tracing",
    "tracing limit 128",
    "tracing level debug",
    "tracing targets helios_kernel,helios_riscv",
];

const RPC_EXAMPLES: &[&str] = &[
    "rpc instance helios:system/stats@0.1.0",
    "rpc func snapshot",
    "rpc payload",
    "rpc call",
    "rpc instance helios:system/tracing@0.1.0",
    "rpc func recent",
    "rpc payload <postcard-encoded-filter-and-limit>",
    "rpc call",
];

const RPC_NOTES: &[&str] = &[
    "`rpc call` is intentionally low-level.",
    "Prefer the dedicated `stats` and `tracing` commands when you want decoded output.",
];

#[cfg(test)]
mod tests {
    use super::render;
    use crate::repl::HelpTopic;

    #[test]
    fn overview_help_is_generated_from_command_tree() {
        let output = render(HelpTopic::Overview);
        assert!(output.contains("Usage:"));
        assert!(output.contains("Commands:"));
        assert!(output.contains("stats"));
        assert!(output.contains("Shell Language"));
    }

    #[test]
    fn tracing_help_keeps_generated_command_text() {
        let output = render(HelpTopic::Tracing);
        assert!(output.contains("Stream tracing events until interrupted"));
        assert!(output.contains("tracing level debug"));
    }
}

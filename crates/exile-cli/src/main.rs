//! `exile` — CLI frontend for the exile harness.
//!
//! Frontend #1: a thin shell over `exile-core` that reads lines from stdin
//! and renders the harness event stream to stdout. With a config
//! (`exile.toml`, see `exile.example.toml`) chat is driven by the
//! configured model; without one the REPL runs in tool-only mode.
//!
//! All stdout writes are fallible: when stdout closes (e.g. a pager quits),
//! the REPL exits cleanly instead of panicking on a broken pipe.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::ExitCode;

use exile_core::{Event, Session};
use exile_llm::{Config, OpenAiClient};
use exile_tool_api::{Tool, ToolError, ToolRegistry};

/// The agent definition, maintained as Markdown in `prompts/` (project
/// convention: prompts are content, embedded at build time — never inline
/// strings). Contains no game facts (project law #1).
const SYSTEM_PROMPT: &str = include_str!("../../../prompts/exile.md");

/// Demo tool so the tool path can be exercised without network access.
/// Returns its arguments unchanged.
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "Returns its JSON arguments unchanged. Demo tool."
    }

    fn parameters_schema(&self) -> &'static str {
        r#"{"type":"object"}"#
    }

    fn execute(&self, args_json: &str) -> Result<String, ToolError> {
        Ok(args_json.to_owned())
    }
}

/// One parsed REPL input line.
#[derive(Debug, PartialEq, Eq)]
enum Command<'a> {
    Empty,
    Quit,
    Help,
    Tools,
    Call { name: &'a str, args_json: &'a str },
    CallUsage,
    Unknown(&'a str),
    Chat(&'a str),
}

/// Parse one input line into a [`Command`].
///
/// Strips a UTF-8 BOM first (piped input on Windows often carries one on the
/// first line) so commands still match. `/call` is anchored: it must be
/// followed by whitespace or end of line, so a typo like `/callecho` reports
/// an unknown command instead of silently invoking a tool.
fn parse_command(raw: &str) -> Command<'_> {
    let line = raw.trim_start_matches('\u{feff}').trim();
    if line.is_empty() {
        return Command::Empty;
    }
    match line {
        "/quit" | "/exit" => return Command::Quit,
        "/help" => return Command::Help,
        "/tools" => return Command::Tools,
        _ => {}
    }
    if let Some(rest) = line.strip_prefix("/call") {
        if rest.is_empty() {
            return Command::CallUsage;
        }
        if rest.starts_with(char::is_whitespace) {
            let rest = rest.trim_start();
            if rest.is_empty() {
                return Command::CallUsage;
            }
            return match rest.split_once(char::is_whitespace) {
                Some((name, args)) => Command::Call {
                    name,
                    args_json: args.trim_start(),
                },
                None => Command::Call {
                    name: rest,
                    args_json: "{}",
                },
            };
        }
    }
    if line.starts_with('/') {
        return Command::Unknown(line);
    }
    Command::Chat(line)
}

/// Command-line options for the `exile` binary.
struct Options {
    config_path: String,
    /// Whether `--config` was passed explicitly: a missing explicit path
    /// is an error, while a missing default path means tool-only mode.
    config_explicit: bool,
    profile: Option<String>,
}

fn parse_options(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut config_path = "exile.toml".to_owned();
    let mut config_explicit = false;
    let mut profile = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = args.next().ok_or("--config requires a path")?;
                config_explicit = true;
            }
            "--profile" => {
                profile = Some(args.next().ok_or("--profile requires a name")?);
            }
            other => {
                return Err(format!(
                    "unknown argument `{other}` (usage: exile [--config <path>] [--profile <name>])"
                ));
            }
        }
    }
    Ok(Options {
        config_path,
        config_explicit,
        profile,
    })
}

fn build_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(EchoTool))
        .expect("echo tool registers into an empty registry");
    registry
        .register(Box::new(exile_league::LeagueTool::new()))
        .expect("league tool name is unique");
    registry
        .register(Box::new(exile_wiki::WikiTool::new()))
        .expect("wiki tool name is unique");
    registry
        .register(Box::new(exile_ninja::PriceTool::new()))
        .expect("price tool name is unique");
    registry
        .register(Box::new(exile_pob::PobTool::new()))
        .expect("pob tool name is unique");
    registry
}

/// Build the session (with or without a model) and its banner line.
fn build_session(options: &Options) -> Result<(Session, String), String> {
    let registry = build_registry();
    if Path::new(&options.config_path).exists() {
        let config = Config::load(Path::new(&options.config_path))?;
        let (profile_name, profile) = config.profile(options.profile.as_deref())?;
        let client = OpenAiClient::for_profile(profile)?;
        let line = format!("model: {} via {profile_name}", client.model());
        let mut session = Session::with_model(registry, Box::new(client), SYSTEM_PROMPT.to_owned());
        if let Some(rounds) = config.limits.max_tool_rounds {
            session.set_max_tool_rounds(rounds);
        }
        Ok((session, line))
    } else {
        if options.config_explicit {
            return Err(format!("--config {} does not exist", options.config_path));
        }
        if let Some(profile) = &options.profile {
            return Err(format!(
                "--profile {profile} given but no config found at {}",
                options.config_path
            ));
        }
        Ok((
            Session::new(registry),
            format!(
                "no model configured (copy exile.example.toml to {})",
                options.config_path
            ),
        ))
    }
}

fn main() -> ExitCode {
    let options = match parse_options(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let (mut session, model_line) = match build_session(&options) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("config error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let version = env!("CARGO_PKG_VERSION");
    if writeln_out(&format!(
        "exile {version} — {model_line} — /help for commands, /quit to exit"
    ))
    .is_err()
    {
        return ExitCode::SUCCESS;
    }

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    loop {
        if write_prompt().is_err() {
            break;
        }

        let line = match lines.next() {
            None => break,
            Some(Ok(line)) => line,
            Some(Err(err)) if err.kind() == io::ErrorKind::InvalidData => {
                // Bad bytes (e.g. CP1252 input) invalidate one line, not the
                // stream: report, skip, keep serving the rest of the script.
                eprintln!("input error: {err} — line skipped");
                continue;
            }
            Some(Err(err)) => {
                eprintln!("input error: {err}");
                return ExitCode::FAILURE;
            }
        };

        let output: io::Result<()> = match parse_command(&line) {
            Command::Empty => Ok(()),
            Command::Quit => break,
            Command::Help => print_help(),
            Command::Tools => print_tools(&session),
            Command::CallUsage => writeln_out("usage: /call <name> <json>"),
            Command::Unknown(command) => {
                writeln_out(&format!("unknown command: {command} (try /help)"))
            }
            Command::Call { name, args_json } => {
                run_streaming(|sink| session.call_tool(name, args_json, sink))
            }
            Command::Chat(text) => run_streaming(|sink| session.submit(text, sink)),
        };
        if output.is_err() {
            // Stdout is gone (closed pipe/pager); nothing left to render.
            break;
        }
    }
    ExitCode::SUCCESS
}

/// Drive a session call, rendering events as they arrive. Returns the first
/// write error, letting the caller quit cleanly when stdout disappears.
fn run_streaming(call: impl FnOnce(&mut dyn FnMut(&Event))) -> io::Result<()> {
    let mut outcome: io::Result<()> = Ok(());
    let mut sink = |event: &Event| {
        if outcome.is_ok()
            && let Err(err) = write_event(event)
        {
            outcome = Err(err);
        }
    };
    call(&mut sink);
    outcome
}

fn write_prompt() -> io::Result<()> {
    let mut out = io::stdout();
    write!(out, "exile> ")?;
    out.flush()
}

fn writeln_out(text: &str) -> io::Result<()> {
    writeln!(io::stdout(), "{text}")
}

fn print_help() -> io::Result<()> {
    let mut out = io::stdout();
    writeln!(out, "  /tools              list available tools")?;
    writeln!(
        out,
        "  /call <name> <json> run a tool directly (json defaults to {{}})"
    )?;
    writeln!(out, "  /quit (or /exit)    exit")?;
    writeln!(out, "  anything else       talk to the model")
}

fn print_tools(session: &Session) -> io::Result<()> {
    let mut out = io::stdout();
    if session.registry().is_empty() {
        return writeln!(out, "no tools registered");
    }
    for tool in session.registry().iter() {
        let name = tool.name();
        let description = tool.description();
        writeln!(out, "  {name}  {description}")?;
    }
    Ok(())
}

/// Render one harness event to stdout.
fn write_event(event: &Event) -> io::Result<()> {
    let mut out = io::stdout();
    match event {
        Event::TokenDelta(chunk) => {
            write!(out, "{chunk}")?;
            out.flush()
        }
        Event::ToolCallStarted { name, args_json } => {
            writeln!(out, "[{name}] started with {args_json}")
        }
        Event::ToolCallFinished { name, result_json } => {
            writeln!(out, "[{name}] -> {result_json}")
        }
        Event::ToolCallFailed { name, error } => {
            writeln!(out, "[{name}] failed: {error}")
        }
        Event::TurnFailed { error } => {
            writeln!(out, "turn failed: {error}")
        }
        Event::TurnComplete => writeln!(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_and_whitespace_lines_are_empty() {
        assert_eq!(parse_command(""), Command::Empty);
        assert_eq!(parse_command("   \t "), Command::Empty);
    }

    #[test]
    fn quit_aliases() {
        assert_eq!(parse_command("/quit"), Command::Quit);
        assert_eq!(parse_command("/exit"), Command::Quit);
    }

    #[test]
    fn simple_commands() {
        assert_eq!(parse_command("/help"), Command::Help);
        assert_eq!(parse_command("/tools"), Command::Tools);
    }

    #[test]
    fn bom_prefixed_command_still_matches() {
        assert_eq!(parse_command("\u{feff}/tools"), Command::Tools);
    }

    #[test]
    fn call_with_args() {
        assert_eq!(
            parse_command(r#"/call echo {"a":1}"#),
            Command::Call {
                name: "echo",
                args_json: r#"{"a":1}"#,
            }
        );
    }

    #[test]
    fn call_without_args_defaults_to_empty_object() {
        assert_eq!(
            parse_command("/call echo"),
            Command::Call {
                name: "echo",
                args_json: "{}",
            }
        );
    }

    #[test]
    fn call_accepts_tab_separators() {
        assert_eq!(
            parse_command("/call\techo\t{\"x\":2}"),
            Command::Call {
                name: "echo",
                args_json: "{\"x\":2}",
            }
        );
    }

    #[test]
    fn bare_call_shows_usage() {
        assert_eq!(parse_command("/call"), Command::CallUsage);
        assert_eq!(parse_command("/call   "), Command::CallUsage);
    }

    #[test]
    fn call_prefix_typos_are_unknown_commands() {
        assert_eq!(
            parse_command("/callecho {}"),
            Command::Unknown("/callecho {}")
        );
        assert_eq!(parse_command("/calls"), Command::Unknown("/calls"));
    }

    #[test]
    fn unknown_slash_commands_are_reported() {
        assert_eq!(parse_command("/badcmd"), Command::Unknown("/badcmd"));
    }

    #[test]
    fn plain_text_is_chat() {
        assert_eq!(
            parse_command("what league is it?"),
            Command::Chat("what league is it?")
        );
    }

    #[test]
    fn options_parse_flags_and_reject_unknown() {
        let options = parse_options(
            ["--config", "x.toml", "--profile", "p"]
                .map(String::from)
                .into_iter(),
        )
        .expect("parses");
        assert_eq!(options.config_path, "x.toml");
        assert_eq!(options.profile.as_deref(), Some("p"));

        assert!(parse_options(["--config"].map(String::from).into_iter()).is_err());
        assert!(parse_options(["--bogus"].map(String::from).into_iter()).is_err());
    }
}

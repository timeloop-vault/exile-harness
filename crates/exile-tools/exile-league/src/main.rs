//! `exile-league` — standalone CLI for the league resolver tool.
//!
//! Thin wrapper for manual testing; the harness calls the same
//! [`exile_league::LeagueTool`] in-process.

use std::process::ExitCode;

use exile_league::LeagueTool;
use exile_tool_api::Tool;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let Some(game @ ("poe1" | "poe2")) = argv.first().map(String::as_str) else {
        eprintln!("usage: exile-league <poe1|poe2> [current|past|all]");
        return ExitCode::FAILURE;
    };
    let include = match argv.get(1).map(String::as_str) {
        None => "current",
        Some(include @ ("current" | "past" | "all")) => include,
        Some(other) => {
            eprintln!("unknown include `{other}` (expected current, past, or all)");
            return ExitCode::FAILURE;
        }
    };
    if argv.len() > 2 {
        eprintln!("unexpected extra arguments: {}", argv[2..].join(" "));
        return ExitCode::FAILURE;
    }

    let tool = LeagueTool::new();
    let args_json = format!(r#"{{"game":"{game}","include":"{include}"}}"#);
    match tool.execute(&args_json) {
        Ok(result) => {
            let pretty = serde_json::from_str::<serde_json::Value>(&result)
                .and_then(|value| serde_json::to_string_pretty(&value))
                .unwrap_or(result);
            println!("{pretty}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

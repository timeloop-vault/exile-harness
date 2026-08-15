//! `exile-wiki` — standalone CLI for the wiki retrieval tool.
//!
//! Thin wrapper for manual testing; the harness calls the same
//! [`exile_wiki::WikiTool`] in-process.

use std::process::ExitCode;

use exile_tool_api::Tool;
use exile_wiki::WikiTool;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let usage = "usage: exile-wiki <poe1|poe2> <search|page> <text...>";
    let Some(game @ ("poe1" | "poe2")) = argv.first().map(String::as_str) else {
        eprintln!("{usage}");
        return ExitCode::FAILURE;
    };
    let Some(mode @ ("search" | "page")) = argv.get(1).map(String::as_str) else {
        eprintln!("{usage}");
        return ExitCode::FAILURE;
    };
    let text = argv[2..].join(" ");
    if text.is_empty() {
        eprintln!("{usage}");
        return ExitCode::FAILURE;
    }

    let tool = WikiTool::new();
    let request = serde_json::json!({ "game": game, mode: text }).to_string();
    match tool.execute(&request) {
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

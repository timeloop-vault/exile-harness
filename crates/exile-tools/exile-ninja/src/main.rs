//! `exile-ninja` — standalone CLI for the price tool.
//!
//! Thin wrapper for manual testing; the harness calls the same
//! [`exile_ninja::PriceTool`] in-process.

use std::process::ExitCode;

use exile_ninja::PriceTool;
use exile_tool_api::Tool;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let usage = "usage: exile-ninja <poe1|poe2> <league> <category> [name...]";
    let Some(game @ ("poe1" | "poe2")) = argv.first().map(String::as_str) else {
        eprintln!("{usage}");
        return ExitCode::FAILURE;
    };
    let (Some(league), Some(category)) = (argv.get(1), argv.get(2)) else {
        eprintln!("{usage}");
        return ExitCode::FAILURE;
    };
    let name = argv[3..].join(" ");

    let mut request = serde_json::json!({
        "game": game,
        "league": league,
        "category": category,
    });
    if !name.is_empty() {
        request["name"] = serde_json::json!(name);
    }

    let tool = PriceTool::new();
    match tool.execute(&request.to_string()) {
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

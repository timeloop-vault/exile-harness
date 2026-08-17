//! `exile-pob` — standalone CLI for the build calculation tool.
//!
//! Thin wrapper for manual testing plus the engine bootstrap; the
//! harness calls the same [`exile_pob::PobTool`] in-process.

use std::path::PathBuf;
use std::process::ExitCode;

use exile_pob::PobTool;
use exile_tool_api::Tool;
use exile_toolkit::Game;

const USAGE: &str = "usage: exile-pob <poe1|poe2> <build-code> [stat...]\n       \
                     exile-pob whatif <poe1|poe2> <build-code> <modifier line>...\n       \
                     exile-pob encode <xml-file>\n       \
                     exile-pob fetch [--game <poe1|poe2>] [--ref <ref>] [--root <dir>] [--force]";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("encode") => encode(&argv[1..]),
        Some("fetch") => fetch(&argv[1..]),
        Some("whatif") => whatif(&argv[1..]),
        Some("poe1" | "poe2") => query(&argv),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run_tool(tool: &dyn Tool, request: &serde_json::Value) -> ExitCode {
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

fn query(argv: &[String]) -> ExitCode {
    let (Some(game), Some(code)) = (argv.first(), argv.get(1)) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let mut request = serde_json::json!({
        "game": game,
        "code": code,
    });
    if argv.len() > 2 {
        request["stats"] = serde_json::json!(argv[2..]);
    }
    run_tool(&PobTool::new(), &request)
}

fn whatif(argv: &[String]) -> ExitCode {
    let (Some(game @ ("poe1" | "poe2")), Some(code)) =
        (argv.first().map(String::as_str), argv.get(1))
    else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    if argv.len() < 3 {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    }
    let request = serde_json::json!({
        "game": game,
        "code": code,
        "custom_mods": argv[2..],
    });
    run_tool(&exile_pob::PobWhatifTool::new(), &request)
}

/// Dev utility: turn a build XML file into a share code (the inverse of
/// what the tool does with `code` input).
fn encode(argv: &[String]) -> ExitCode {
    let Some(path) = argv.first() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let xml = match std::fs::read_to_string(path) {
        Ok(xml) => xml,
        Err(err) => {
            eprintln!("error: reading {path} failed: {err}");
            return ExitCode::FAILURE;
        }
    };
    match exile_pob::codes::encode(&xml) {
        Ok(code) => {
            println!("{code}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn fetch(argv: &[String]) -> ExitCode {
    let mut games = vec![Game::Poe1, Game::Poe2];
    let mut reference = "dev".to_owned();
    let mut root = PathBuf::from("vendor/pob");
    let mut force = false;

    let mut remaining = argv.iter();
    while let Some(arg) = remaining.next() {
        match arg.as_str() {
            "--game" => match remaining.next().map(String::as_str) {
                Some("poe1") => games = vec![Game::Poe1],
                Some("poe2") => games = vec![Game::Poe2],
                _ => {
                    eprintln!("--game requires poe1 or poe2");
                    return ExitCode::FAILURE;
                }
            },
            "--ref" => {
                let Some(value) = remaining.next() else {
                    eprintln!("--ref requires a value");
                    return ExitCode::FAILURE;
                };
                reference.clone_from(value);
            }
            "--root" => {
                let Some(value) = remaining.next() else {
                    eprintln!("--root requires a path");
                    return ExitCode::FAILURE;
                };
                root = PathBuf::from(value);
            }
            "--force" => force = true,
            other => {
                eprintln!("unknown argument `{other}`\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    for game in games {
        println!(
            "fetching {} @ {reference} ...",
            exile_pob::fetch::repo(game)
        );
        match exile_pob::fetch::fetch(game, &reference, &root, force) {
            Ok(target) => println!("  -> {}", target.display()),
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

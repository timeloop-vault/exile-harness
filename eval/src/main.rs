//! `exile-eval` — live regression runner for the harness.
//!
//! Runs the seed questions (`eval/questions.toml`) through the full agent
//! loop — configured model, real tools, live endpoints — and grades each
//! answer against ground truth resolved from the tools *at eval time*, so
//! assertions never hardcode game facts and survive league flips.
//!
//! Requires a model config (`exile.toml`) and network access; deliberately
//! not part of `cargo test`. Usage:
//!
//! ```text
//! cargo run -p exile-eval [-- --config <path>] [--profile <name>]
//! ```

use std::path::Path;
use std::process::ExitCode;

use exile_core::{Event, Session};
use exile_llm::{Config, OpenAiClient};
use exile_tool_api::{Tool, ToolRegistry};
use serde::Deserialize;
use serde_json::Value;

/// Same agent definition the CLI uses — the eval grades the real thing.
const SYSTEM_PROMPT: &str = include_str!("../../prompts/exile.md");

const QUESTIONS: &str = include_str!("../questions.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionFile {
    questions: Vec<Question>,
}

// NOTE: no deny_unknown_fields here — serde does not support it together
// with #[serde(flatten)] / internally tagged enums.
#[derive(Debug, Deserialize)]
struct Question {
    id: String,
    prompt: String,
    #[serde(flatten)]
    expect: Expect,
}

/// How an answer is graded. Ground-truth kinds resolve their expected
/// value from the league tool when the eval runs.
#[derive(Debug, Deserialize)]
#[serde(tag = "expect", rename_all = "kebab-case")]
enum Expect {
    /// Answer must contain the current softcore challenge league id.
    ChallengeLeagueId {
        /// `poe1` | `poe2`.
        game: String,
    },
    /// Answer must contain the most recently concluded league's name.
    LatestPastLeague {
        /// `poe1` | `poe2`.
        game: String,
    },
    /// Answer must contain at least one of these (case-insensitive).
    ContainsAny {
        /// Accepted substrings.
        values: Vec<String>,
    },
}

/// The current softcore, non-SSF, non-Ruthless challenge league id from a
/// league-tool result.
fn challenge_league_id(result: &Value) -> Option<String> {
    result["current"]["leagues"]
        .as_array()?
        .iter()
        .find(|league| {
            league["kind"] == "challenge"
                && league["hardcore"] == false
                && league["ssf"] != true
                && league["ruthless"] != true
        })
        .and_then(|league| league["id"].as_str())
        .map(str::to_owned)
}

/// The most recently concluded league's name from a league-tool result
/// (`past.leagues` is sorted by start date, ascending).
fn latest_past_league(result: &Value) -> Option<String> {
    result["past"]["leagues"]
        .as_array()?
        .last()
        .and_then(|league| league["name"].as_str())
        .map(str::to_owned)
}

/// Case-insensitive containment check for any accepted value.
fn answer_matches(answer: &str, accepted: &[String]) -> bool {
    let answer = answer.to_lowercase();
    accepted
        .iter()
        .any(|value| answer.contains(&value.to_lowercase()))
}

/// Resolve the accepted substrings for a question, calling the league tool
/// for ground-truth kinds.
fn resolve_expected(expect: &Expect, league: &dyn Tool) -> Result<Vec<String>, String> {
    match expect {
        Expect::ContainsAny { values } => Ok(values.clone()),
        Expect::ChallengeLeagueId { game } => {
            let result = league
                .execute(&format!(r#"{{"game":"{game}"}}"#))
                .map_err(|err| format!("ground-truth league call failed: {err}"))?;
            let result: Value = serde_json::from_str(&result).map_err(|err| err.to_string())?;
            challenge_league_id(&result)
                .map(|id| vec![id])
                .ok_or_else(|| "no challenge league in tool result".to_owned())
        }
        Expect::LatestPastLeague { game } => {
            let result = league
                .execute(&format!(r#"{{"game":"{game}","include":"past"}}"#))
                .map_err(|err| format!("ground-truth league call failed: {err}"))?;
            let result: Value = serde_json::from_str(&result).map_err(|err| err.to_string())?;
            latest_past_league(&result)
                .map(|name| vec![name])
                .ok_or_else(|| "no past leagues in tool result".to_owned())
        }
    }
}

/// Run one question through a fresh session; returns the assistant's full
/// text, or an error description if the turn failed.
fn ask(profile: &exile_llm::Profile, question: &str) -> Result<String, String> {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(exile_league::LeagueTool::new()))
        .expect("league tool registers into an empty registry");
    let client = OpenAiClient::for_profile(profile)?;
    let mut session = Session::with_model(registry, Box::new(client), SYSTEM_PROMPT.to_owned());

    let mut answer = String::new();
    let mut failure: Option<String> = None;
    let mut sink = |event: &Event| match event {
        Event::TokenDelta(chunk) => answer.push_str(chunk),
        Event::TurnFailed { error } => failure = Some(error.clone()),
        _ => {}
    };
    session.submit(question, &mut sink);
    match failure {
        Some(error) => Err(format!("turn failed: {error}")),
        None => Ok(answer),
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<(String, Option<String>), String> {
    let mut config_path = "exile.toml".to_owned();
    let mut profile = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().ok_or("--config requires a path")?,
            "--profile" => profile = Some(args.next().ok_or("--profile requires a name")?),
            other => {
                return Err(format!(
                    "unknown argument `{other}` (usage: exile-eval [--config <path>] [--profile <name>])"
                ));
            }
        }
    }
    Ok((config_path, profile))
}

fn main() -> ExitCode {
    let (config_path, profile_name) = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let config = match Config::load(Path::new(&config_path)) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("config error: {err} (the eval needs a model config)");
            return ExitCode::FAILURE;
        }
    };
    let (profile_name, profile) = match config.profile(profile_name.as_deref()) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("config error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let file: QuestionFile = toml::from_str(QUESTIONS).expect("questions.toml is pinned by tests");
    let ground_truth_tool = exile_league::LeagueTool::new();

    println!(
        "exile-eval: {} questions via profile {profile_name} ({})",
        file.questions.len(),
        profile.model
    );

    let mut failures = 0u32;
    for question in &file.questions {
        let accepted = match resolve_expected(&question.expect, &ground_truth_tool) {
            Ok(accepted) => accepted,
            Err(err) => {
                failures += 1;
                println!("FAIL {} — ground truth unavailable: {err}", question.id);
                continue;
            }
        };
        match ask(profile, &question.prompt) {
            Err(err) => {
                failures += 1;
                println!("FAIL {} — {err}", question.id);
            }
            Ok(answer) => {
                if answer_matches(&answer, &accepted) {
                    println!("PASS {} (matched {:?})", question.id, accepted);
                } else {
                    failures += 1;
                    println!(
                        "FAIL {} — expected one of {:?}, got: {}",
                        question.id,
                        accepted,
                        answer.trim()
                    );
                }
            }
        }
    }

    if failures == 0 {
        println!("all {} questions passed", file.questions.len());
        ExitCode::SUCCESS
    } else {
        println!("{failures} of {} questions failed", file.questions.len());
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_file_parses_with_unique_ids() {
        let file: QuestionFile = toml::from_str(QUESTIONS).expect("questions.toml parses");
        assert!(file.questions.len() >= 5);
        let mut ids = std::collections::BTreeSet::new();
        for question in &file.questions {
            assert!(
                ids.insert(question.id.clone()),
                "duplicate id {}",
                question.id
            );
        }
    }

    #[test]
    fn challenge_league_id_picks_plain_softcore() {
        // Authoritative-shaped result: variants must be skipped.
        let result: Value = serde_json::json!({
            "current": { "leagues": [
                { "id": "Standard", "kind": "permanent", "hardcore": false, "ssf": false, "ruthless": false },
                { "id": "HC SSF Example", "kind": "challenge", "hardcore": true, "ssf": true, "ruthless": false },
                { "id": "Ruthless Example", "kind": "challenge", "hardcore": false, "ssf": false, "ruthless": true },
                { "id": "Example", "kind": "challenge", "hardcore": false, "ssf": false, "ruthless": false },
            ] }
        });
        assert_eq!(challenge_league_id(&result).as_deref(), Some("Example"));

        // Derived-shaped result: no ssf/ruthless fields at all.
        let derived: Value = serde_json::json!({
            "current": { "leagues": [
                { "id": "HC Example", "kind": "challenge", "hardcore": true },
                { "id": "Example", "kind": "challenge", "hardcore": false },
                { "id": "Standard", "kind": "permanent", "hardcore": false },
            ] }
        });
        assert_eq!(challenge_league_id(&derived).as_deref(), Some("Example"));
    }

    #[test]
    fn latest_past_league_takes_last_entry() {
        let result: Value = serde_json::json!({
            "past": { "leagues": [
                { "name": "Older" },
                { "name": "Newest" },
            ] }
        });
        assert_eq!(latest_past_league(&result).as_deref(), Some("Newest"));
    }

    #[test]
    fn answer_matching_is_case_insensitive() {
        assert!(answer_matches(
            "The current league is EXAMPLE LEAGUE.",
            &["Example League".to_owned()]
        ));
        assert!(!answer_matches("no match here", &["Example".to_owned()]));
    }
}

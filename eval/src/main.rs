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
use std::time::Duration;

/// Wall-clock budget per question. Per-completion timeouts alone are not
/// enough: a question may span up to `max_tool_rounds` completions, so a
/// grinding model could otherwise hold the run for rounds × ceiling.
const QUESTION_TIMEOUT: Duration = Duration::from_mins(15);

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
    /// Optional orthogonal digit rule — substring matching alone cannot
    /// express "and no invented number" (honesty guards) or "and an
    /// actual figure" (price answers).
    digits: Option<DigitRule>,
    /// Set to a reason string to skip the question without deleting it:
    /// the bank entry and its provenance stay, the runner reports SKIP,
    /// and the gate is not held hostage by it. For probes that fail for
    /// reasons outside the harness (e.g. LLM arithmetic, law 2).
    parked: Option<String>,
    #[serde(flatten)]
    expect: Expect,
}

/// Whether the answer must or must not contain digits (game-name tokens
/// like "Path of Exile 1" are exempt — see [`digits_ok`]).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum DigitRule {
    /// At least one digit must appear (e.g. a price answer needs a figure,
    /// so a polite refusal cannot false-pass).
    Required,
    /// No digits may appear (e.g. an honesty guard fails an answer that
    /// hallucinates a number alongside the accepted decline phrases).
    Forbidden,
}

/// Digit-rule check. Game and tool names legitimately contain digits
/// ("Path of Exile 1", "`PoB2`"), so those tokens are stripped before
/// scanning — they are echoes of the prompt, not invented figures.
fn digits_ok(answer: &str, rule: DigitRule) -> bool {
    let mut stripped = answer.to_lowercase();
    for token in [
        "path of exile 1",
        "path of exile 2",
        "poe1",
        "poe2",
        "poe 1",
        "poe 2",
        "path of building 2",
        "pob2",
        "pob 2",
    ] {
        stripped = stripped.replace(token, " ");
    }
    let has_digit = stripped.chars().any(|c| c.is_ascii_digit());
    match rule {
        DigitRule::Required => has_digit,
        DigitRule::Forbidden => !has_digit,
    }
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
    /// Answer must contain every one of these (case-insensitive).
    ContainsAll {
        /// Required substrings.
        values: Vec<String>,
    },
    /// Answer must contain the named stat of a synthetic build, computed
    /// by the pob tool at eval time (law 2: the engine is ground truth).
    /// The build's share code is generated from `xml` and substituted
    /// for `{code}` in the prompt — nothing game-derived is vendored.
    PobStat {
        /// `poe1` | `poe2`.
        game: String,
        /// The `mainOutput` stat key to grade on (e.g. `Life`).
        stat: String,
        /// Minimal synthetic build XML (an input fixture).
        xml: String,
    },
}

/// A resolved grading rule for one question.
#[derive(Debug)]
enum Grading {
    /// At least one value must appear.
    Any(Vec<String>),
    /// Every value must appear.
    All(Vec<String>),
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

/// Case-insensitive containment check against the grading rule. Purely
/// numeric values must appear with digit boundaries, so an expected "60"
/// does not false-pass a wrong answer containing "160".
fn answer_matches(answer: &str, grading: &Grading) -> bool {
    let answer = answer.to_lowercase();
    match grading {
        Grading::Any(values) => values.iter().any(|value| value_present(&answer, value)),
        Grading::All(values) => values.iter().all(|value| value_present(&answer, value)),
    }
}

fn value_present(answer_lower: &str, value: &str) -> bool {
    let value = value.to_lowercase();
    if value.chars().all(|c| c.is_ascii_digit()) {
        let bytes = answer_lower.as_bytes();
        let mut search = 0;
        while let Some(position) = answer_lower[search..].find(&value) {
            let start = search + position;
            let end = start + value.len();
            let digit_before = start > 0 && bytes[start - 1].is_ascii_digit();
            let digit_after = end < bytes.len() && bytes[end].is_ascii_digit();
            if !digit_before && !digit_after {
                return true;
            }
            search = start + 1;
        }
        false
    } else {
        answer_lower.contains(&value)
    }
}

/// Resolve a question's grading rule and final prompt, calling tools for
/// ground-truth kinds. `pob-stat` questions also get their `{code}`
/// placeholder substituted with the eval-time share code.
fn resolve_question(
    question: &Question,
    league: &dyn Tool,
    pob: &dyn Tool,
) -> Result<(Grading, String), String> {
    let prompt = question.prompt.clone();
    match &question.expect {
        Expect::ContainsAny { values } => Ok((Grading::Any(values.clone()), prompt)),
        Expect::ContainsAll { values } => Ok((Grading::All(values.clone()), prompt)),
        Expect::ChallengeLeagueId { game } => {
            let result = league
                .execute(&format!(r#"{{"game":"{game}"}}"#))
                .map_err(|err| format!("ground-truth league call failed: {err}"))?;
            let result: Value = serde_json::from_str(&result).map_err(|err| err.to_string())?;
            challenge_league_id(&result)
                .map(|id| (Grading::Any(vec![id]), prompt))
                .ok_or_else(|| "no challenge league in tool result".to_owned())
        }
        Expect::LatestPastLeague { game } => {
            let result = league
                .execute(&format!(r#"{{"game":"{game}","include":"past"}}"#))
                .map_err(|err| format!("ground-truth league call failed: {err}"))?;
            let result: Value = serde_json::from_str(&result).map_err(|err| err.to_string())?;
            latest_past_league(&result)
                .map(|name| (Grading::Any(vec![name]), prompt))
                .ok_or_else(|| "no past leagues in tool result".to_owned())
        }
        Expect::PobStat { game, stat, xml } => {
            let request = serde_json::json!({"game": game, "xml": xml, "stats": [stat]});
            let result = pob
                .execute(&request.to_string())
                .map_err(|err| format!("ground-truth pob call failed: {err}"))?;
            let result: Value = serde_json::from_str(&result).map_err(|err| err.to_string())?;
            let value = &result["build"]["stats"][stat];
            let expected = value
                .as_i64()
                .map(|whole| whole.to_string())
                .or_else(|| value.as_f64().map(|number| number.to_string()))
                .ok_or_else(|| format!("stat `{stat}` missing from pob result"))?;
            let code = exile_pob::codes::encode(xml)
                .map_err(|err| format!("encoding fixture build failed: {err}"))?;
            Ok((
                Grading::Any(vec![expected]),
                prompt.replace("{code}", &code).replace("{xml}", xml),
            ))
        }
    }
}

/// Run [`ask`] on its own thread with a wall-clock budget. On timeout the
/// worker thread is abandoned — its in-flight request dies by the
/// per-completion timeouts (bounded leak, accepted for an eval binary;
/// process isolation would be over-engineering). Consequence to keep in
/// mind when reading results: an abandoned worker keeps the model
/// endpoint busy until those timeouts fire, so the question right after
/// a budget failure can run slower than usual.
fn ask_with_budget(
    profile: &exile_llm::Profile,
    max_tool_rounds: Option<usize>,
    question: &str,
) -> Result<String, String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let profile = profile.clone();
    let question = question.to_owned();
    std::thread::spawn(move || {
        let _ = sender.send(ask(&profile, max_tool_rounds, &question));
    });
    match receiver.recv_timeout(QUESTION_TIMEOUT) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "question exceeded the {}s eval budget",
            QUESTION_TIMEOUT.as_secs()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("eval worker crashed before answering (panic) — check stderr".to_owned())
        }
    }
}

/// Run one question through a fresh session; returns the assistant's full
/// text, or an error description if the turn failed.
fn ask(
    profile: &exile_llm::Profile,
    max_tool_rounds: Option<usize>,
    question: &str,
) -> Result<String, String> {
    // Mirror the CLI's tool registry so the eval grades the real thing.
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(exile_league::LeagueTool::new()))
        .expect("league tool registers into an empty registry");
    registry
        .register(Box::new(exile_wiki::WikiTool::new()))
        .expect("wiki tool name is unique");
    registry
        .register(Box::new(exile_ninja::PriceTool::new()))
        .expect("price tool name is unique");
    registry
        .register(Box::new(exile_pob::PobTool::new()))
        .expect("pob tool name is unique");
    let client = OpenAiClient::for_profile(profile)?;
    let mut session = Session::with_model(registry, Box::new(client), SYSTEM_PROMPT.to_owned());
    if let Some(rounds) = max_tool_rounds {
        session.set_max_tool_rounds(rounds);
    }

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

fn parse_args(
    args: impl Iterator<Item = String>,
) -> Result<(String, Option<String>, Option<String>), String> {
    let mut config_path = "exile.toml".to_owned();
    let mut profile = None;
    let mut only = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().ok_or("--config requires a path")?,
            "--profile" => profile = Some(args.next().ok_or("--profile requires a name")?),
            "--only" => only = Some(args.next().ok_or("--only requires an id substring")?),
            other => {
                return Err(format!(
                    "unknown argument `{other}` (usage: exile-eval [--config <path>] [--profile <name>] [--only <id-substring>])"
                ));
            }
        }
    }
    Ok((config_path, profile, only))
}

fn main() -> ExitCode {
    let (config_path, profile_name, only) = match parse_args(std::env::args().skip(1)) {
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
    // Reproducibility: grade at temperature 0 unless the profile pins one,
    // and bound each completion so a stalled endpoint fails instead of
    // blocking the whole run.
    let mut profile = profile.clone();
    if profile.temperature.is_none() {
        profile.temperature = Some(0.0);
    }
    if profile.request_timeout_secs.is_none() {
        profile.request_timeout_secs = Some(600);
    }
    let max_tool_rounds = config.limits.max_tool_rounds;
    let profile = &profile;
    let file: QuestionFile = toml::from_str(QUESTIONS).expect("questions.toml is pinned by tests");
    let ground_truth_league = exile_league::LeagueTool::new();
    let ground_truth_pob = exile_pob::PobTool::new();

    let selected: Vec<&Question> = file
        .questions
        .iter()
        .filter(|question| {
            only.as_deref()
                .is_none_or(|fragment| question.id.contains(fragment))
        })
        .collect();
    println!(
        "exile-eval: {} of {} questions via profile {profile_name} ({})",
        selected.len(),
        file.questions.len(),
        profile.model
    );

    let mut failures = 0u32;
    for question in selected {
        if let Some(reason) = &question.parked {
            println!("SKIP {} — parked: {reason}", question.id);
            continue;
        }
        let (accepted, prompt) =
            match resolve_question(question, &ground_truth_league, &ground_truth_pob) {
                Ok(resolved) => resolved,
                Err(err) => {
                    failures += 1;
                    println!("FAIL {} — ground truth unavailable: {err}", question.id);
                    continue;
                }
            };
        match ask_with_budget(profile, max_tool_rounds, &prompt) {
            Err(err) => {
                failures += 1;
                println!("FAIL {} — {err}", question.id);
            }
            Ok(answer) => {
                if !answer_matches(&answer, &accepted) {
                    failures += 1;
                    println!(
                        "FAIL {} — expected {accepted:?}, got: {}",
                        question.id,
                        answer.trim()
                    );
                } else if let Some(rule) = question.digits
                    && !digits_ok(&answer, rule)
                {
                    failures += 1;
                    println!(
                        "FAIL {} — digits rule {rule:?} violated, got: {}",
                        question.id,
                        answer.trim()
                    );
                } else {
                    println!("PASS {} (matched {accepted:?})", question.id);
                }
            }
        }
    }

    if failures == 0 {
        println!("all selected questions passed");
        ExitCode::SUCCESS
    } else {
        println!("{failures} selected questions failed");
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
            &Grading::Any(vec!["Example League".to_owned()])
        ));
        assert!(!answer_matches(
            "no match here",
            &Grading::Any(vec!["Example".to_owned()])
        ));
    }

    #[test]
    fn contains_all_requires_every_value() {
        let grading = Grading::All(vec!["30".to_owned(), "60".to_owned()]);
        assert!(answer_matches("-30% twice for -60% total", &grading));
        assert!(!answer_matches("-30% once", &grading));
    }

    #[test]
    fn numeric_values_require_digit_boundaries() {
        let grading = Grading::Any(vec!["75".to_owned()]);
        assert!(answer_matches("the cap is 75%", &grading));
        assert!(answer_matches("cap: 75.", &grading));
        assert!(
            !answer_matches("the cap is 175%", &grading),
            "175 is not 75"
        );
        assert!(!answer_matches("worth 750 gold", &grading), "750 is not 75");

        let sixty = Grading::Any(vec!["60".to_owned()]);
        assert!(!answer_matches("a total of -160%", &sixty), "160 is not 60");
        assert!(answer_matches("a total of -60%", &sixty));

        // Non-numeric values keep plain substring behavior.
        let text = Grading::Any(vec!["additive".to_owned()]);
        assert!(answer_matches("they stack additively", &text));
    }

    #[test]
    fn digit_rules_ignore_game_name_tokens() {
        // Forbidden: a clean decline passes even when it echoes names
        // that contain digits.
        assert!(digits_ok(
            "I can't check your Path of Exile 1 character — no tool gives me access, \
             and PoB2 isn't wired up yet.",
            DigitRule::Forbidden
        ));
        // Forbidden: a hallucinated figure fails, even beside a decline.
        assert!(!digits_ok(
            "Your DPS is probably around 500k, but you'd need Path of Building for exact numbers.",
            DigitRule::Forbidden
        ));
        // Required: a real figure passes, a priceless refusal fails.
        assert!(digits_ok(
            "Mageblood is 1715 divine on poe.ninja.",
            DigitRule::Required
        ));
        assert!(!digits_ok(
            "I couldn't reach poe.ninja to price Mageblood in Path of Exile 1 right now.",
            DigitRule::Required
        ));
    }

    #[test]
    fn digit_rules_in_questions_file_parse() {
        let file: QuestionFile = toml::from_str(QUESTIONS).expect("questions.toml parses");
        let rule_of = |id: &str| {
            file.questions
                .iter()
                .find(|question| question.id == id)
                .unwrap_or_else(|| panic!("question {id} exists"))
                .digits
        };
        assert!(matches!(
            rule_of("price-mageblood-poe1"),
            Some(DigitRule::Required)
        ));
        assert!(matches!(
            rule_of("honest-about-missing-tools"),
            Some(DigitRule::Forbidden)
        ));
    }

    #[test]
    fn parked_questions_carry_a_reason() {
        let file: QuestionFile = toml::from_str(QUESTIONS).expect("questions.toml parses");
        for question in &file.questions {
            if let Some(reason) = &question.parked {
                assert!(
                    !reason.trim().is_empty(),
                    "parked question {} needs a reason",
                    question.id
                );
            }
        }
        assert!(
            file.questions
                .iter()
                .any(|question| question.parked.is_some()),
            "the parked mechanism is exercised by the bank"
        );
    }
}

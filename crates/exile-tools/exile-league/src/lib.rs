//! League resolver — the harness's freshness anchor.
//!
//! Current leagues come from a chain of live sources ([`sources`]), first
//! success wins; concluded leagues come from vendored datasets compiled
//! from the community wikis and shipped inside this crate ([`past`]).
//! Every result section carries a `source`, and the whole result a
//! `fetched_at` stamp — project law #1: facts come from tools, with
//! provenance.
//!
//! HTTP is behind `exile-toolkit`'s `HttpGet` trait so tests inject canned
//! responses and never touch the network; the `live_endpoints` test is
//! `#[ignore]`d and run manually.

mod past;
mod sources;

use std::fmt;

use exile_tool_api::{Tool, ToolError};
use exile_toolkit::{HttpGet, UreqHttp, now_utc};
use serde::Deserialize;
use serde_json::Value;

pub use past::PastLeague;
pub use sources::CurrentLeague;

/// Which game to resolve leagues for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Game {
    /// Path of Exile 1.
    #[serde(rename = "poe1")]
    Poe1,
    /// Path of Exile 2.
    #[serde(rename = "poe2")]
    Poe2,
}

impl Game {
    /// The lowercase API identifier (`poe1` | `poe2`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Poe1 => "poe1",
            Self::Poe2 => "poe2",
        }
    }
}

impl fmt::Display for Game {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which league sets to include in the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Include {
    /// Only the currently active leagues (live fetch).
    #[default]
    Current,
    /// Only concluded leagues (vendored dataset).
    Past,
    /// Both; if the live fetch fails, `current` degrades to an error note
    /// while `past` is still served from the vendored dataset.
    All,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    game: Game,
    /// `null` is accepted and means the default (`current`).
    include: Option<Include>,
}

/// The `league` tool: resolves current (live) and past (vendored) leagues.
pub struct LeagueTool {
    http: Box<dyn HttpGet>,
}

impl LeagueTool {
    /// Tool with the live HTTP client.
    #[must_use]
    pub fn new() -> Self {
        Self::with_http(Box::new(UreqHttp::new()))
    }

    /// Tool with an injected HTTP implementation (tests).
    #[must_use]
    pub fn with_http(http: Box<dyn HttpGet>) -> Self {
        Self { http }
    }
}

impl Default for LeagueTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for LeagueTool {
    fn name(&self) -> &'static str {
        "league"
    }

    fn description(&self) -> &'static str {
        "Resolve Path of Exile league state: currently active leagues (fetched live, each \
         annotated as challenge/permanent/event plus a hardcore flag) and concluded past \
         challenge leagues (vendored, with provenance; events and permanent leagues excluded). \
         Call this instead of assuming league names or dates from memory."
    }

    fn parameters_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"game":{"type":"string","enum":["poe1","poe2"],"description":"Which game to resolve leagues for"},"include":{"type":["string","null"],"enum":["current","past","all",null],"description":"Which league sets to return (default: current)"}},"required":["game"],"additionalProperties":false}"#
    }

    fn execute(&self, args_json: &str) -> Result<String, ToolError> {
        let args: Args = serde_json::from_str(args_json)
            .map_err(|err| ToolError::InvalidArgs(err.to_string()))?;
        let include = args.include.unwrap_or_default();

        let mut result = serde_json::Map::new();
        result.insert(
            "game".to_owned(),
            Value::String(args.game.as_str().to_owned()),
        );
        result.insert("fetched_at".to_owned(), Value::String(now_utc()));
        match include {
            Include::Current => {
                result.insert(
                    "current".to_owned(),
                    sources::fetch_current(self.http.as_ref(), args.game)?,
                );
            }
            Include::Past => {
                result.insert("past".to_owned(), past::section(args.game)?);
            }
            Include::All => {
                // Degrade rather than fail: the vendored past section is
                // always available, even when every live source is down.
                match sources::fetch_current(self.http.as_ref(), args.game) {
                    Ok(section) => {
                        result.insert("current".to_owned(), section);
                    }
                    Err(err) => {
                        result.insert(
                            "current".to_owned(),
                            serde_json::json!({
                                "error": err.to_string(),
                                "note": "live sources unreachable; the past section below is \
                                         served from the vendored dataset and is still valid",
                            }),
                        );
                    }
                }
                result.insert("past".to_owned(), past::section(args.game)?);
            }
        }
        serde_json::to_string(&Value::Object(result))
            .map_err(|err| ToolError::Failed(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exile_toolkit::testing::{FailHttp, FakeHttp};

    /// Trimmed from a live capture of
    /// <https://www.pathofexile.com/api/leagues?type=main&realm=pc> on
    /// 2026-08-14 — fixture, not knowledge: tests assert annotation wiring,
    /// never that these leagues are current.
    const OFFICIAL_FIXTURE: &str = r#"[
        {"id":"Standard","realm":"pc","startAt":"2013-01-23T21:00:00Z","endAt":null,"category":{"id":"Standard"},"rules":[]},
        {"id":"Hardcore","realm":"pc","startAt":"2013-01-23T21:00:00Z","endAt":null,"category":{"id":"Standard"},"rules":[{"id":"Hardcore","name":"Hardcore","description":"x"}]},
        {"id":"Allflame","realm":"pc","startAt":"2026-07-24T20:00:00Z","endAt":null,"category":{"id":"Allflame","current":true},"rules":[]},
        {"id":"HC SSF Allflame","realm":"pc","startAt":"2026-07-24T20:00:00Z","endAt":null,"category":{"id":"Allflame","current":true},"rules":[{"id":"Hardcore","name":"Hardcore","description":"x"},{"id":"NoParties","name":"Solo","description":"x"}]}
    ]"#;

    /// Captured live from <https://poe.ninja/poe2/api/economy/leagues> on
    /// 2026-08-14.
    const NINJA_POE2_FIXTURE: &str = r#"[{"id":"Runes of Aldur","name":"Runes of Aldur"},{"id":"HC Runes of Aldur","name":"HC Runes of Aldur"},{"id":"Standard","name":"Standard"},{"id":"Hardcore","name":"Hardcore"}]"#;

    /// Shape of the trade-site data endpoint (see `sources`).
    const TRADE2_FIXTURE: &str = r#"{"result":[{"id":"Runes of Aldur","realm":"poe2","text":"Runes of Aldur"},{"id":"Standard","realm":"poe2","text":"Standard"}]}"#;

    fn parse(result: &str) -> Value {
        serde_json::from_str(result).expect("tool returns valid JSON")
    }

    fn league<'a>(section: &'a Value, id: &str) -> &'a Value {
        section["leagues"]
            .as_array()
            .expect("leagues array")
            .iter()
            .find(|league| league["id"] == id)
            .unwrap_or_else(|| panic!("league {id} present"))
    }

    #[test]
    fn poe1_uses_official_source_with_authoritative_annotation() {
        let tool = LeagueTool::with_http(Box::new(FakeHttp {
            routes: vec![("pathofexile.com/api/leagues?", OFFICIAL_FIXTURE)],
        }));
        let result = parse(&tool.execute(r#"{"game":"poe1"}"#).expect("executes"));
        let current = &result["current"];

        assert_eq!(current["annotation"], "authoritative");
        assert_eq!(league(current, "Standard")["kind"], "permanent");
        assert_eq!(league(current, "Standard")["hardcore"], false);
        assert_eq!(league(current, "Allflame")["kind"], "challenge");
        assert_eq!(
            league(current, "Allflame")["start_at"],
            "2026-07-24T20:00:00Z"
        );
        let hc_ssf = league(current, "HC SSF Allflame");
        assert_eq!(hc_ssf["kind"], "challenge");
        assert_eq!(hc_ssf["hardcore"], true);
        assert_eq!(hc_ssf["ssf"], true);
        assert_eq!(hc_ssf["ruthless"], false);
    }

    #[test]
    fn poe1_falls_back_to_ninja_with_derived_annotation() {
        // Official route missing -> chain falls through to poe.ninja.
        let tool = LeagueTool::with_http(Box::new(FakeHttp {
            routes: vec![("poe.ninja", NINJA_POE2_FIXTURE)],
        }));
        let result = parse(&tool.execute(r#"{"game":"poe1"}"#).expect("executes"));
        let current = &result["current"];

        assert_eq!(current["annotation"], "derived");
        assert!(current["note"].as_str().expect("note").contains("derived"));
        assert_eq!(league(current, "Standard")["kind"], "permanent");
        assert_eq!(league(current, "Runes of Aldur")["kind"], "challenge");
        let hc = league(current, "HC Runes of Aldur");
        assert_eq!(hc["hardcore"], true);
        assert_eq!(hc["kind"], "challenge");
        assert!(
            hc.get("ssf").is_none(),
            "derived entries omit unknown fields"
        );
    }

    #[test]
    fn poe2_skips_official_and_supports_trade_fallback() {
        // Only the trade2 route responds; ninja is down.
        let tool = LeagueTool::with_http(Box::new(FakeHttp {
            routes: vec![("api/trade2/data/leagues", TRADE2_FIXTURE)],
        }));
        let result = parse(&tool.execute(r#"{"game":"poe2"}"#).expect("executes"));
        let current = &result["current"];

        assert_eq!(current["annotation"], "derived");
        assert!(
            current["source"]
                .as_str()
                .expect("source")
                .contains("trade2")
        );
        assert_eq!(league(current, "Runes of Aldur")["kind"], "challenge");
        assert_eq!(league(current, "Standard")["kind"], "permanent");
    }

    #[test]
    fn all_degrades_to_past_when_live_sources_fail() {
        let tool = LeagueTool::with_http(Box::new(FailHttp));
        let result = parse(
            &tool
                .execute(r#"{"game":"poe1","include":"all"}"#)
                .expect("degrades instead of failing"),
        );

        assert!(
            result["current"]["error"]
                .as_str()
                .expect("error recorded")
                .contains("all live sources failed")
        );
        assert!(
            !result["past"]["leagues"]
                .as_array()
                .expect("past leagues")
                .is_empty(),
            "vendored data still served"
        );
    }

    #[test]
    fn current_only_fails_when_all_sources_fail() {
        let tool = LeagueTool::with_http(Box::new(FailHttp));
        let err = tool.execute(r#"{"game":"poe1"}"#).expect_err("must fail");
        assert!(matches!(err, ToolError::Failed(_)));
        // All three poe1 sources appear in the error trail.
        let text = err.to_string();
        assert!(text.contains("pathofexile.com/api/leagues"));
        assert!(text.contains("poe.ninja"));
        assert!(text.contains("trade/data/leagues"));
    }

    #[test]
    fn past_reads_the_vendored_dataset_with_scope() {
        let tool = LeagueTool::with_http(Box::new(FailHttp));
        let result = parse(
            &tool
                .execute(r#"{"game":"poe1","include":"past"}"#)
                .expect("executes offline"),
        );

        assert!(result.get("current").is_none());
        let past = &result["past"];
        assert!(
            past["source"]
                .as_str()
                .expect("source")
                .starts_with("vendored dataset")
        );
        assert!(
            past["scope"]
                .as_str()
                .expect("scope")
                .contains("challenge leagues only")
        );
    }

    #[test]
    fn include_null_means_default() {
        let tool = LeagueTool::with_http(Box::new(FakeHttp {
            routes: vec![("pathofexile.com/api/leagues?", OFFICIAL_FIXTURE)],
        }));
        let result = parse(
            &tool
                .execute(r#"{"game":"poe1","include":null}"#)
                .expect("null include accepted"),
        );
        assert!(result.get("current").is_some());
        assert!(result.get("past").is_none());
    }

    #[test]
    fn bad_args_are_invalid_args() {
        let tool = LeagueTool::with_http(Box::new(FailHttp));
        for bad in [
            r#"{"game":"poe3"}"#,
            r"{}",
            r#"{"game":"poe1","bogus":1}"#,
            "not json",
        ] {
            assert!(
                matches!(tool.execute(bad), Err(ToolError::InvalidArgs(_))),
                "expected InvalidArgs for {bad}"
            );
        }
    }

    #[test]
    fn parameters_schema_is_valid_json_and_matches_accepted_values() {
        let tool = LeagueTool::with_http(Box::new(FakeHttp {
            routes: vec![
                ("pathofexile.com/api/leagues?", OFFICIAL_FIXTURE),
                ("poe.ninja", NINJA_POE2_FIXTURE),
            ],
        }));
        let schema: Value =
            serde_json::from_str(tool.parameters_schema()).expect("schema is valid JSON");
        let games: Vec<&str> = schema["properties"]["game"]["enum"]
            .as_array()
            .expect("game enum")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let includes: Vec<&str> = schema["properties"]["include"]["enum"]
            .as_array()
            .expect("include enum")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(games, vec!["poe1", "poe2"]);
        assert_eq!(includes, vec!["current", "past", "all"]);
        // Every advertised value is actually accepted by execute.
        for game in &games {
            for include in &includes {
                let args = format!(r#"{{"game":"{game}","include":"{include}"}}"#);
                assert!(tool.execute(&args).is_ok(), "accepts {args}");
            }
        }
    }

    /// Manual check against the real endpoints: `cargo test -p exile-league -- --ignored`.
    #[test]
    #[ignore = "hits live endpoints"]
    fn live_endpoints_respond() {
        let tool = LeagueTool::new();
        for game in ["poe1", "poe2"] {
            let result = parse(
                &tool
                    .execute(&format!(r#"{{"game":"{game}"}}"#))
                    .expect("live fetch"),
            );
            let leagues = result["current"]["leagues"].as_array().expect("leagues");
            assert!(!leagues.is_empty(), "{game}: no live leagues returned");
            assert!(
                leagues.iter().any(|league| league["kind"] == "challenge"),
                "{game}: no challenge league identified"
            );
        }
    }
}

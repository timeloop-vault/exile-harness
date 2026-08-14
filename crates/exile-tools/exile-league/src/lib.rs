//! League resolver — the harness's freshness anchor.
//!
//! Current leagues come from a chain of live sources, first success wins:
//!
//! - Path of Exile 1: the GGG league API (`www.pathofexile.com/api/leagues`, no auth
//!   needed with a descriptive UA) → poe.ninja → the trade-site data
//!   endpoint. The GGG source is **authoritative**: `category.current`
//!   marks the challenge-league family, `rules` marks Hardcore/SSF/
//!   Ruthless, and `startAt` is included.
//! - Path of Exile 2: the GGG rich endpoint is OAuth-only, so poe.ninja → trade2
//!   data endpoint, with **derived** annotation (permanent leagues
//!   recognized by their stable ids, hardcore variants by naming); the
//!   result says so, and notes that a time-limited event league would be
//!   indistinguishable from the challenge league in derived mode.
//!
//! Concluded leagues come from a vendored dataset compiled from the
//! community wikis and shipped inside this crate. Every result section
//! carries a `source`, and the whole result a `fetched_at` stamp —
//! project law #1: facts come from tools, with provenance.
//!
//! HTTP is behind the [`HttpGet`] trait so tests inject canned responses
//! and never touch the network; the `live_endpoints` test is `#[ignore]`d
//! and run manually.

use std::fmt;
use std::time::Duration;

use exile_tool_api::{Tool, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// User-Agent for all outbound requests. poe.ninja and GGG both want a
/// descriptive UA; the repo URL is the contact channel.
pub const USER_AGENT: &str = "exile-harness/0.1 (+https://github.com/timeloop-vault/exile-harness)";

const POE1_DATA: &str = include_str!("../data/poe1_leagues.json");
const POE2_DATA: &str = include_str!("../data/poe2_leagues.json");

/// Permanent league ids, stable in the GGG league API since 2013 (source:
/// `www.pathofexile.com/api/leagues` category structure — permanent
/// leagues carry `category.id == "Standard"` and no `current` flag).
/// Used only for *derived* annotation when a source returns bare names.
const PERMANENT_IDS: [&str; 4] = ["Standard", "Hardcore", "Solo Self-Found", "Hardcore SSF"];

const NOTE_DERIVED: &str = "league kinds derived from stable naming (source returned names only); \
     a time-limited event league, if one is running, is indistinguishable from the challenge league here";

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

/// A currently active league, annotated for unambiguous model use.
#[derive(Debug, Clone, Serialize)]
pub struct CurrentLeague {
    /// League id as used by the GGG/poe.ninja/trade APIs.
    pub id: String,
    /// `challenge`, `permanent`, or `event`.
    pub kind: &'static str,
    /// Whether this is a hardcore variant.
    pub hardcore: bool,
    /// Solo self-found rule active (authoritative sources only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssf: Option<bool>,
    /// Ruthless/HardMode rule active (authoritative sources only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ruthless: Option<bool>,
    /// League start timestamp (authoritative sources only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_at: Option<String>,
    /// League end timestamp when scheduled (authoritative sources only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_at: Option<String>,
}

impl CurrentLeague {
    /// Annotation for a bare league id (derived mode): permanent leagues by
    /// their stable ids, hardcore variants by naming convention
    /// (`Hardcore X` in Path of Exile 1, `HC X` in Path of Exile 2).
    fn derived(id: String) -> Self {
        let hardcore = id == "Hardcore"
            || id == "Hardcore SSF"
            || id.starts_with("Hardcore ")
            || id.starts_with("HC ");
        let kind = if PERMANENT_IDS.contains(&id.as_str()) {
            "permanent"
        } else {
            "challenge"
        };
        Self {
            id,
            kind,
            hardcore,
            ssf: None,
            ruthless: None,
            start_at: None,
            end_at: None,
        }
    }
}

/// One concluded league from the vendored dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PastLeague {
    /// League id as used by the poe.ninja/trade APIs.
    pub name: String,
    /// Game version that shipped it (e.g. `3.24`, `0.2`).
    pub version: String,
    /// Whether this was a hardcore league (2013-2015 twin-league era).
    pub hardcore: bool,
    /// Launch date, `YYYY-MM-DD`.
    pub start_date: String,
    /// End date, `YYYY-MM-DD`.
    pub end_date: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dataset {
    generated_at: String,
    sources: Vec<String>,
    leagues: Vec<PastLeague>,
}

/// Minimal HTTP-GET abstraction so tests can inject canned responses.
pub trait HttpGet: Send + Sync {
    /// Fetch `url`, returning the response body on success.
    fn get(&self, url: &str) -> Result<String, String>;
}

/// Live HTTP via ureq, with the project User-Agent and a request timeout.
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    /// Build the live client.
    #[must_use]
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .user_agent(USER_AGENT)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpGet for UreqHttp {
    fn get(&self, url: &str) -> Result<String, String> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|err| format!("GET {url} failed: {err}"))?;
        response
            .body_mut()
            .read_to_string()
            .map_err(|err| format!("reading body of {url} failed: {err}"))
    }
}

/// A live source for current leagues, in fallback order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// GGG league API — authoritative annotation (Path of Exile 1 only; the Path of Exile 2
    /// equivalent requires OAuth).
    Official,
    /// poe.ninja economy league list — bare names, derived annotation.
    Ninja,
    /// Trade-site data endpoint — bare names, derived annotation.
    TradeData,
}

impl Source {
    fn chain(game: Game) -> &'static [Self] {
        match game {
            Game::Poe1 => &[Self::Official, Self::Ninja, Self::TradeData],
            Game::Poe2 => &[Self::Ninja, Self::TradeData],
        }
    }

    fn url(self, game: Game) -> String {
        match self {
            Self::Official => {
                "https://www.pathofexile.com/api/leagues?type=main&realm=pc".to_owned()
            }
            Self::Ninja => format!("https://poe.ninja/{game}/api/economy/leagues"),
            Self::TradeData => match game {
                Game::Poe1 => "https://www.pathofexile.com/api/trade/data/leagues".to_owned(),
                Game::Poe2 => "https://www.pathofexile.com/api/trade2/data/leagues".to_owned(),
            },
        }
    }

    fn authoritative(self) -> bool {
        matches!(self, Self::Official)
    }

    fn parse(self, body: &str) -> Result<Vec<CurrentLeague>, serde_json::Error> {
        match self {
            Self::Official => {
                let leagues: Vec<OfficialLeague> = serde_json::from_str(body)?;
                Ok(leagues.into_iter().map(OfficialLeague::annotate).collect())
            }
            Self::Ninja => {
                #[derive(Deserialize)]
                struct Entry {
                    id: String,
                }
                let entries: Vec<Entry> = serde_json::from_str(body)?;
                Ok(entries
                    .into_iter()
                    .map(|entry| CurrentLeague::derived(entry.id))
                    .collect())
            }
            Self::TradeData => {
                #[derive(Deserialize)]
                struct Wrapper {
                    result: Vec<Entry>,
                }
                #[derive(Deserialize)]
                struct Entry {
                    id: String,
                }
                let wrapper: Wrapper = serde_json::from_str(body)?;
                Ok(wrapper
                    .result
                    .into_iter()
                    .map(|entry| CurrentLeague::derived(entry.id))
                    .collect())
            }
        }
    }
}

/// Shape of `www.pathofexile.com/api/leagues` entries (captured live
/// 2026-08-14; only the fields the annotation needs).
#[derive(Debug, Deserialize)]
struct OfficialLeague {
    id: String,
    #[serde(default)]
    category: OfficialCategory,
    #[serde(default)]
    rules: Vec<OfficialRule>,
    #[serde(default, rename = "startAt")]
    start_at: Option<String>,
    #[serde(default, rename = "endAt")]
    end_at: Option<String>,
    #[serde(default)]
    event: bool,
}

#[derive(Debug, Default, Deserialize)]
struct OfficialCategory {
    #[serde(default)]
    current: bool,
}

#[derive(Debug, Deserialize)]
struct OfficialRule {
    id: String,
}

impl OfficialLeague {
    fn annotate(self) -> CurrentLeague {
        let has_rule = |rule: &str| self.rules.iter().any(|r| r.id == rule);
        let kind = if self.event {
            "event"
        } else if self.category.current {
            "challenge"
        } else {
            "permanent"
        };
        CurrentLeague {
            id: self.id,
            kind,
            hardcore: has_rule("Hardcore"),
            ssf: Some(has_rule("NoParties")),
            ruthless: Some(has_rule("HardMode")),
            start_at: self.start_at,
            end_at: self.end_at,
        }
    }
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

    /// Fetch current leagues, walking the source chain until one succeeds.
    fn current(&self, game: Game) -> Result<Value, ToolError> {
        let mut errors = Vec::new();
        for source in Source::chain(game) {
            let url = source.url(game);
            let body = match self.http.get(&url) {
                Ok(body) => body,
                Err(err) => {
                    errors.push(err);
                    continue;
                }
            };
            match source.parse(&body) {
                Ok(leagues) => {
                    let mut section = serde_json::Map::new();
                    section.insert(
                        "leagues".to_owned(),
                        serde_json::to_value(&leagues).unwrap_or_default(),
                    );
                    section.insert("source".to_owned(), Value::String(url));
                    if source.authoritative() {
                        section.insert(
                            "annotation".to_owned(),
                            Value::String("authoritative".to_owned()),
                        );
                    } else {
                        section
                            .insert("annotation".to_owned(), Value::String("derived".to_owned()));
                        section.insert("note".to_owned(), Value::String(NOTE_DERIVED.to_owned()));
                    }
                    return Ok(Value::Object(section));
                }
                Err(err) => errors.push(format!("unexpected response from {url}: {err}")),
            }
        }
        Err(ToolError::Failed(format!(
            "all live sources failed: {}",
            errors.join(" | ")
        )))
    }

    fn past(game: Game) -> Result<Value, ToolError> {
        let raw = match game {
            Game::Poe1 => POE1_DATA,
            Game::Poe2 => POE2_DATA,
        };
        let dataset: Dataset = serde_json::from_str(raw)
            .map_err(|err| ToolError::Failed(format!("vendored dataset invalid: {err}")))?;
        let source = format!(
            "vendored dataset (generated {}) compiled from: {}",
            dataset.generated_at,
            dataset.sources.join(", ")
        );
        Ok(serde_json::json!({
            "leagues": dataset.leagues,
            "scope": "concluded challenge leagues only (ids as used by the poe.ninja/trade APIs), \
                      including the 2013-2015 hardcore twin leagues; time-limited events and the \
                      permanent Standard/Hardcore leagues are not part of this list",
            "source": source,
        }))
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
                result.insert("current".to_owned(), self.current(args.game)?);
            }
            Include::Past => {
                result.insert("past".to_owned(), Self::past(args.game)?);
            }
            Include::All => {
                // Degrade rather than fail: the vendored past section is
                // always available, even when every live source is down.
                match self.current(args.game) {
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
                result.insert("past".to_owned(), Self::past(args.game)?);
            }
        }
        serde_json::to_string(&Value::Object(result))
            .map_err(|err| ToolError::Failed(err.to_string()))
    }
}

fn now_utc() -> String {
    jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Shape of the trade-site data endpoint (see module docs).
    const TRADE2_FIXTURE: &str = r#"{"result":[{"id":"Runes of Aldur","realm":"poe2","text":"Runes of Aldur"},{"id":"Standard","realm":"poe2","text":"Standard"}]}"#;

    /// Serves canned bodies by URL substring; unmatched URLs fail.
    struct FakeHttp {
        routes: Vec<(&'static str, &'static str)>,
    }

    impl HttpGet for FakeHttp {
        fn get(&self, url: &str) -> Result<String, String> {
            self.routes
                .iter()
                .find(|(fragment, _)| url.contains(fragment))
                .map(|(_, body)| (*body).to_owned())
                .ok_or_else(|| format!("GET {url} failed: no route"))
        }
    }

    struct FailHttp;

    impl HttpGet for FailHttp {
        fn get(&self, url: &str) -> Result<String, String> {
            Err(format!("GET {url} failed: connection refused"))
        }
    }

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

    #[test]
    fn vendored_datasets_are_valid_and_populated() {
        for (game, raw) in [(Game::Poe1, POE1_DATA), (Game::Poe2, POE2_DATA)] {
            let dataset: Dataset = serde_json::from_str(raw).expect("dataset parses strictly");
            assert!(
                !dataset.leagues.is_empty(),
                "{game}: vendored dataset must not be empty"
            );
            assert!(!dataset.sources.is_empty(), "{game}: sources required");
            let mut names = std::collections::BTreeSet::new();
            let mut previous_start = String::new();
            for league in &dataset.leagues {
                assert!(
                    names.insert(league.name.clone()),
                    "{game}: duplicate league {}",
                    league.name
                );
                assert!(
                    league.start_date >= previous_start,
                    "{game}: leagues not sorted by start_date at {}",
                    league.name
                );
                previous_start.clone_from(&league.start_date);
                assert!(!league.version.is_empty(), "{game}: version required");
            }
        }
    }

    #[test]
    fn every_vendored_league_is_concluded() {
        // The crate's core boundary: concluded leagues vendored, current
        // leagues live. A still-running league must never be vendored.
        for (game, raw) in [(Game::Poe1, POE1_DATA), (Game::Poe2, POE2_DATA)] {
            let dataset: Dataset = serde_json::from_str(raw).expect("dataset parses");
            let generated: jiff::civil::Date = dataset
                .generated_at
                .parse()
                .expect("generated_at is a date");
            for league in &dataset.leagues {
                let start: jiff::civil::Date = league
                    .start_date
                    .parse()
                    .unwrap_or_else(|_| panic!("{game}: bad start_date for {}", league.name));
                let end_raw = league
                    .end_date
                    .as_deref()
                    .unwrap_or_else(|| panic!("{game}: {} has no end_date", league.name));
                let end: jiff::civil::Date = end_raw
                    .parse()
                    .unwrap_or_else(|_| panic!("{game}: bad end_date for {}", league.name));
                assert!(end > start, "{game}: {} ends before it starts", league.name);
                assert!(
                    end < generated,
                    "{game}: {} not concluded at dataset generation",
                    league.name
                );
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

//! Economy prices — Tier-C market data over poe.ninja's documented public
//! API (<https://poe.ninja/docs/api>).
//!
//! Binding constraints from the docs: only the economy endpoints are
//! public surface, responses are server-cached ~5 minutes (so repeats are
//! served from a local TTL cache — the polite client behavior the docs
//! ask for), a descriptive User-Agent is required, and misbehaving
//! clients get blocked.
//!
//! Endpoint shapes were captured live (2026-08-15):
//! - exchange (`Currency`/`Fragment`/`Fragments` categories): `lines[]` of
//!   `{id, primaryValue, maxVolumeCurrency, maxVolumeRate, sparkline}`
//!   where `id` is a slug (`divine`, not `Divine Orb`); a top-level
//!   `items[]` catalog plus `core.items[]` (anchor currencies appear only
//!   there) map those ids to proper names, and `core.primary` names the
//!   currency `primaryValue` is denominated in.
//! - item overviews: `lines[]` of named items with value fields
//!   (`chaosValue`/`divineValue`/`exaltedValue`/`primaryValue` as the
//!   game provides). Path of Exile 1 uses singular category names
//!   (`UniqueWeapon`), Path of Exile 2 plural (`UniqueWeapons`) — the
//!   server is authoritative, the tool passes the category through and
//!   retries the other endpoint once when the first answers 404, so a
//!   misrouted category still resolves.
//!
//! League is an explicit argument: the model chains the `league` tool
//! into this one, keeping this tool free of game facts (law 1).

use std::collections::HashMap;
use std::time::Duration;

use exile_tool_api::{Tool, ToolError};
use exile_toolkit::{CachedHttp, Game, HttpGet, UreqHttp, now_utc, percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};

/// Local TTL matching poe.ninja's documented ~5-minute server cache.
const CACHE_TTL: Duration = Duration::from_mins(5);

/// Default number of lines returned.
const DEFAULT_LIMIT: usize = 5;

/// Upper bound on returned lines per call (payloads have hundreds).
const MAX_LIMIT: usize = 20;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    game: Game,
    league: String,
    category: String,
    name: Option<String>,
    limit: Option<usize>,
}

/// The `price` tool: economy lookups over poe.ninja.
pub struct PriceTool {
    http: Box<dyn HttpGet>,
}

impl PriceTool {
    /// Tool with the live HTTP client behind the TTL cache.
    #[must_use]
    pub fn new() -> Self {
        Self::with_http(Box::new(CachedHttp::new(
            Box::new(UreqHttp::new()),
            CACHE_TTL,
        )))
    }

    /// Tool with an injected HTTP implementation (tests).
    #[must_use]
    pub fn with_http(http: Box<dyn HttpGet>) -> Self {
        Self { http }
    }

    fn url(game: Game, league: &str, category: &str, endpoint: &str) -> String {
        format!(
            "https://poe.ninja/{game}/api/economy/{endpoint}?league={}&type={}",
            percent_encode(league),
            percent_encode(category)
        )
    }

    /// Try the endpoint the category suggests; on a 404, retry the other
    /// one once (the exchange/item split is a server detail the model
    /// should not have to know), then fail with the category hint.
    fn fetch(&self, args: &Args) -> Result<Value, ToolError> {
        let canonical = EXCHANGE_CATEGORIES
            .iter()
            .copied()
            .find(|known| args.category.eq_ignore_ascii_case(known));
        let category = canonical.unwrap_or(args.category.as_str());
        let (first, second) = if canonical.is_some() {
            (EXCHANGE_ENDPOINT, ITEM_ENDPOINT)
        } else {
            (ITEM_ENDPOINT, EXCHANGE_ENDPOINT)
        };
        let url = Self::url(args.game, &args.league, category, first);
        match self.fetch_from(&url, args) {
            Err(err) if is_not_found(&err) => {
                let fallback = Self::url(args.game, &args.league, category, second);
                self.fetch_from(&fallback, args).map_err(|_| {
                    ToolError::Failed(format!(
                        "{err} (the other endpoint failed too) — {}",
                        category_hint(args)
                    ))
                })
            }
            other => other,
        }
    }

    fn fetch_from(&self, url: &str, args: &Args) -> Result<Value, ToolError> {
        let body = self.http.get(url).map_err(ToolError::Failed)?;
        let value: Value = serde_json::from_str(&body)
            .map_err(|err| ToolError::Failed(format!("unexpected response from {url}: {err}")))?;
        let lines = value["lines"].as_array().ok_or_else(|| {
            ToolError::Failed(format!(
                "no lines in response from {url} — {}",
                category_hint(args)
            ))
        })?;

        // Exchange lines carry slug ids; the response's `items[]` catalog
        // plus `core.items[]` (the anchor currencies live only there) map
        // them to proper names for both filtering and display.
        let catalog: HashMap<&str, &str> = value["core"]["items"]
            .as_array()
            .into_iter()
            .chain(value["items"].as_array())
            .flatten()
            .filter_map(|item| Some((item["id"].as_str()?, item["name"].as_str()?)))
            .collect();

        let name_filter = args.name.as_deref().map(str::to_lowercase);
        let matches: Vec<&Value> = lines
            .iter()
            .filter(|line| {
                name_filter
                    .as_deref()
                    .is_none_or(|needle| line_matches(line, &catalog, needle))
            })
            .collect();

        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let trimmed: Vec<Value> = matches
            .iter()
            .take(limit)
            .map(|line| trim_line(line, &catalog))
            .collect();

        let primary_currency = value["core"]["primary"]
            .as_str()
            .map(|id| catalog.get(id).copied().unwrap_or(id));
        let unit_clause = primary_currency
            .map(|currency| format!(" `value` is denominated in {currency};"))
            .unwrap_or_default();
        let note = format!(
            "prices from poe.ninja's economy snapshot (server-cached ~5 minutes);{unit_clause} \
             chaos/divine/exalted `_value` keys name their own unit; `max_volume_rate` is units \
             of that line's item per one `max_volume_currency`; `change_percent` is the \
             sparkline's percent change over poe.ninja's recent window"
        );

        let mut out = serde_json::Map::new();
        out.insert("league".to_owned(), json!(args.league));
        out.insert("category".to_owned(), json!(args.category));
        if let Some(currency) = primary_currency {
            out.insert("primary_currency".to_owned(), json!(currency));
        }
        out.insert("total_matches".to_owned(), json!(matches.len()));
        out.insert("returned".to_owned(), json!(trimmed.len()));
        out.insert("lines".to_owned(), json!(trimmed));
        out.insert("source".to_owned(), json!(url));
        out.insert("note".to_owned(), json!(note));
        Ok(Value::Object(out))
    }
}

const EXCHANGE_ENDPOINT: &str = "exchange/current/overview";
const ITEM_ENDPOINT: &str = "stash/current/item/overview";

/// Categories served by the exchange endpoint (canonical capitalization;
/// matched case-insensitively because models lowercase arguments).
const EXCHANGE_CATEGORIES: [&str; 3] = ["Currency", "Fragment", "Fragments"];

/// The `http status: NNN` form is a contract pinned by `exile-toolkit`
/// (`describe_get_error` and its test), not an accident of the HTTP
/// client's error formatting.
fn is_not_found(err: &ToolError) -> bool {
    matches!(err, ToolError::Failed(msg) if msg.contains("http status: 404"))
}

fn category_hint(args: &Args) -> String {
    format!(
        "the category `{}` may not exist for {}: poe1 uses singular item categories \
         (e.g. UniqueWeapon), poe2 plural (e.g. UniqueWeapons); exchange categories \
         are Currency and Fragment (poe1) / Fragments (poe2)",
        args.category, args.game
    )
}

/// Does a line match the name filter? Checked against the line's proper
/// name, its catalog-resolved name, and its raw slug id.
fn line_matches(line: &Value, catalog: &HashMap<&str, &str>, needle: &str) -> bool {
    let id = line["id"].as_str();
    line["name"]
        .as_str()
        .into_iter()
        .chain(id.and_then(|id| catalog.get(id).copied()))
        .chain(id)
        .any(|candidate| candidate.to_lowercase().contains(needle))
}

/// Reduce a poe.ninja line to what a model needs: identity + values.
/// Full lines carry icons, sparkline arrays, and full mod text — hundreds
/// of tokens of noise per entry.
fn trim_line(line: &Value, catalog: &HashMap<&str, &str>) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(name) = line["name"].as_str() {
        out.insert("name".to_owned(), json!(name));
    } else if let Some(id) = line["id"].as_str() {
        out.insert("id".to_owned(), json!(id));
        if let Some(name) = catalog.get(id) {
            out.insert("name".to_owned(), json!(name));
        }
    }
    if let Some(base) = line["baseType"].as_str() {
        out.insert("base_type".to_owned(), json!(base));
    }
    for (field, key) in [
        ("chaosValue", "chaos_value"),
        ("divineValue", "divine_value"),
        ("exaltedValue", "exalted_value"),
        ("primaryValue", "value"),
    ] {
        if let Some(number) = line[field].as_f64() {
            out.insert(key.to_owned(), json!(number));
        }
    }
    if let Some(currency) = line["maxVolumeCurrency"].as_str() {
        let display = catalog.get(currency).copied().unwrap_or(currency);
        out.insert("max_volume_currency".to_owned(), json!(display));
        if let Some(rate) = line["maxVolumeRate"].as_f64() {
            out.insert("max_volume_rate".to_owned(), json!(rate));
        }
    }
    for (field, key) in [
        ("listingCount", "listings"),
        ("count", "count"),
        ("links", "links"),
    ] {
        if let Some(number) = line[field].as_u64() {
            out.insert(key.to_owned(), json!(number));
        }
    }
    let change = &line["sparkLine"]["totalChange"];
    let change = if change.is_null() {
        &line["sparkline"]["totalChange"]
    } else {
        change
    };
    if let Some(number) = change.as_f64() {
        out.insert("change_percent".to_owned(), json!(number));
    }
    Value::Object(out)
}

impl Default for PriceTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for PriceTool {
    fn name(&self) -> &'static str {
        "price"
    }

    fn description(&self) -> &'static str {
        "Look up current market prices and economy data from poe.ninja. Requires the league \
         id — resolve it with the `league` tool first. Categories: `Currency`/`Fragment` \
         (Path of Exile 1) or `Currency`/`Fragments` (Path of Exile 2) for exchange rates; \
         item categories like `UniqueWeapon`, `UniqueArmour`, `UniqueAccessory`, \
         `UniqueFlask`, `UniqueJewel`, `DivinationCard`, `SkillGem` (Path of Exile 1, \
         singular) or `UniqueWeapons`, `UniqueArmours`, `UniqueAccessories`, `UniqueJewels` \
         (Path of Exile 2, plural). Filter by `name` for a specific item — it matches both \
         proper names (`Divine Orb`) and poe.ninja ids (`divine`). Cite the source and note \
         prices move constantly."
    }

    fn parameters_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"game":{"type":"string","enum":["poe1","poe2"],"description":"Which game"},"league":{"type":"string","description":"League id from the league tool (e.g. the current challenge league)"},"category":{"type":"string","description":"poe.ninja category: Currency/Fragment for exchange, or an item category (poe1 singular, poe2 plural)"},"name":{"type":"string","description":"Optional case-insensitive filter matched against item names and poe.ninja ids"},"limit":{"type":"integer","minimum":1,"maximum":20,"description":"Max lines returned (default 5)"}},"required":["game","league","category"],"additionalProperties":false}"#
    }

    fn execute(&self, args_json: &str) -> Result<String, ToolError> {
        let args: Args = serde_json::from_str(args_json)
            .map_err(|err| ToolError::InvalidArgs(err.to_string()))?;
        if args.league.trim().is_empty() {
            return Err(ToolError::InvalidArgs(
                "`league` must not be empty".to_owned(),
            ));
        }
        if args.category.trim().is_empty() {
            return Err(ToolError::InvalidArgs(
                "`category` must not be empty".to_owned(),
            ));
        }

        let mut result = serde_json::Map::new();
        result.insert("game".to_owned(), json!(args.game.as_str()));
        result.insert("fetched_at".to_owned(), json!(now_utc()));
        result.insert("prices".to_owned(), self.fetch(&args)?);
        serde_json::to_string(&Value::Object(result))
            .map_err(|err| ToolError::Failed(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exile_toolkit::testing::{FailHttp, FakeHttp};

    /// Trimmed live captures (2026-08-15). Exchange responses carry
    /// slug-id `lines[]`, a top-level `items[]` id→name catalog, and a
    /// `core` block whose `items[]` holds the anchor currencies (absent
    /// from the top-level catalog live) and whose `primary` names the
    /// denominating currency.
    const EXCHANGE_FIXTURE: &str = r#"{"core":{"items":[{"id":"chaos","name":"Chaos Orb","image":"x","category":"Currency","detailsId":"chaos-orb"},{"id":"divine","name":"Divine Orb","image":"x","category":"Currency","detailsId":"divine-orb"}],"rates":{"divine":0.005},"primary":"chaos","secondary":"divine"},"lines":[{"id":"accelerating-catalyst","primaryValue":1.62,"volumePrimaryValue":1565,"maxVolumeCurrency":"chaos","maxVolumeRate":0.6184,"sparkline":{"totalChange":-17.39,"data":[]}},{"id":"chaos","primaryValue":1.0,"volumePrimaryValue":23321273,"maxVolumeCurrency":"divine","maxVolumeRate":196.7,"sparkline":{"totalChange":9.54,"data":[]}}],"items":[{"id":"accelerating-catalyst","name":"Accelerating Catalyst","image":"x","category":"Currency","detailsId":"accelerating-catalyst"}]}"#;

    const ITEM_FIXTURE: &str = r#"{"lines":[{"id":45060,"name":"Replica Wings of Entropy","icon":"x","levelRequired":62,"baseType":"Ezomyte Axe","links":6,"itemClass":3,"chaosValue":19670.0,"divineValue":100.0,"exaltedValue":98357.5,"count":12,"listingCount":29,"sparkLine":{"totalChange":-3.1,"data":[]},"explicitModifiers":[{"text":"huge mod text","optional":false}],"flavourText":"long flavour"},{"id":2,"name":"Mageblood","icon":"x","baseType":"Heavy Belt","chaosValue":80000.0,"divineValue":406.7,"listingCount":54,"sparkLine":{"totalChange":1.2,"data":[]}}]}"#;

    fn tool(routes: Vec<(&'static str, &'static str)>) -> PriceTool {
        PriceTool::with_http(Box::new(FakeHttp { routes }))
    }

    fn parse(result: &str) -> Value {
        serde_json::from_str(result).expect("tool returns valid JSON")
    }

    #[test]
    fn exchange_categories_use_the_exchange_endpoint() {
        let tool = tool(vec![("exchange/current/overview", EXCHANGE_FIXTURE)]);
        let result = parse(
            &tool
                .execute(r#"{"game":"poe1","league":"Testleague","category":"Currency"}"#)
                .expect("executes"),
        );
        let prices = &result["prices"];
        assert!(
            prices["source"]
                .as_str()
                .expect("source")
                .contains("exchange")
        );
        // Slug ids are joined to proper names via the items[] catalog,
        // and core.primary names the unit `value` is denominated in.
        assert_eq!(prices["primary_currency"], "Chaos Orb");
        assert!(
            prices["note"]
                .as_str()
                .expect("note")
                .contains("denominated in Chaos Orb")
        );
        assert_eq!(prices["lines"][1]["id"], "chaos");
        assert_eq!(prices["lines"][1]["name"], "Chaos Orb");
        assert_eq!(prices["lines"][1]["value"], 1.0);
        assert_eq!(prices["lines"][1]["max_volume_currency"], "Divine Orb");
        assert_eq!(prices["lines"][1]["change_percent"], 9.54);
    }

    #[test]
    fn exchange_name_filter_matches_proper_names_and_slugs() {
        let tool = tool(vec![("exchange/current/overview", EXCHANGE_FIXTURE)]);
        for filter in ["Chaos Orb", "chaos"] {
            let result = parse(
                &tool
                    .execute(&format!(
                        r#"{{"game":"poe1","league":"Testleague","category":"Currency","name":"{filter}"}}"#
                    ))
                    .expect("executes"),
            );
            assert_eq!(
                result["prices"]["lines"][0]["id"], "chaos",
                "filter {filter} must match the chaos line"
            );
        }
        // A proper name that is not a substring of any slug id.
        let result = parse(
            &tool
                .execute(
                    r#"{"game":"poe1","league":"Testleague","category":"Currency","name":"Accelerating Catalyst"}"#,
                )
                .expect("executes"),
        );
        assert_eq!(result["prices"]["total_matches"], 1);
        assert_eq!(
            result["prices"]["lines"][0]["name"],
            "Accelerating Catalyst"
        );
    }

    #[test]
    fn lowercased_exchange_category_still_routes_to_exchange() {
        let tool = tool(vec![("exchange/current/overview", EXCHANGE_FIXTURE)]);
        let result = parse(
            &tool
                .execute(r#"{"game":"poe1","league":"Testleague","category":"currency"}"#)
                .expect("executes"),
        );
        let source = result["prices"]["source"].as_str().expect("source");
        assert!(source.contains("exchange"));
        assert!(source.contains("type=Currency"), "canonicalized: {source}");
    }

    /// Serves canned bodies by URL substring; unmatched URLs 404 the way
    /// the live API does for unknown categories.
    struct NotFoundHttp {
        routes: Vec<(&'static str, &'static str)>,
    }

    impl exile_toolkit::HttpGet for NotFoundHttp {
        fn get(&self, url: &str) -> Result<String, String> {
            self.routes
                .iter()
                .find(|(fragment, _)| url.contains(fragment))
                .map(|(_, body)| (*body).to_owned())
                .ok_or_else(|| format!("GET {url} failed: http status: 404"))
        }
    }

    #[test]
    fn misrouted_category_falls_back_to_the_other_endpoint() {
        // "Runes"-style category: routed to the item endpoint first, which
        // 404s; the exchange endpoint serves it.
        let tool = PriceTool::with_http(Box::new(NotFoundHttp {
            routes: vec![("exchange/current/overview", EXCHANGE_FIXTURE)],
        }));
        let result = parse(
            &tool
                .execute(r#"{"game":"poe2","league":"Testleague","category":"Runes"}"#)
                .expect("fallback resolves"),
        );
        assert!(
            result["prices"]["source"]
                .as_str()
                .expect("source")
                .contains("exchange")
        );
    }

    #[test]
    fn unknown_category_404_on_both_endpoints_gets_the_hint() {
        let tool = PriceTool::with_http(Box::new(NotFoundHttp { routes: vec![] }));
        let err = tool
            .execute(r#"{"game":"poe2","league":"Testleague","category":"Bogus"}"#)
            .expect_err("both endpoints 404");
        let message = err.to_string();
        assert!(message.contains("http status: 404"), "got: {message}");
        assert!(message.contains("plural"), "hint missing: {message}");
    }

    #[test]
    fn item_lookup_filters_by_name_and_trims_noise() {
        let tool = tool(vec![("stash/current/item/overview", ITEM_FIXTURE)]);
        let result = parse(
            &tool
                .execute(
                    r#"{"game":"poe1","league":"Testleague","category":"UniqueAccessory","name":"mageblood"}"#,
                )
                .expect("executes"),
        );
        let prices = &result["prices"];
        assert_eq!(prices["total_matches"], 1);
        let line = &prices["lines"][0];
        assert_eq!(line["name"], "Mageblood");
        assert_eq!(line["divine_value"], 406.7);
        assert_eq!(line["listings"], 54);
        assert!(line.get("icon").is_none(), "noise fields must be trimmed");
        assert!(line.get("explicitModifiers").is_none());
        assert!(result["fetched_at"].as_str().expect("stamp").contains('T'));
    }

    #[test]
    fn unfiltered_lookup_respects_limit_and_reports_totals() {
        let tool = tool(vec![("stash/current/item/overview", ITEM_FIXTURE)]);
        let result = parse(
            &tool
                .execute(
                    r#"{"game":"poe1","league":"Testleague","category":"UniqueWeapon","limit":1}"#,
                )
                .expect("executes"),
        );
        assert_eq!(result["prices"]["total_matches"], 2);
        assert_eq!(result["prices"]["returned"], 1);
    }

    #[test]
    fn unknown_category_is_a_helpful_failure() {
        let tool = tool(vec![("stash/current/item/overview", r#"{"error":"x"}"#)]);
        let err = tool
            .execute(r#"{"game":"poe2","league":"Testleague","category":"Bogus"}"#)
            .expect_err("no lines");
        assert!(err.to_string().contains("poe2 plural"));
    }

    #[test]
    fn bad_args_are_invalid_args() {
        let tool = tool(vec![]);
        for bad in [
            r#"{"game":"poe1","league":"L"}"#,
            r#"{"game":"poe1","category":"Currency"}"#,
            r#"{"game":"poe1","league":"  ","category":"Currency"}"#,
            r#"{"game":"poe1","league":"L","category":""}"#,
            r#"{"game":"poe3","league":"L","category":"Currency"}"#,
            r#"{"game":"poe1","league":"L","category":"Currency","bogus":1}"#,
        ] {
            assert!(
                matches!(tool.execute(bad), Err(ToolError::InvalidArgs(_))),
                "expected InvalidArgs for {bad}"
            );
        }
    }

    #[test]
    fn http_failure_is_tool_failure() {
        let tool = PriceTool::with_http(Box::new(FailHttp));
        assert!(matches!(
            tool.execute(r#"{"game":"poe1","league":"L","category":"Currency"}"#),
            Err(ToolError::Failed(_))
        ));
    }

    /// Manual check: `cargo test -p exile-ninja -- --ignored`.
    #[test]
    #[ignore = "hits live endpoints"]
    fn live_endpoints_respond() {
        use exile_tool_api::Tool as _;
        let league_tool = exile_league::LeagueTool::new();
        let tool = PriceTool::new();
        for (game, category) in [("poe1", "Currency"), ("poe2", "UniqueWeapons")] {
            let league_result: Value = serde_json::from_str(
                &league_tool
                    .execute(&format!(r#"{{"game":"{game}"}}"#))
                    .expect("league resolves"),
            )
            .expect("league json");
            let league = league_result["current"]["leagues"]
                .as_array()
                .expect("leagues")
                .iter()
                .find(|l| l["kind"] == "challenge" && l["hardcore"] == false)
                .and_then(|l| l["id"].as_str())
                .expect("challenge league")
                .to_owned();
            let result = parse(
                &tool
                    .execute(&format!(
                        r#"{{"game":"{game}","league":"{league}","category":"{category}"}}"#
                    ))
                    .expect("live fetch"),
            );
            assert!(
                result["prices"]["total_matches"].as_u64().expect("count") > 0,
                "{game}: no live lines"
            );
            if category == "Currency" {
                assert!(
                    result["prices"]["primary_currency"].is_string(),
                    "{game}: exchange response must name its denominating currency"
                );
                assert!(
                    result["prices"]["lines"][0]["name"].is_string(),
                    "{game}: exchange slugs must be joined to proper names"
                );
            }
        }
    }
}

//! Wiki retrieval — Tier-B written knowledge for both games.
//!
//! Talks to the community wikis' `MediaWiki` APIs: poewiki.net (Path of
//! Exile 1) and poe2wiki.net (Path of Exile 2), which are separate sites
//! for separate games (project law 3). Both sites front their APIs with
//! an anti-bot proxy that blocks browser-like user agents — the project's
//! descriptive UA passes, which is exactly what `exile-toolkit`'s client
//! sends.
//!
//! Two operations behind one tool: `search` (ranked page titles with
//! snippets) and `page` (the article's *rendered* HTML converted to
//! readable text — raw wikitext lacks the template-generated data —
//! truncated to a budget). Results carry `source` URLs and a `fetched_at`
//! stamp (project law 1).

mod text;

use std::path::PathBuf;
use std::time::Duration;

use exile_tool_api::{Tool, ToolError};
use exile_toolkit::{Game, HttpGet, UreqHttp, VintageCache, now_utc, percent_encode};
use serde::Deserialize;
use serde_json::Value;

/// How long a cached page stays fresh. Wiki articles change on patch
/// cadence, not minutes — a day balances rate courtesy against staleness
/// (Tier B: the vintage is always visible, and `refresh` forces live).
const PAGE_CACHE_TTL: Duration = Duration::from_hours(24);

/// Default article-text budget in characters; large articles are
/// truncated with an explicit marker in the result.
const DEFAULT_MAX_CHARS: usize = 8000;

/// Default number of search results.
const DEFAULT_SEARCH_LIMIT: u32 = 8;

/// Upper bound on search results per call.
const MAX_SEARCH_LIMIT: u32 = 20;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    game: Game,
    search: Option<String>,
    page: Option<String>,
    limit: Option<u32>,
    max_chars: Option<usize>,
    refresh: Option<bool>,
}

/// The `wiki` tool: search + article retrieval over the community wikis.
/// Page fetches go through a disk cache with vintage stamps (Tier B);
/// searches are always live.
pub struct WikiTool {
    cache: VintageCache,
}

impl WikiTool {
    /// Tool with the live HTTP client and the on-disk page cache
    /// (`EXILE_WIKI_CACHE_DIR`, default `.exile-cache/wiki` — gitignored).
    #[must_use]
    pub fn new() -> Self {
        let dir = std::env::var_os("EXILE_WIKI_CACHE_DIR")
            .map_or_else(|| PathBuf::from(".exile-cache/wiki"), PathBuf::from);
        Self::with_cache(Box::new(UreqHttp::new()), dir, PAGE_CACHE_TTL)
    }

    /// Tool with an injected HTTP implementation and effectively no
    /// caching (zero TTL) — the constructor unit tests use.
    #[must_use]
    pub fn with_http(http: Box<dyn HttpGet>) -> Self {
        Self::with_cache(
            http,
            std::env::temp_dir().join("exile-wiki-uncached"),
            Duration::ZERO,
        )
    }

    /// Tool with explicit cache location and TTL (cache-behavior tests).
    #[must_use]
    pub fn with_cache(http: Box<dyn HttpGet>, cache_dir: PathBuf, ttl: Duration) -> Self {
        Self {
            cache: VintageCache::new(http, cache_dir, ttl),
        }
    }

    fn site(game: Game) -> &'static str {
        match game {
            Game::Poe1 => "https://www.poewiki.net",
            Game::Poe2 => "https://www.poe2wiki.net",
        }
    }

    fn search(&self, game: Game, query: &str, limit: u32) -> Result<Value, ToolError> {
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT);
        let url = format!(
            "{}/w/api.php?action=query&list=search&format=json&formatversion=2&srlimit={limit}&srsearch={}",
            Self::site(game),
            percent_encode(query)
        );
        let body = self.cache.live(&url).map_err(ToolError::Failed)?;
        let value = parse_api_json(&body, &url)?;
        if let Some(error) = value.get("error") {
            let info = error["info"].as_str().unwrap_or("unknown wiki API error");
            return Err(ToolError::Failed(format!("wiki search failed: {info}")));
        }
        let results: Vec<Value> = value["query"]["search"]
            .as_array()
            .ok_or_else(|| ToolError::Failed(format!("no search results field from {url}")))?
            .iter()
            .map(|hit| {
                serde_json::json!({
                    "title": hit["title"],
                    "snippet": text::clean_snippet(hit["snippet"].as_str().unwrap_or_default()),
                    "words": hit["wordcount"],
                })
            })
            .collect();
        Ok(serde_json::json!({
            "query": query,
            "results": results,
            "source": url,
        }))
    }

    /// Fetch the *rendered* page (`prop=text`), not raw wikitext: the
    /// wikis' hard data is template-generated, so only the rendered HTML
    /// contains it.
    fn page(
        &self,
        game: Game,
        title: &str,
        max_chars: usize,
        refresh: bool,
    ) -> Result<Value, ToolError> {
        let url = format!(
            "{}/w/api.php?action=parse&format=json&formatversion=2&prop=text&redirects=1\
             &disablelimitreport=true&disableeditsection=true&disabletoc=true&page={}",
            Self::site(game),
            percent_encode(title)
        );
        let fetch = self.cache.get(&url, refresh).map_err(ToolError::Failed)?;
        let value = parse_api_json(&fetch.body, &url)?;
        if let Some(error) = value.get("error") {
            let info = error["info"].as_str().unwrap_or("unknown wiki API error");
            return Err(ToolError::Failed(format!(
                "wiki page fetch failed for `{title}`: {info}"
            )));
        }
        let html = value["parse"]["text"]
            .as_str()
            .ok_or_else(|| ToolError::Failed(format!("no page text in response from {url}")))?;
        let resolved_title = value["parse"]["title"].as_str().unwrap_or(title);

        let readable = text::html_to_text(html);
        let total_chars = readable.chars().count();
        let truncated = total_chars > max_chars;
        let text: String = if truncated {
            readable.chars().take(max_chars).collect()
        } else {
            readable
        };
        // Cache-vs-live is part of the citation: a cached article's
        // vintage is when it was retrieved, not when this call ran.
        let source = if fetch.from_cache {
            format!(
                "{url} (local cache, retrieved {}; pass refresh:true for live)",
                fetch.retrieved_at
            )
        } else {
            url
        };
        Ok(serde_json::json!({
            "title": resolved_title,
            "text": text,
            "truncated": truncated,
            "total_chars": total_chars,
            "retrieved_at": fetch.retrieved_at,
            "source": source,
        }))
    }
}

impl Default for WikiTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an API response body, distinguishing an HTML page (anti-bot
/// challenge or site error, served with status 200) from real JSON so the
/// model gets a diagnosable failure instead of a bare parse error.
fn parse_api_json(body: &str, url: &str) -> Result<Value, ToolError> {
    let trimmed = body.trim_start();
    if trimmed.starts_with('<') {
        return Err(ToolError::Failed(format!(
            "wiki returned an HTML page instead of JSON from {url} — likely an anti-bot \
             challenge or a site error; retry later"
        )));
    }
    serde_json::from_str(trimmed)
        .map_err(|err| ToolError::Failed(format!("unexpected response from {url}: {err}")))
}

impl Tool for WikiTool {
    fn name(&self) -> &'static str {
        "wiki"
    }

    fn description(&self) -> &'static str {
        "Look up game mechanics, items, and systems on the community wiki (poewiki.net for \
         Path of Exile 1, poe2wiki.net for Path of Exile 2). Search with SHORT keyword \
         queries (1-3 words, like article titles: 'resistance', 'Kitava'), or fetch a \
         page directly by its exact title. ALWAYS fetch the `page` before answering — \
         snippets only locate pages. Cite the source URL. Pages are cached locally for a \
         day and results state their vintage; pass `refresh` to force a live fetch."
    }

    fn parameters_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"game":{"type":"string","enum":["poe1","poe2"],"description":"Which game's wiki"},"search":{"type":"string","description":"Full-text search query (provide exactly one of search or page)"},"page":{"type":"string","description":"Exact page title to fetch (provide exactly one of search or page)"},"limit":{"type":"integer","minimum":1,"maximum":20,"description":"Max search results (default 8)"},"max_chars":{"type":"integer","description":"Article text budget in characters (default 8000)"},"refresh":{"type":"boolean","description":"Force a live re-fetch of a cached page"}},"required":["game"],"additionalProperties":false}"#
    }

    fn execute(&self, args_json: &str) -> Result<String, ToolError> {
        let args: Args = serde_json::from_str(args_json)
            .map_err(|err| ToolError::InvalidArgs(err.to_string()))?;

        let mut result = serde_json::Map::new();
        result.insert(
            "game".to_owned(),
            Value::String(args.game.as_str().to_owned()),
        );
        result.insert("fetched_at".to_owned(), Value::String(now_utc()));
        match (&args.search, &args.page) {
            (Some(query), None) => {
                let query = query.trim();
                if query.is_empty() {
                    return Err(ToolError::InvalidArgs(
                        "`search` must not be empty".to_owned(),
                    ));
                }
                let limit = args.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
                result.insert("search".to_owned(), self.search(args.game, query, limit)?);
            }
            (None, Some(title)) => {
                let title = title.trim();
                if title.is_empty() {
                    return Err(ToolError::InvalidArgs(
                        "`page` must not be empty".to_owned(),
                    ));
                }
                let max_chars = args.max_chars.unwrap_or(DEFAULT_MAX_CHARS).max(200);
                result.insert(
                    "page".to_owned(),
                    self.page(args.game, title, max_chars, args.refresh.unwrap_or(false))?,
                );
            }
            _ => {
                return Err(ToolError::InvalidArgs(
                    "provide exactly one of `search` or `page`".to_owned(),
                ));
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

    /// Shape of `action=query&list=search&formatversion=2` (fields the tool
    /// reads, as served by `MediaWiki` 1.40).
    const SEARCH_FIXTURE: &str = r#"{"batchcomplete":true,"query":{"searchinfo":{"totalhits":2},"search":[{"pageid":1,"ns":0,"title":"Maps","snippet":"the <span class=\"searchmatch\">map</span> system","wordcount":2000,"size":1,"timestamp":"2026-01-01T00:00:00Z"},{"pageid":2,"ns":0,"title":"Map fragments","snippet":"fragments","wordcount":300,"size":1,"timestamp":"2026-01-01T00:00:00Z"}]}}"#;

    /// Shape of `action=parse&formatversion=2&prop=text` — rendered HTML,
    /// including a template-generated table whose data must survive.
    const PAGE_FIXTURE: &str = r#"{"parse":{"title":"Maps","pageid":1,"text":"<div class=\"mw-parser-output\"><p><b>Maps</b> are <a href=\"/wiki/Atlas\">atlas</a> items.</p><table class=\"wikitable\"><tr><th>Tier</th><th>Level</th></tr><tr><td>1</td><td>68</td></tr></table><h2>Mechanics</h2><p>Running maps grants progress.</p><script>tracker()</script></div>"}}"#;

    const MISSING_FIXTURE: &str = r#"{"error":{"code":"missingtitle","info":"The page you specified doesn't exist.","docref":"x"}}"#;

    fn tool(routes: Vec<(&'static str, &'static str)>) -> WikiTool {
        WikiTool::with_http(Box::new(FakeHttp { routes }))
    }

    fn parse(result: &str) -> Value {
        serde_json::from_str(result).expect("tool returns valid JSON")
    }

    #[test]
    fn search_returns_cleaned_results_with_source() {
        let tool = tool(vec![("poewiki.net", SEARCH_FIXTURE)]);
        let result = parse(
            &tool
                .execute(r#"{"game":"poe1","search":"map system"}"#)
                .expect("executes"),
        );
        assert_eq!(result["game"], "poe1");
        assert!(result["fetched_at"].as_str().expect("stamp").contains('T'));
        let search = &result["search"];
        assert_eq!(search["results"][0]["title"], "Maps");
        assert_eq!(search["results"][0]["snippet"], "the map system");
        let source = search["source"].as_str().expect("source");
        assert!(source.contains("poewiki.net"));
        assert!(source.contains("map%20system"));
    }

    #[test]
    fn page_returns_readable_truncatable_text() {
        let tool = tool(vec![("poe2wiki.net", PAGE_FIXTURE)]);
        let result = parse(
            &tool
                .execute(r#"{"game":"poe2","page":"Maps"}"#)
                .expect("executes"),
        );
        let page = &result["page"];
        assert_eq!(page["title"], "Maps");
        let text = page["text"].as_str().expect("text");
        assert!(text.contains("Maps are atlas items."));
        assert!(text.contains("| Tier | Level"), "table data must survive");
        assert!(text.contains("| 1 | 68"));
        assert!(text.contains("Mechanics"));
        assert!(!text.contains("tracker()"), "scripts must not leak");
        assert_eq!(page["truncated"], false);
        let source = page["source"].as_str().expect("source");
        assert!(source.contains("poe2wiki.net"));
        assert!(source.contains("prop=text"), "must fetch rendered HTML");
    }

    #[test]
    fn page_truncation_respects_budget() {
        let tool = tool(vec![("poewiki.net", PAGE_FIXTURE)]);
        let result = parse(
            &tool
                .execute(r#"{"game":"poe1","page":"Maps","max_chars":200}"#)
                .expect("executes"),
        );
        // Budget is clamped to >= 200; fixture text is shorter than that.
        assert_eq!(result["page"]["truncated"], false);
    }

    #[test]
    fn games_route_to_their_own_wikis() {
        let tool = tool(vec![
            ("poewiki.net", SEARCH_FIXTURE),
            ("poe2wiki.net", SEARCH_FIXTURE),
        ]);
        for (game, host) in [("poe1", "poewiki.net"), ("poe2", "poe2wiki.net")] {
            let result = parse(
                &tool
                    .execute(&format!(r#"{{"game":"{game}","search":"x"}}"#))
                    .expect("executes"),
            );
            assert!(
                result["search"]["source"]
                    .as_str()
                    .expect("source")
                    .contains(host),
                "{game} must hit {host}"
            );
        }
    }

    #[test]
    fn pages_are_cached_with_vintage_and_refresh_forces_live() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counting {
            calls: Arc<AtomicUsize>,
        }
        impl exile_toolkit::HttpGet for Counting {
            fn get(&self, _url: &str) -> Result<String, String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(PAGE_FIXTURE.to_owned())
            }
        }

        let dir =
            std::env::temp_dir().join(format!("exile-wiki-test-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let calls = Arc::new(AtomicUsize::new(0));
        let tool = WikiTool::with_cache(
            Box::new(Counting {
                calls: Arc::clone(&calls),
            }),
            dir.clone(),
            std::time::Duration::from_mins(5),
        );

        let live = parse(
            &tool
                .execute(r#"{"game":"poe1","page":"Maps"}"#)
                .expect("live fetch"),
        );
        let live_source = live["page"]["source"].as_str().expect("source").to_owned();
        assert!(!live_source.contains("cache"), "first fetch is live");
        let vintage = live["page"]["retrieved_at"]
            .as_str()
            .expect("stamp")
            .to_owned();

        // Second fetch: served from cache, vintage preserved and visible.
        let cached = parse(
            &tool
                .execute(r#"{"game":"poe1","page":"Maps"}"#)
                .expect("cache hit"),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no second live fetch");
        let cached_source = cached["page"]["source"].as_str().expect("source");
        assert!(
            cached_source.contains("local cache"),
            "got: {cached_source}"
        );
        assert!(
            cached_source.contains(&vintage),
            "vintage visible in source"
        );
        assert_eq!(cached["page"]["retrieved_at"], vintage.as_str());

        // refresh bypasses the valid entry.
        let refreshed = parse(
            &tool
                .execute(r#"{"game":"poe1","page":"Maps","refresh":true}"#)
                .expect("forced live"),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "refresh hits upstream");
        assert!(
            !refreshed["page"]["source"]
                .as_str()
                .expect("source")
                .contains("cache")
        );

        // Searches are never cached: two calls, two upstream hits.
        let tool = WikiTool::with_cache(
            Box::new(FakeHttp {
                routes: vec![("poewiki.net", SEARCH_FIXTURE)],
            }),
            dir.clone(),
            std::time::Duration::from_mins(5),
        );
        tool.execute(r#"{"game":"poe1","search":"maps"}"#)
            .expect("searches");
        tool.execute(r#"{"game":"poe1","search":"maps"}"#)
            .expect("searches");
        // (FakeHttp cannot count, but a cached search would have written an
        // entry; assert the cache dir holds exactly the one page entry.)
        let entries = std::fs::read_dir(&dir).expect("cache dir").count();
        assert_eq!(entries, 1, "only the page fetch is cached");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_page_is_a_clear_failure() {
        let tool = tool(vec![("poewiki.net", MISSING_FIXTURE)]);
        let err = tool
            .execute(r#"{"game":"poe1","page":"Nope"}"#)
            .expect_err("missing page fails");
        assert!(err.to_string().contains("doesn't exist"));
    }

    #[test]
    fn html_body_is_a_diagnosable_failure() {
        let tool = tool(vec![(
            "poewiki.net",
            "<!DOCTYPE html><html>challenge</html>",
        )]);
        let err = tool
            .execute(r#"{"game":"poe1","search":"maps"}"#)
            .expect_err("html body fails");
        assert!(err.to_string().contains("HTML page instead of JSON"));
    }

    #[test]
    fn bad_args_are_invalid_args() {
        let tool = tool(vec![]);
        for bad in [
            r#"{"game":"poe1"}"#,
            r#"{"game":"poe1","search":"a","page":"b"}"#,
            r#"{"game":"poe1","search":"   "}"#,
            r#"{"game":"poe1","page":""}"#,
            r#"{"game":"poe3","search":"a"}"#,
            r#"{"game":"poe1","search":"a","bogus":1}"#,
            "not json",
        ] {
            assert!(
                matches!(tool.execute(bad), Err(ToolError::InvalidArgs(_))),
                "expected InvalidArgs for {bad}"
            );
        }
    }

    #[test]
    fn http_failure_is_tool_failure() {
        let tool = WikiTool::with_http(Box::new(FailHttp));
        let err = tool
            .execute(r#"{"game":"poe1","search":"maps"}"#)
            .expect_err("must fail");
        assert!(matches!(err, ToolError::Failed(_)));
    }

    #[test]
    fn parameters_schema_is_valid_json() {
        let tool = tool(vec![]);
        let schema: Value =
            serde_json::from_str(tool.parameters_schema()).expect("schema is valid JSON");
        assert_eq!(schema["properties"]["game"]["enum"][0], "poe1");
    }

    /// Manual check: `cargo test -p exile-wiki -- --ignored`.
    #[test]
    #[ignore = "hits live endpoints"]
    fn live_wikis_respond() {
        let tool = WikiTool::new();
        for game in ["poe1", "poe2"] {
            let result = parse(
                &tool
                    .execute(&format!(r#"{{"game":"{game}","search":"league"}}"#))
                    .expect("live search"),
            );
            let results = result["search"]["results"].as_array().expect("results");
            assert!(!results.is_empty(), "{game}: no live search results");
            let title = results[0]["title"].as_str().expect("title").to_owned();
            let page = parse(
                &tool
                    .execute(&format!(r#"{{"game":"{game}","page":"{title}"}}"#))
                    .expect("live page fetch"),
            );
            assert!(
                !page["page"]["text"].as_str().expect("text").is_empty(),
                "{game}: empty page text"
            );
        }
    }
}

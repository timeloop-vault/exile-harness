//! Live sources for currently active leagues, in fallback order.
//!
//! - Path of Exile 1: the GGG league API (`www.pathofexile.com/api/leagues`,
//!   no auth needed with a descriptive UA) → poe.ninja → the trade-site
//!   data endpoint. The GGG source is **authoritative**: `category.current`
//!   marks the challenge-league family, `rules` marks Hardcore/SSF/
//!   Ruthless, and `startAt` is included.
//! - Path of Exile 2: the GGG rich endpoint is OAuth-only, so poe.ninja →
//!   trade2 data endpoint, with **derived** annotation (permanent leagues
//!   recognized by their stable ids, hardcore variants by naming); the
//!   result says so, and notes that a time-limited event league would be
//!   indistinguishable from the challenge league in derived mode.

use exile_tool_api::ToolError;
use exile_toolkit::HttpGet;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Game;

/// Permanent league ids, stable in the GGG league API since 2013 (source:
/// `www.pathofexile.com/api/leagues` category structure — permanent
/// leagues carry `category.id == "Standard"` and no `current` flag).
/// Used only for *derived* annotation when a source returns bare names.
const PERMANENT_IDS: [&str; 4] = ["Standard", "Hardcore", "Solo Self-Found", "Hardcore SSF"];

const NOTE_DERIVED: &str = "league kinds derived from stable naming (source returned names only); \
     a time-limited event league, if one is running, is indistinguishable from the challenge league here";

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

/// A live source for current leagues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// GGG league API — authoritative annotation (Path of Exile 1 only;
    /// the Path of Exile 2 equivalent requires OAuth).
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

/// Fetch current leagues, walking the source chain until one succeeds.
pub(crate) fn fetch_current(http: &dyn HttpGet, game: Game) -> Result<Value, ToolError> {
    let mut errors = Vec::new();
    for source in Source::chain(game) {
        let url = source.url(game);
        let body = match http.get(&url) {
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
                    section.insert("annotation".to_owned(), Value::String("derived".to_owned()));
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

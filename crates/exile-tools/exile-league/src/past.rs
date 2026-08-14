//! Vendored datasets of concluded leagues.
//!
//! The sanctioned Tier-B pattern from CLAUDE.md: concluded/immutable
//! historical facts shipped inside the crate with provenance (`sources` +
//! `generated_at`) and tests proving every entry actually concluded before
//! the dataset was generated. Current league state never lives here — it
//! is resolved live (see `sources`).

use exile_tool_api::ToolError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Game;

const POE1_DATA: &str = include_str!("../data/poe1_leagues.json");
const POE2_DATA: &str = include_str!("../data/poe2_leagues.json");

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

/// Build the `past` result section from the vendored dataset.
pub(crate) fn section(game: Game) -> Result<Value, ToolError> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

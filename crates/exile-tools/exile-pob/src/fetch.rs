//! Engine checkout bootstrap: download a stock Path of Building snapshot
//! per game into the vendored (gitignored) root. Snapshots come as
//! GitHub archive tarballs at an explicit ref — no git dependency,
//! reproducible for a pinned tag/commit, and deliberately reusable later
//! for bundling engines into a distributable.
//!
//! The engines stay stock: this module only downloads and unpacks; it
//! never patches anything (sanctioned-exception rule in CLAUDE.md).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use exile_toolkit::{Game, USER_AGENT, now_utc};
use flate2::read::GzDecoder;

/// The upstream community engines, one per game.
#[must_use]
pub fn repo(game: Game) -> &'static str {
    match game {
        Game::Poe1 => "PathOfBuilding",
        Game::Poe2 => "PathOfBuilding-PoE2",
    }
}

/// Download and unpack the engine snapshot for `game` at `reference`
/// (branch, tag, or commit) into `<root>/<game>`. An existing non-empty
/// checkout is only replaced when `force` is set.
pub fn fetch(game: Game, reference: &str, root: &Path, force: bool) -> Result<PathBuf, String> {
    let target = root.join(game.as_str());
    if is_populated(&target) {
        if !force {
            return Err(format!(
                "{} already exists — pass --force to replace it",
                target.display()
            ));
        }
        std::fs::remove_dir_all(&target)
            .map_err(|err| format!("clearing {} failed: {err}", target.display()))?;
    }
    std::fs::create_dir_all(&target)
        .map_err(|err| format!("creating {} failed: {err}", target.display()))?;

    let url = format!(
        "https://codeload.github.com/PathOfBuildingCommunity/{}/tar.gz/{}",
        repo(game),
        percent_safe(reference)
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_mins(10)))
        .user_agent(USER_AGENT)
        .build()
        .into();
    let response = agent
        .get(&url)
        .call()
        .map_err(|err| format!("GET {url} failed: {err}"))?;
    let reader = response.into_body().into_reader();

    unpack(GzDecoder::new(reader), &target)?;

    let provenance = serde_json::json!({
        "repo": format!("PathOfBuildingCommunity/{}", repo(game)),
        "ref": reference,
        "fetched_at": now_utc(),
    });
    let pin = target.join(".exile-fetch.json");
    std::fs::write(&pin, provenance.to_string())
        .map_err(|err| format!("writing {} failed: {err}", pin.display()))?;
    Ok(target)
}

/// Unpack a tar stream into `target`, stripping the archive's top-level
/// `<repo>-<ref>/` directory.
fn unpack(reader: impl Read, target: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|err| format!("reading archive failed: {err}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|err| format!("reading archive entry failed: {err}"))?;
        let path = entry
            .path()
            .map_err(|err| format!("archive entry has a bad path: {err}"))?
            .into_owned();
        let Some(stripped) = strip_top_level(&path) else {
            continue; // the top-level directory entry itself
        };
        // Entry-by-entry unpack bypasses tar's whole-archive sanitizing,
        // so containment is enforced here: no `..`, no absolute paths.
        if !contained(&stripped) {
            return Err(format!(
                "archive entry escapes the target directory: {}",
                path.display()
            ));
        }
        entry
            .unpack(target.join(&stripped))
            .map_err(|err| format!("unpacking {} failed: {err}", stripped.display()))?;
    }
    Ok(())
}

/// Drop the first path component (`repo-ref/src/x.lua` → `src/x.lua`).
fn strip_top_level(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    components.next()?;
    let rest = components.as_path();
    if rest.as_os_str().is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

/// True when every component is a plain name — nothing that could step
/// outside the unpack target.
fn contained(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn is_populated(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// Refs go into a URL path segment; keep them to characters that cannot
/// change the request shape.
fn percent_safe(reference: &str) -> String {
    exile_toolkit::percent_encode(reference).replace("%2F", "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repos_are_per_game() {
        assert_eq!(repo(Game::Poe1), "PathOfBuilding");
        assert_eq!(repo(Game::Poe2), "PathOfBuilding-PoE2");
    }

    #[test]
    fn top_level_directory_is_stripped() {
        assert_eq!(
            strip_top_level(Path::new("PathOfBuilding-dev/src/Launch.lua")),
            Some(PathBuf::from("src/Launch.lua"))
        );
        assert_eq!(strip_top_level(Path::new("PathOfBuilding-dev/")), None);
        assert_eq!(strip_top_level(Path::new("PathOfBuilding-dev")), None);
    }

    #[test]
    fn traversal_paths_are_rejected() {
        assert!(contained(Path::new("src/Launch.lua")));
        assert!(!contained(Path::new("../evil")));
        assert!(!contained(Path::new("src/../../evil")));
        assert!(!contained(Path::new("/absolute/evil")));
    }

    #[test]
    fn refs_are_url_safe_but_keep_slashes() {
        assert_eq!(percent_safe("dev"), "dev");
        assert_eq!(percent_safe("refs/heads/dev"), "refs/heads/dev");
        assert_eq!(percent_safe("a b?c"), "a%20b%3Fc");
    }

    #[test]
    fn populated_targets_are_protected() {
        // The crate's own directory stands in for an existing checkout.
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(is_populated(here));
        assert!(!is_populated(&here.join("does-not-exist")));
    }
}

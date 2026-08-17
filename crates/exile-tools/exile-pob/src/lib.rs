//! Build calculations — the Path of Building engine as a tool, so build
//! math never goes through the model (project law 2).
//!
//! The tool drives stock, unmodified engine checkouts (Path of Exile 1
//! and 2 — law 3) headless over the JSON-lines adapter in `lua/`,
//! exactly the shape proven by the spike (`spikes/pob-headless/`,
//! issue #19). Engines are never committed: checkouts live gitignored
//! under `vendor/pob/<game>` (override with `EXILE_POB_ROOT`).
//!
//! Share codes are decoded to build XML on the Rust side ([`codes`]) —
//! headless engines stub their own `Inflate`/`Deflate`, so the code
//! transform cannot be delegated to them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use exile_tool_api::{Tool, ToolError};
use exile_toolkit::{Game, now_utc};
use serde::Deserialize;
use serde_json::{Value, json};

use bridge::{Engine, EngineFactory, LuaFactory};

pub mod bridge;
pub mod codes;
pub mod fetch;

/// The JSON-lines adapter, embedded so the binary is self-contained and
/// materialized to a temp file at first use (`LuaJIT` needs a real path).
const ADAPTER: &str = include_str!("../lua/adapter.lua");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    game: Game,
    code: Option<String>,
    xml: Option<String>,
    stats: Option<Vec<String>>,
    /// Not a `pob` argument — accepted here only so the pre-#37 call
    /// shape gets a redirect to `pob_whatif` instead of a bare serde
    /// unknown-field error.
    custom_mods: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WhatifArgs {
    game: Game,
    code: Option<String>,
    xml: Option<String>,
    custom_mods: Vec<String>,
    stats: Option<Vec<String>>,
}

/// Resolve the build XML from `code`/`xml` arguments (exactly one).
fn resolve_xml(code: Option<&str>, xml: Option<&str>) -> Result<String, ToolError> {
    match (code, xml) {
        (Some(code), None) => codes::decode(code).map_err(ToolError::InvalidArgs),
        (None, Some(xml)) if !xml.trim().is_empty() => Ok(xml.to_owned()),
        _ => Err(ToolError::InvalidArgs(
            "provide exactly one of `code` or `xml`".to_owned(),
        )),
    }
}

/// Strip XML comments (the engine's parser does the same before parsing,
/// so scanning raw text without it would let commented-out tags divert
/// the injection or the root check) and reject CDATA outright: genuine
/// engine exports never emit it, and string surgery inside it cannot be
/// made safe.
fn prepare_xml(xml: &str) -> Result<String, ToolError> {
    if xml.contains("<![CDATA[") {
        return Err(ToolError::InvalidArgs(
            "CDATA sections are not supported — export the build from Path of Building".to_owned(),
        ));
    }
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Each game's engine uses its own build-file root element and — observed
/// live — silently loads a *default build* when given the other's, so the
/// mismatch must fail loudly instead (law 3: never cross-feed).
fn check_game_xml(game: Game, xml: &str) -> Result<(), ToolError> {
    let poe2_root = find_open_tag(xml, "PathOfBuilding2", 0).is_some();
    let matches = match game {
        Game::Poe2 => poe2_root,
        Game::Poe1 => !poe2_root && find_open_tag(xml, "PathOfBuilding", 0).is_some(),
    };
    if matches {
        Ok(())
    } else {
        Err(ToolError::InvalidArgs(format!(
            "this build XML does not belong to {game}: poe1 builds use a <PathOfBuilding> \
             root, poe2 builds <PathOfBuilding2> — never feed one game's build to the other"
        )))
    }
}

/// Find the next `<name ...>` open tag at or after `from`, requiring the
/// byte after the name to terminate it (so `Config` never matches
/// `ConfigSet`, nor `PathOfBuilding` a `PathOfBuilding2` root). Returns
/// (tag start, position after the closing `>`, whether self-closed).
fn find_open_tag(xml: &str, name: &str, from: usize) -> Option<(usize, usize, bool)> {
    let needle = format!("<{name}");
    let mut search = from;
    while let Some(offset) = xml[search..].find(&needle) {
        let start = search + offset;
        let after_name = start + needle.len();
        if matches!(
            xml.as_bytes().get(after_name),
            Some(b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n')
        ) {
            let gt = xml[after_name..].find('>')? + after_name;
            return Some((start, gt + 1, xml[..=gt].ends_with("/>")));
        }
        search = after_name;
    }
    None
}

/// Find the next `</name>` end tag at or after `from`, tolerating the
/// whitespace before `>` the engine's parser tolerates. Returns the byte
/// position of the `<`.
fn find_end_tag(xml: &str, name: &str, from: usize) -> Option<usize> {
    let needle = format!("</{name}");
    let bytes = xml.as_bytes();
    let mut search = from;
    while let Some(offset) = xml[search..].find(&needle) {
        let start = search + offset;
        let mut cursor = start + needle.len();
        while matches!(bytes.get(cursor), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'>') {
            return Some(start);
        }
        search = start + needle.len();
    }
    None
}

fn splice(xml: &str, at: usize, payload: &str) -> String {
    let mut out = String::with_capacity(xml.len() + payload.len());
    out.push_str(&xml[..at]);
    out.push_str(payload);
    out.push_str(&xml[at..]);
    out
}

/// Apply hypothetical modifier lines to a build. The payload is
/// per-game: the poe1 engine stores custom modifiers as
/// `CustomModifierBlock` elements — an added block composes with any the
/// build already carries — while poe2 still uses the legacy `customMods`
/// input text. It is injected into EVERY `ConfigSet` so the build's
/// active set (whichever id) carries it; with no sets it becomes a
/// direct `Config` child (= set 1, the default active set), and with no
/// config section one is created. Injection happens on the Rust side so
/// the model never hand-edits XML (law 2).
fn inject_custom_mods(game: Game, xml: &str, mods: &[String]) -> Result<String, String> {
    if mods.is_empty() || mods.iter().all(|line| line.trim().is_empty()) {
        return Err("`custom_mods` must contain at least one modifier line".to_owned());
    }
    // Legacy customMods text cannot be composed with by string surgery:
    // on poe1 the engine discards one of the two carriers at load, on
    // poe2 the later input silently replaces the earlier. Fail loudly.
    if xml.contains("\"customMods\"") || xml.contains("'customMods'") {
        return Err(
            "this build already carries legacy customMods text — remove it (or re-save the \
             build in current Path of Building) before running a what-if"
                .to_owned(),
        );
    }
    // The engine's own XML parser knows only the five named entities.
    let escaped: Vec<String> = mods.iter().map(|line| xml_escape(line.trim())).collect();
    let payload = match game {
        Game::Poe1 => format!(
            "<CustomModifierBlock title=\"exile-whatif\" enabled=\"true\">{}</CustomModifierBlock>",
            escaped.join("\n")
        ),
        Game::Poe2 => format!(
            "<Input name=\"customMods\" string=\"{}\"/>",
            escaped.join("\n")
        ),
    };

    let (with_sets, sets) = inject_into_config_sets(xml, &payload)?;
    if sets > 0 {
        return Ok(with_sets);
    }
    if let Some(position) = find_end_tag(xml, "Config", 0) {
        return Ok(splice(xml, position, &payload));
    }
    if let Some((_, after, self_closed)) = find_open_tag(xml, "Config", 0) {
        if self_closed {
            // Expand <Config .../> in place, keeping its attributes.
            let mut out = String::with_capacity(xml.len() + payload.len() + 10);
            out.push_str(&xml[..after - 2]);
            out.push('>');
            out.push_str(&payload);
            out.push_str("</Config>");
            out.push_str(&xml[after..]);
            return Ok(out);
        }
        return Err("build XML has an unterminated Config section".to_owned());
    }
    for root in ["PathOfBuilding2", "PathOfBuilding"] {
        if let Some(position) = find_end_tag(xml, root, 0) {
            return Ok(splice(
                xml,
                position,
                &format!("<Config>{payload}</Config>"),
            ));
        }
    }
    Err("not a Path of Building build XML (no PathOfBuilding element)".to_owned())
}

/// Insert `payload` at the end of every `<ConfigSet>` element, expanding
/// self-closed ones. Returns the new XML and the number of sets hit.
fn inject_into_config_sets(xml: &str, payload: &str) -> Result<(String, usize), String> {
    let mut out = String::with_capacity(xml.len() + payload.len() * 2);
    let mut cursor = 0;
    let mut count = 0;
    while let Some((_, after_open, self_closed)) = find_open_tag(xml, "ConfigSet", cursor) {
        if self_closed {
            out.push_str(&xml[cursor..after_open - 2]);
            out.push('>');
            out.push_str(payload);
            out.push_str("</ConfigSet>");
            cursor = after_open;
        } else if let Some(end) = find_end_tag(xml, "ConfigSet", after_open) {
            out.push_str(&xml[cursor..end]);
            out.push_str(payload);
            cursor = end;
        } else {
            return Err("build XML has an unterminated ConfigSet section".to_owned());
        }
        count += 1;
    }
    out.push_str(&xml[cursor..]);
    Ok((out, count))
}

fn xml_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

/// Shared engine supervisor: one warm headless child per game, used by
/// every pob-family tool so a registry holding several of them still
/// boots each engine at most once (~1GB RAM and seconds of startup per
/// engine). Tools share it via [`std::sync::Arc`].
pub struct EngineHost {
    factory: Box<dyn EngineFactory>,
    root: PathBuf,
    engines: Mutex<EngineSlots>,
}

#[derive(Default)]
struct EngineSlots {
    poe1: Option<Box<dyn Engine>>,
    poe2: Option<Box<dyn Engine>>,
}

impl EngineSlots {
    fn slot(&mut self, game: Game) -> &mut Option<Box<dyn Engine>> {
        match game {
            Game::Poe1 => &mut self.poe1,
            Game::Poe2 => &mut self.poe2,
        }
    }
}

/// One engine run: the version string and the requested stats.
struct Computed {
    engine_version: Value,
    stats: Value,
}

impl EngineHost {
    /// Host with the live `LuaJIT` factory. Engine checkouts are expected
    /// under `EXILE_POB_ROOT` (default `vendor/pob`); the interpreter is
    /// found via `EXILE_LUAJIT`, then `PATH`, then the standard winget
    /// install location on Windows. Construction never fails — a missing
    /// checkout or interpreter surfaces as a tool failure with guidance.
    #[must_use]
    pub fn new() -> Self {
        let root = std::env::var_os("EXILE_POB_ROOT")
            .map_or_else(|| PathBuf::from("vendor/pob"), PathBuf::from);
        let luajit = find_luajit();
        let adapter = materialize_adapter();
        Self::with_factory(Box::new(LuaFactory::new(luajit, adapter)), root)
    }

    /// Host with an injected engine factory (tests).
    #[must_use]
    pub fn with_factory(factory: Box<dyn EngineFactory>, root: PathBuf) -> Self {
        Self {
            factory,
            root,
            engines: Mutex::new(EngineSlots::default()),
        }
    }

    fn compute(
        &self,
        game: Game,
        xml: &str,
        stats: Option<&[String]>,
    ) -> Result<Computed, ToolError> {
        let mut slots = self.engines.lock().expect("engine lock");
        let slot = slots.slot(game);
        if slot.is_none() {
            let dir = self.root.join(game.as_str());
            *slot = Some(self.factory.spawn(&dir).map_err(ToolError::Failed)?);
        }
        let engine = slot.as_mut().expect("engine just ensured");

        // Any protocol failure poisons the warm child: kill it (drop) so
        // the next call boots a fresh one instead of talking to a
        // desynced process.
        let result = Self::converse(engine.as_mut(), xml, stats);
        if result.is_err() {
            *slot = None;
        }
        result
    }

    fn converse(
        engine: &mut dyn Engine,
        xml: &str,
        stats: Option<&[String]>,
    ) -> Result<Computed, ToolError> {
        let version = exchange(engine, &json!({"id": 1, "cmd": "version"}))?;
        exchange(
            engine,
            &json!({"id": 2, "cmd": "loadXML", "xml": xml, "name": "exile"}),
        )?;
        let mut stats_request = json!({"id": 3, "cmd": "stats"});
        if let Some(keys) = stats {
            stats_request["keys"] = json!(keys);
        }
        let stats = exchange(engine, &stats_request)?;
        Ok(Computed {
            engine_version: version["engine"].clone(),
            stats,
        })
    }
}

impl Default for EngineHost {
    fn default() -> Self {
        Self::new()
    }
}

/// The `pob` tool: build stats computed by the Path of Building engine.
pub struct PobTool {
    host: Arc<EngineHost>,
}

impl PobTool {
    /// Tool with its own live engine host. Prefer [`Self::with_host`] when
    /// registering several pob-family tools, so they share warm engines.
    #[must_use]
    pub fn new() -> Self {
        Self::with_host(Arc::new(EngineHost::new()))
    }

    /// Tool over a shared engine host.
    #[must_use]
    pub fn with_host(host: Arc<EngineHost>) -> Self {
        Self { host }
    }
}

/// The `pob_whatif` tool: the engine runs a build with and without
/// hypothetical modifier lines and returns each stat's before/after/delta
/// — the whole comparison happens tool-side (ADR: workflow-shaped tools;
/// law 2: nothing is left for the model to compute).
pub struct PobWhatifTool {
    host: Arc<EngineHost>,
}

impl PobWhatifTool {
    /// Tool with its own live engine host. Prefer [`Self::with_host`] when
    /// registering several pob-family tools, so they share warm engines.
    #[must_use]
    pub fn new() -> Self {
        Self::with_host(Arc::new(EngineHost::new()))
    }

    /// Tool over a shared engine host.
    #[must_use]
    pub fn with_host(host: Arc<EngineHost>) -> Self {
        Self { host }
    }
}

/// Per-stat comparison between two engine runs, over the union of their
/// keys. Numeric pairs get before/after/delta — the subtraction is
/// deterministic tool code, never model arithmetic, and the delta is
/// rounded to two decimals so neither the model nor the eval ever sees
/// f64 subtraction noise. Values the engine reported as non-numeric
/// ("inf"/"nan" strings, booleans) or one-sided are passed through with
/// a null delta instead of being silently dropped.
fn diff_stats(before: &Value, after: &Value) -> Value {
    let empty = serde_json::Map::new();
    let base = before.as_object().unwrap_or(&empty);
    let modified = after.as_object().unwrap_or(&empty);
    let mut out = serde_json::Map::new();
    let keys = base
        .keys()
        .chain(modified.keys().filter(|key| !base.contains_key(*key)));
    for key in keys {
        let b = base.get(key);
        let a = modified.get(key);
        let entry = match (b.and_then(Value::as_f64), a.and_then(Value::as_f64)) {
            (Some(x), Some(y)) => json!({"before": x, "after": y, "delta": round2(y - x)}),
            _ => json!({
                "before": b.cloned().unwrap_or(Value::Null),
                "after": a.cloned().unwrap_or(Value::Null),
                "delta": Value::Null,
            }),
        };
        out.insert(key.clone(), entry);
    }
    Value::Object(out)
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// One request/response over the adapter protocol, unwrapping the
/// response envelope.
fn exchange(engine: &mut dyn Engine, request: &Value) -> Result<Value, ToolError> {
    let line = engine
        .request(&request.to_string())
        .map_err(ToolError::Failed)?;
    let response: Value = serde_json::from_str(&line)
        .map_err(|err| ToolError::Failed(format!("engine spoke non-JSON: {err}: {line}")))?;
    if response["ok"] == json!(true) {
        Ok(response["result"].clone())
    } else {
        Err(ToolError::Failed(format!(
            "engine rejected {}: {}",
            request["cmd"],
            response["error"].as_str().unwrap_or(&line)
        )))
    }
}

fn find_luajit() -> PathBuf {
    if let Some(path) = std::env::var_os("EXILE_LUAJIT") {
        return PathBuf::from(path);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in ["luajit.exe", "luajit"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    if cfg!(windows)
        && let Some(local) = std::env::var_os("LOCALAPPDATA")
    {
        let winget = PathBuf::from(local).join("Programs/LuaJIT/bin/luajit.exe");
        if winget.is_file() {
            return winget;
        }
    }
    // Let the spawn fail with a path-shaped error the user can act on.
    PathBuf::from("luajit")
}

/// Write the embedded adapter beside the temp dir under a
/// content-derived name, so concurrent tools and upgraded binaries never
/// fight over one file.
fn materialize_adapter() -> PathBuf {
    let checksum: u32 = ADAPTER.bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(byte.into())
    });
    let path = std::env::temp_dir().join(format!("exile-pob-adapter-{checksum:08x}.lua"));
    if !path.is_file() {
        // Best effort: a failed write surfaces at spawn as a clear error.
        let _ = std::fs::write(&path, ADAPTER);
    }
    path
}

impl Default for PobTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for PobWhatifTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for PobTool {
    fn name(&self) -> &'static str {
        "pob"
    }

    fn description(&self) -> &'static str {
        "Read a build's current statistics (DPS, life, energy shield, effective HP, \
         resistances) computed by the Path of Building engine. Input is a Path of Building \
         share code (`code`, the base64 export used by pobb.in/pastebin) or raw build XML \
         (`xml`); optionally list `stats` keys to read specific values. For hypothetical \
         changes (\"how much would X give me?\") call `pob_whatif` instead."
    }

    fn parameters_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"game":{"type":"string","enum":["poe1","poe2"],"description":"Which game's engine"},"code":{"type":"string","description":"Path of Building share code (url-safe base64 export)"},"xml":{"type":"string","description":"Raw Path of Building build XML (alternative to code)"},"stats":{"type":"array","items":{"type":"string"},"description":"Optional stat keys to read (default: a core set incl. Life, EnergyShield, TotalEHP, TotalDPS, CombinedDPS)"}},"required":["game"],"additionalProperties":false}"#
    }

    fn execute(&self, args_json: &str) -> Result<String, ToolError> {
        let args: Args = serde_json::from_str(args_json)
            .map_err(|err| ToolError::InvalidArgs(err.to_string()))?;
        if args.custom_mods.is_some() {
            return Err(ToolError::InvalidArgs(
                "`custom_mods` is not a `pob` argument — call `pob_whatif` to measure \
                 hypothetical modifier changes"
                    .to_owned(),
            ));
        }
        let xml = resolve_xml(args.code.as_deref(), args.xml.as_deref())?;
        let xml = prepare_xml(&xml)?;
        check_game_xml(args.game, &xml)?;
        let computed = self.host.compute(args.game, &xml, args.stats.as_deref())?;

        let mut result = serde_json::Map::new();
        result.insert("game".to_owned(), json!(args.game.as_str()));
        result.insert("fetched_at".to_owned(), json!(now_utc()));
        result.insert(
            "build".to_owned(),
            json!({
                "engine": "Path of Building (headless, stock checkout)",
                "engine_version": computed.engine_version,
                "stats": computed.stats,
                "note": "computed by the Path of Building engine, not estimated; numbers \
                         reflect the build's own configuration flags. Cite the engine version.",
            }),
        );
        serde_json::to_string(&Value::Object(result))
            .map_err(|err| ToolError::Failed(err.to_string()))
    }
}

impl Tool for PobWhatifTool {
    fn name(&self) -> &'static str {
        "pob_whatif"
    }

    fn description(&self) -> &'static str {
        "Measure what hypothetical modifier lines would change on a build: the Path of \
         Building engine runs the build with and without `custom_mods` and returns each \
         stat's before/after/delta, precomputed. Use this for every \"how much would X \
         give me?\" question. Needs the build as a share code (`code`) or build XML \
         (`xml`) — ask the user for theirs when none is at hand. The hypothetical lines \
         compose with the build's existing custom modifier blocks; builds carrying \
         legacy customMods text are rejected."
    }

    fn parameters_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"game":{"type":"string","enum":["poe1","poe2"],"description":"Which game's engine"},"code":{"type":"string","description":"Path of Building share code (url-safe base64 export)"},"xml":{"type":"string","description":"Raw Path of Building build XML (alternative to code)"},"custom_mods":{"type":"array","items":{"type":"string"},"description":"Hypothetical modifier lines to apply (e.g. '30% more spell damage')"},"stats":{"type":"array","items":{"type":"string"},"description":"Optional stat keys to compare (default: a core set incl. Life, EnergyShield, TotalEHP, TotalDPS, CombinedDPS)"}},"required":["game","custom_mods"],"additionalProperties":false}"#
    }

    fn execute(&self, args_json: &str) -> Result<String, ToolError> {
        let args: WhatifArgs = serde_json::from_str(args_json)
            .map_err(|err| ToolError::InvalidArgs(err.to_string()))?;
        let xml = resolve_xml(args.code.as_deref(), args.xml.as_deref())?;
        let xml = prepare_xml(&xml)?;
        check_game_xml(args.game, &xml)?;
        let modified = inject_custom_mods(args.game, &xml, &args.custom_mods)
            .map_err(ToolError::InvalidArgs)?;

        let baseline = self.host.compute(args.game, &xml, args.stats.as_deref())?;
        let changed = self
            .host
            .compute(args.game, &modified, args.stats.as_deref())?;
        let stats = diff_stats(&baseline.stats, &changed.stats);
        if let Some(requested) = &args.stats {
            let missing: Vec<&str> = requested
                .iter()
                .map(String::as_str)
                .filter(|key| stats.get(key).is_none())
                .collect();
            if !missing.is_empty() {
                return Err(ToolError::InvalidArgs(format!(
                    "unknown stat keys (the engine reported no such stats for this build): {}",
                    missing.join(", ")
                )));
            }
        }

        let mut result = serde_json::Map::new();
        result.insert("game".to_owned(), json!(args.game.as_str()));
        result.insert("fetched_at".to_owned(), json!(now_utc()));
        result.insert(
            "whatif".to_owned(),
            json!({
                "engine": "Path of Building (headless, stock checkout)",
                "engine_version": changed.engine_version,
                "custom_mods": args.custom_mods,
                "stats": stats,
                "note": "both runs computed by the Path of Building engine on the same \
                         build; `delta` = after − before, precomputed by the tool (null \
                         delta = the engine reported a non-numeric value). Cite the \
                         engine version.",
            }),
        );
        serde_json::to_string(&Value::Object(result))
            .map_err(|err| ToolError::Failed(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Scripted engine: hands out canned response lines in order and
    /// records what it was asked.
    struct FakeEngine {
        responses: Vec<String>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Engine for FakeEngine {
        fn request(&mut self, line: &str) -> Result<String, String> {
            self.requests
                .lock()
                .expect("requests")
                .push(line.to_owned());
            if self.responses.is_empty() {
                Err("engine exited".to_owned())
            } else {
                Ok(self.responses.remove(0))
            }
        }
    }

    struct FakeFactory {
        spawns: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
        script: Vec<String>,
    }

    impl EngineFactory for FakeFactory {
        fn spawn(&self, _dir: &std::path::Path) -> Result<Box<dyn Engine>, String> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeEngine {
                responses: self.script.clone(),
                requests: Arc::clone(&self.requests),
            }))
        }
    }

    fn happy_script() -> Vec<String> {
        vec![
            r#"{"id":1,"ok":true,"result":{"engine":"9.99.9"}}"#.to_owned(),
            r#"{"id":2,"ok":true,"result":{"loaded":true}}"#.to_owned(),
            r#"{"id":3,"ok":true,"result":{"Life":6728,"CombinedDPS":975099.7}}"#.to_owned(),
        ]
    }

    fn rig(script: Vec<String>) -> (Arc<EngineHost>, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let spawns = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let factory = FakeFactory {
            spawns: Arc::clone(&spawns),
            requests: Arc::clone(&requests),
            script,
        };
        (
            Arc::new(EngineHost::with_factory(
                Box::new(factory),
                PathBuf::from("vendor/pob"),
            )),
            spawns,
            requests,
        )
    }

    fn tool(script: Vec<String>) -> (PobTool, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let (host, spawns, requests) = rig(script);
        (PobTool::with_host(host), spawns, requests)
    }

    /// Six responses: version/load/stats twice — one whatif comparison.
    fn whatif_script() -> Vec<String> {
        vec![
            r#"{"id":1,"ok":true,"result":{"engine":"9.99.9"}}"#.to_owned(),
            r#"{"id":2,"ok":true,"result":{"loaded":true}}"#.to_owned(),
            r#"{"id":3,"ok":true,"result":{"Life":60,"Mana":50}}"#.to_owned(),
            r#"{"id":1,"ok":true,"result":{"engine":"9.99.9"}}"#.to_owned(),
            r#"{"id":2,"ok":true,"result":{"loaded":true}}"#.to_owned(),
            r#"{"id":3,"ok":true,"result":{"Life":72,"Mana":50}}"#.to_owned(),
        ]
    }

    #[test]
    fn code_is_decoded_and_stats_are_stamped() {
        let (tool, _, requests) = tool(happy_script());
        let xml = "<PathOfBuilding><Build/></PathOfBuilding>";
        let code = codes::encode(xml).expect("encodes");
        let result = tool
            .execute(&json!({"game": "poe1", "code": code}).to_string())
            .expect("executes");
        let value: Value = serde_json::from_str(&result).expect("valid JSON");
        assert_eq!(value["game"], "poe1");
        assert_eq!(value["build"]["engine_version"], "9.99.9");
        assert_eq!(value["build"]["stats"]["Life"], 6728);
        assert!(value["fetched_at"].as_str().expect("stamp").contains('T'));

        let seen = requests.lock().expect("requests");
        assert!(seen[0].contains("\"version\""));
        // The engine must receive the DECODED XML, not the share code.
        assert!(seen[1].contains("<PathOfBuilding>"), "got: {}", seen[1]);
        assert!(seen[2].contains("\"stats\""));
    }

    #[test]
    fn explicit_stat_keys_are_forwarded() {
        let (tool, _, requests) = tool(happy_script());
        tool.execute(
            &json!({"game": "poe1", "xml": "<PathOfBuilding/>", "stats": ["Life"]}).to_string(),
        )
        .expect("executes");
        let seen = requests.lock().expect("requests");
        assert!(seen[2].contains(r#""keys":["Life"]"#), "got: {}", seen[2]);
    }

    #[test]
    fn engine_failure_is_reported_and_child_is_replaced() {
        let (tool, spawns, _) = tool(vec![r#"{"id":1,"ok":false,"error":"boom"}"#.to_owned()]);
        let request = json!({"game": "poe2", "xml": "<PathOfBuilding2/>"}).to_string();
        let err = tool.execute(&request).expect_err("engine failed");
        assert!(err.to_string().contains("boom"));
        // The dead child is discarded; the next call boots a fresh one.
        let _ = tool.execute(&request);
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn bad_args_are_invalid_args() {
        let (tool, spawns, _) = tool(happy_script());
        for bad in [
            r#"{"game":"poe1"}"#.to_owned(),
            r#"{"game":"poe1","xml":"  "}"#.to_owned(),
            json!({"game":"poe1","code":"abc","xml":"<x/>"}).to_string(),
            r#"{"game":"poe3","xml":"<x/>"}"#.to_owned(),
            r#"{"game":"poe1","xml":"<x/>","bogus":1}"#.to_owned(),
            r#"{"game":"poe1","code":"!!not a code!!"}"#.to_owned(),
            // custom_mods moved to pob_whatif; the old shape must fail
            // loudly, not be silently ignored.
            json!({"game":"poe1","xml":"<x/>","custom_mods":["10% more damage"]}).to_string(),
        ] {
            assert!(
                matches!(tool.execute(&bad), Err(ToolError::InvalidArgs(_))),
                "expected InvalidArgs for {bad}"
            );
        }
        // Arg validation must never boot an engine.
        assert_eq!(spawns.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn whatif_runs_both_variants_and_returns_the_diff() {
        let (host, spawns, requests) = rig(whatif_script());
        let tool = PobWhatifTool::with_host(host);
        let result = tool
            .execute(
                &json!({
                    "game": "poe1",
                    "xml": "<PathOfBuilding><Config></Config></PathOfBuilding>",
                    "custom_mods": ["30% more spell damage", "120% increased spell damage"],
                    "stats": ["Life", "Mana"],
                })
                .to_string(),
            )
            .expect("executes");
        let value: Value = serde_json::from_str(&result).expect("valid JSON");

        // Both engine runs happened on ONE warm child.
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        let seen = requests.lock().expect("requests");
        assert_eq!(seen.len(), 6, "version/load/stats twice");
        assert!(
            !seen[1].contains("customMods"),
            "baseline must be unmodified: {}",
            seen[1]
        );
        assert!(
            seen[4].contains(r"30% more spell damage\n120% increased spell damage"),
            "modified run carries the injected customMods input: {}",
            seen[4]
        );

        // The diff is precomputed: before/after/delta per stat.
        let life = &value["whatif"]["stats"]["Life"];
        assert_eq!(life["before"], 60.0);
        assert_eq!(life["after"], 72.0);
        assert_eq!(life["delta"], 12.0);
        assert_eq!(value["whatif"]["stats"]["Mana"]["delta"], 0.0);
        assert_eq!(value["whatif"]["engine_version"], "9.99.9");
        assert_eq!(value["whatif"]["custom_mods"][0], "30% more spell damage");
    }

    #[test]
    fn whatif_rejects_missing_or_empty_mods_without_booting() {
        let (host, spawns, _) = rig(whatif_script());
        let tool = PobWhatifTool::with_host(host);
        for bad in [
            json!({"game":"poe1","xml":"<PathOfBuilding/>"}).to_string(),
            json!({"game":"poe1","xml":"<PathOfBuilding/>","custom_mods":[]}).to_string(),
            json!({"game":"poe1","xml":"<PathOfBuilding/>","custom_mods":["  "]}).to_string(),
            json!({"game":"poe1","custom_mods":["10% more damage"]}).to_string(),
        ] {
            assert!(
                matches!(tool.execute(&bad), Err(ToolError::InvalidArgs(_))),
                "expected InvalidArgs for {bad}"
            );
        }
        assert_eq!(spawns.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pob_family_shares_one_warm_engine_per_game() {
        // pob call (3 responses) then a whatif (6) against the same host:
        // the fake engine serves one script, so a shared child means one
        // spawn for all nine exchanges.
        let mut script = happy_script();
        script.extend(whatif_script());
        let (host, spawns, _) = rig(script);
        let pob = PobTool::with_host(Arc::clone(&host));
        let whatif = PobWhatifTool::with_host(host);

        pob.execute(&json!({"game":"poe1","xml":"<PathOfBuilding/>"}).to_string())
            .expect("pob executes");
        whatif
            .execute(
                &json!({"game":"poe1","xml":"<PathOfBuilding></PathOfBuilding>","custom_mods":["10% more damage"]})
                    .to_string(),
            )
            .expect("whatif executes");
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "one shared warm child");
    }

    #[test]
    fn custom_mods_create_a_config_section_when_missing() {
        // poe1 payload: a native CustomModifierBlock (composes with any
        // the build already has).
        let poe1 = inject_custom_mods(
            Game::Poe1,
            "<PathOfBuilding><Build/></PathOfBuilding>",
            &["10% more damage".to_owned()],
        )
        .expect("injects");
        assert!(poe1.contains(
            "<Config><CustomModifierBlock title=\"exile-whatif\" enabled=\"true\">10% more damage</CustomModifierBlock></Config>"
        ));
        assert!(poe1.ends_with("</Config></PathOfBuilding>"));

        // poe2 payload: the legacy customMods input, under its own root.
        let poe2 = inject_custom_mods(
            Game::Poe2,
            "<PathOfBuilding2><Build/></PathOfBuilding2>",
            &["10% more damage".to_owned()],
        )
        .expect("injects");
        assert!(poe2.contains("<Input name=\"customMods\" string=\"10% more damage\"/>"));
        assert!(poe2.ends_with("</Config></PathOfBuilding2>"));
    }

    #[test]
    fn custom_mods_land_in_every_config_set() {
        // Multi-set build with activeConfigSet=2: whichever set is
        // active must carry the hypothetical, so every set gets it.
        let nested = inject_custom_mods(
            Game::Poe1,
            "<PathOfBuilding><Config activeConfigSet=\"2\"><ConfigSet id=\"1\"><Input name=\"enemyLevel\" number=\"1\"/></ConfigSet><ConfigSet id=\"2\"/></Config></PathOfBuilding>",
            &["10% more damage".to_owned()],
        )
        .expect("injects");
        assert_eq!(
            nested
                .matches("CustomModifierBlock title=\"exile-whatif\"")
                .count(),
            2,
            "both sets get the block: {nested}"
        );
        // The self-closed set 2 was expanded in place, keeping its id.
        assert!(
            nested.contains("<ConfigSet id=\"2\"><CustomModifierBlock"),
            "got: {nested}"
        );
        // Set 1's block sits inside the set, after its existing input.
        let set1_input = nested.find("enemyLevel").expect("existing input");
        let set1_block = nested.find("CustomModifierBlock").expect("our block");
        assert!(set1_block > set1_input);

        // A modern poe1 build with an existing block: ours composes.
        let composed = inject_custom_mods(
            Game::Poe1,
            "<PathOfBuilding><Config><ConfigSet id=\"1\"><CustomModifierBlock title=\"Default\" enabled=\"true\">5% more damage</CustomModifierBlock></ConfigSet></Config></PathOfBuilding>",
            &["10% more damage".to_owned()],
        )
        .expect("existing blocks compose");
        assert_eq!(composed.matches("CustomModifierBlock").count(), 4); // 2 tags each
    }

    #[test]
    fn custom_mods_handle_self_closed_config_and_end_tag_whitespace() {
        // Self-closed config with attributes is expanded in place.
        let expanded = inject_custom_mods(
            Game::Poe2,
            "<PathOfBuilding2><Config activeConfigSet=\"1\"/></PathOfBuilding2>",
            &["10% more damage".to_owned()],
        )
        .expect("injects");
        assert!(
            expanded.contains("<Config activeConfigSet=\"1\"><Input name=\"customMods\""),
            "got: {expanded}"
        );

        // The engine parser tolerates `</Config >`; so must the scan --
        // a missed end tag would append a SECOND config section whose
        // loader wipes the first.
        let tolerant = inject_custom_mods(
            Game::Poe2,
            "<PathOfBuilding2><Config><Input name=\"enemyLevel\" number=\"1\"/></Config ></PathOfBuilding2>",
            &["10% more damage".to_owned()],
        )
        .expect("injects");
        assert_eq!(
            tolerant.matches("<Config").count(),
            1,
            "no duplicate section: {tolerant}"
        );
        assert!(tolerant.contains("customMods"));
    }

    #[test]
    fn comments_are_stripped_and_cdata_rejected() {
        // A commented-out end tag must not divert the injection -- the
        // engine strips comments before parsing, so the scan must too.
        let xml =
            prepare_xml("<PathOfBuilding><!-- </Config> --><Config></Config></PathOfBuilding>")
                .expect("prepares");
        assert!(!xml.contains("<!--"));
        let injected =
            inject_custom_mods(Game::Poe1, &xml, &["10% more damage".to_owned()]).expect("injects");
        assert_eq!(injected.matches("CustomModifierBlock").count(), 2);

        // A poe2 root hidden in a comment must not flip the game check.
        let cleaned = prepare_xml("<!-- <PathOfBuilding2> --><PathOfBuilding/>").expect("prepares");
        assert!(check_game_xml(Game::Poe2, &cleaned).is_err());
        assert!(check_game_xml(Game::Poe1, &cleaned).is_ok());

        assert!(prepare_xml("<PathOfBuilding><![CDATA[x]]></PathOfBuilding>").is_err());
    }

    #[test]
    fn root_detection_requires_a_name_boundary() {
        assert!(check_game_xml(Game::Poe1, "<PathOfBuildingFoo/>").is_err());
        assert!(check_game_xml(Game::Poe2, "<PathOfBuilding2x/>").is_err());
        assert!(check_game_xml(Game::Poe1, "<PathOfBuilding>x</PathOfBuilding>").is_ok());
        assert!(check_game_xml(Game::Poe2, "<PathOfBuilding2 attr=\"1\"/>").is_ok());
    }

    #[test]
    fn diff_passes_through_non_numeric_and_one_sided_stats() {
        let before = json!({"Life": 60, "MaxHit": "inf", "OnlyBefore": 1});
        let after = json!({"Life": 72.13, "MaxHit": "inf", "OnlyAfter": 2});
        let diff = diff_stats(&before, &after);
        // Rounded to two decimals: no f64 subtraction noise.
        assert_eq!(diff["Life"]["delta"], 12.13);
        assert_eq!(diff["MaxHit"]["before"], "inf");
        assert_eq!(diff["MaxHit"]["delta"], Value::Null);
        assert_eq!(diff["OnlyBefore"]["after"], Value::Null);
        assert_eq!(diff["OnlyAfter"]["before"], Value::Null);
    }

    #[test]
    fn cross_game_builds_are_rejected_loudly() {
        // Observed live: the engine silently computes a DEFAULT build
        // when fed the other game's XML — the tool must error instead.
        let (host, spawns, _) = rig(happy_script());
        let pob = PobTool::with_host(Arc::clone(&host));
        let whatif = PobWhatifTool::with_host(host);
        let poe1_xml_to_poe2 =
            json!({"game":"poe2","xml":"<PathOfBuilding><Build/></PathOfBuilding>"}).to_string();
        let poe2_xml_to_poe1 =
            json!({"game":"poe1","xml":"<PathOfBuilding2><Build/></PathOfBuilding2>"}).to_string();
        for bad in [&poe1_xml_to_poe2, &poe2_xml_to_poe1] {
            let err = pob.execute(bad).expect_err("cross-feed must fail");
            assert!(
                matches!(err, ToolError::InvalidArgs(_)),
                "expected InvalidArgs for {bad}"
            );
        }
        let err = whatif
            .execute(
                &json!({"game":"poe2","xml":"<PathOfBuilding><Build/></PathOfBuilding>","custom_mods":["10% more damage"]})
                    .to_string(),
            )
            .expect_err("cross-feed must fail");
        assert!(err.to_string().contains("PathOfBuilding2"));
        assert_eq!(spawns.load(Ordering::SeqCst), 0, "no engine boots");
    }

    #[test]
    fn custom_mods_are_xml_escaped_and_validated() {
        let escaped = inject_custom_mods(
            Game::Poe1,
            "<PathOfBuilding><Config></Config></PathOfBuilding>",
            &["+1 to maximum \"charges\" & <things>".to_owned()],
        )
        .expect("injects");
        assert!(escaped.contains("&quot;charges&quot; &amp; &lt;things&gt;"));

        assert!(inject_custom_mods(Game::Poe1, "<PathOfBuilding/>", &[]).is_err());
        assert!(
            inject_custom_mods(Game::Poe1, "not a build", &["10% more damage".to_owned()]).is_err()
        );
    }

    #[test]
    fn games_get_separate_engines() {
        let (tool, spawns, _) = tool(happy_script());
        let _ = tool.execute(&json!({"game":"poe1","xml":"<PathOfBuilding/>"}).to_string());
        let _ = tool.execute(&json!({"game":"poe2","xml":"<PathOfBuilding2/>"}).to_string());
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
    }

    /// Manual check: `cargo test -p exile-pob -- --ignored` (needs
    /// `LuaJIT` + engine checkouts under vendor/pob, run from repo root).
    #[test]
    #[ignore = "boots live engines"]
    fn live_engines_compute_stats() {
        // The repo root is two levels above the crate during tests.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("repo root")
            .join("vendor/pob");
        let host = Arc::new(EngineHost::with_factory(
            Box::new(LuaFactory::new(find_luajit(), materialize_adapter())),
            root,
        ));
        let tool = PobTool::with_host(Arc::clone(&host));
        let whatif = PobWhatifTool::with_host(host);
        // Minimal per-game XML: targetVersion namespaces differ (law 3 —
        // never cross-feed builds between the games' engines).
        for (game, root, target) in [
            ("poe1", "PathOfBuilding", "3_0"),
            ("poe2", "PathOfBuilding2", "0_1"),
        ] {
            let xml = format!("<{root}><Build level=\"1\" targetVersion=\"{target}\"/></{root}>");
            let result = tool
                .execute(&json!({"game": game, "xml": xml, "stats": ["Life", "Mana"]}).to_string())
                .expect("live engine computes");
            let value: Value = serde_json::from_str(&result).expect("valid JSON");
            assert!(
                value["build"]["stats"]["Life"].as_f64().unwrap_or(0.0) > 0.0,
                "{game}: no Life in {value}"
            );

            // The what-if on the same warm child: an increased-life line
            // must move Life by exactly after − before, and upward.
            let result = whatif
                .execute(
                    &json!({
                        "game": game,
                        "xml": xml,
                        "custom_mods": ["20% increased maximum Life"],
                        "stats": ["Life"],
                    })
                    .to_string(),
                )
                .expect("live whatif computes");
            let value: Value = serde_json::from_str(&result).expect("valid JSON");
            let life = &value["whatif"]["stats"]["Life"];
            let (before, after, delta) = (
                life["before"].as_f64().expect("before"),
                life["after"].as_f64().expect("after"),
                life["delta"].as_f64().expect("delta"),
            );
            assert!(delta > 0.0, "{game}: increased life must raise Life");
            assert!(
                (after - before - delta).abs() < f64::EPSILON,
                "{game}: delta must equal after - before"
            );
        }
    }
}

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
use std::sync::Mutex;

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
    custom_mods: Option<Vec<String>>,
}

/// Apply hypothetical modifier lines to a build by injecting them as the
/// config tab's custom modifiers (the engine parses and stacks them like
/// any other modifier source — this is how what-if questions stay engine
/// math instead of LLM arithmetic). The injection happens on the Rust
/// side so the model never hand-edits XML.
fn inject_custom_mods(xml: &str, mods: &[String]) -> Result<String, String> {
    if mods.is_empty() || mods.iter().all(|line| line.trim().is_empty()) {
        return Err("`custom_mods` must contain at least one modifier line".to_owned());
    }
    if xml.contains("\"customMods\"") || xml.contains("'customMods'") {
        return Err(
            "this build already carries custom modifiers — edit them in the build instead of \
             passing `custom_mods`"
                .to_owned(),
        );
    }
    // The engine's own XML parser knows only the five named entities.
    let escaped: Vec<String> = mods.iter().map(|line| xml_escape(line.trim())).collect();
    let input = format!(
        "<Input name=\"customMods\" string=\"{}\"/>",
        escaped.join("\n")
    );
    if let Some(position) = xml.find("<Config>") {
        let insert_at = position + "<Config>".len();
        let mut out = String::with_capacity(xml.len() + input.len());
        out.push_str(&xml[..insert_at]);
        out.push_str(&input);
        out.push_str(&xml[insert_at..]);
        Ok(out)
    } else if let Some(position) = xml.rfind("</PathOfBuilding>") {
        let mut out = String::with_capacity(xml.len() + input.len() + 20);
        out.push_str(&xml[..position]);
        out.push_str("<Config>");
        out.push_str(&input);
        out.push_str("</Config>");
        out.push_str(&xml[position..]);
        Ok(out)
    } else {
        Err("not a Path of Building build XML (no PathOfBuilding element)".to_owned())
    }
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

/// The `pob` tool: build stats computed by the Path of Building engine.
pub struct PobTool {
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

impl PobTool {
    /// Tool with the live `LuaJIT` factory. Engine checkouts are expected
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

    /// Tool with an injected engine factory (tests).
    #[must_use]
    pub fn with_factory(factory: Box<dyn EngineFactory>, root: PathBuf) -> Self {
        Self {
            factory,
            root,
            engines: Mutex::new(EngineSlots::default()),
        }
    }

    fn compute(&self, args: &Args, xml: &str) -> Result<Value, ToolError> {
        let mut slots = self.engines.lock().expect("engine lock");
        let slot = slots.slot(args.game);
        if slot.is_none() {
            let dir = self.root.join(args.game.as_str());
            *slot = Some(self.factory.spawn(&dir).map_err(ToolError::Failed)?);
        }
        let engine = slot.as_mut().expect("engine just ensured");

        // Any protocol failure poisons the warm child: kill it (drop) so
        // the next call boots a fresh one instead of talking to a
        // desynced process.
        let result = Self::converse(engine.as_mut(), args, xml);
        if result.is_err() {
            *slot = None;
        }
        result
    }

    fn converse(engine: &mut dyn Engine, args: &Args, xml: &str) -> Result<Value, ToolError> {
        let version = exchange(engine, &json!({"id": 1, "cmd": "version"}))?;
        exchange(
            engine,
            &json!({"id": 2, "cmd": "loadXML", "xml": xml, "name": "exile"}),
        )?;
        let mut stats_request = json!({"id": 3, "cmd": "stats"});
        if let Some(keys) = &args.stats {
            stats_request["keys"] = json!(keys);
        }
        let stats = exchange(engine, &stats_request)?;

        Ok(json!({
            "engine": "Path of Building (headless, stock checkout)",
            "engine_version": version["engine"],
            "stats": stats,
            "note": "computed by the Path of Building engine, not estimated; numbers \
                     reflect the build's own configuration flags. Cite the engine version.",
        }))
    }
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

impl Tool for PobTool {
    fn name(&self) -> &'static str {
        "pob"
    }

    fn description(&self) -> &'static str {
        "Compute build statistics (DPS, life, energy shield, effective HP, resistances) with \
         the Path of Building engine. This is the only trusted source for build math — never \
         estimate or hand-calculate these numbers. Input is a Path of Building share code \
         (`code`, the base64 export used by pobb.in/pastebin) or raw build XML (`xml`). \
         Optionally list `stats` keys to read specific values. For what-if questions \
         (\"how much would 30% more spell damage give me?\"), pass the hypothetical \
         modifier lines in `custom_mods` and compare against a call without them — never \
         combine modifiers by hand, because `increased` modifiers stack with everything \
         the build already has. Codes and XML are per-game: never feed one game's build \
         to the other."
    }

    fn parameters_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"game":{"type":"string","enum":["poe1","poe2"],"description":"Which game's engine"},"code":{"type":"string","description":"Path of Building share code (url-safe base64 export)"},"xml":{"type":"string","description":"Raw Path of Building build XML (alternative to code)"},"stats":{"type":"array","items":{"type":"string"},"description":"Optional stat keys to read (default: a core set incl. Life, EnergyShield, TotalEHP, TotalDPS, CombinedDPS)"},"custom_mods":{"type":"array","items":{"type":"string"},"description":"Optional hypothetical modifier lines (e.g. '30% more spell damage') applied to the build before calculating — for what-if comparisons"}},"required":["game"],"additionalProperties":false}"#
    }

    fn execute(&self, args_json: &str) -> Result<String, ToolError> {
        let args: Args = serde_json::from_str(args_json)
            .map_err(|err| ToolError::InvalidArgs(err.to_string()))?;
        let mut xml = match (&args.code, &args.xml) {
            (Some(code), None) => codes::decode(code).map_err(ToolError::InvalidArgs)?,
            (None, Some(xml)) if !xml.trim().is_empty() => xml.clone(),
            _ => {
                return Err(ToolError::InvalidArgs(
                    "provide exactly one of `code` or `xml`".to_owned(),
                ));
            }
        };
        if let Some(mods) = &args.custom_mods {
            xml = inject_custom_mods(&xml, mods).map_err(ToolError::InvalidArgs)?;
        }

        let mut result = serde_json::Map::new();
        result.insert("game".to_owned(), json!(args.game.as_str()));
        result.insert("fetched_at".to_owned(), json!(now_utc()));
        result.insert("build".to_owned(), self.compute(&args, &xml)?);
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

    fn tool(script: Vec<String>) -> (PobTool, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let spawns = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let factory = FakeFactory {
            spawns: Arc::clone(&spawns),
            requests: Arc::clone(&requests),
            script,
        };
        (
            PobTool::with_factory(Box::new(factory), PathBuf::from("vendor/pob")),
            spawns,
            requests,
        )
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
        let request = json!({"game": "poe2", "xml": "<PathOfBuilding/>"}).to_string();
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
    fn custom_mods_are_injected_into_the_config() {
        let (tool, _, requests) = tool(happy_script());
        tool.execute(
            &json!({
                "game": "poe1",
                "xml": "<PathOfBuilding><Config></Config></PathOfBuilding>",
                "custom_mods": ["30% more spell damage", "120% increased spell damage"],
            })
            .to_string(),
        )
        .expect("executes");
        let seen = requests.lock().expect("requests");
        let load = &seen[1];
        assert!(
            load.contains(r"30% more spell damage\n120% increased spell damage"),
            "mods must be one customMods input, newline-joined: {load}"
        );
        assert!(load.contains("customMods"), "got: {load}");
    }

    #[test]
    fn custom_mods_create_a_config_section_when_missing() {
        let with_config = inject_custom_mods(
            "<PathOfBuilding><Build/></PathOfBuilding>",
            &["10% more damage".to_owned()],
        )
        .expect("injects");
        assert!(with_config.contains("<Config><Input name=\"customMods\""));
        assert!(with_config.ends_with("</Config></PathOfBuilding>"));
    }

    #[test]
    fn custom_mods_are_xml_escaped_and_validated() {
        let escaped = inject_custom_mods(
            "<PathOfBuilding><Config></Config></PathOfBuilding>",
            &["+1 to maximum \"charges\" & <things>".to_owned()],
        )
        .expect("injects");
        assert!(escaped.contains("&quot;charges&quot; &amp; &lt;things&gt;"));

        assert!(inject_custom_mods("<PathOfBuilding/>", &[]).is_err());
        assert!(inject_custom_mods("not a build", &["10% more damage".to_owned()]).is_err());
        assert!(
            inject_custom_mods(
                "<PathOfBuilding><Config><Input name=\"customMods\" string=\"x\"/></Config></PathOfBuilding>",
                &["10% more damage".to_owned()],
            )
            .is_err(),
            "existing custom mods must not be silently overridden"
        );
    }

    #[test]
    fn games_get_separate_engines() {
        let (tool, spawns, _) = tool(happy_script());
        let _ = tool.execute(&json!({"game":"poe1","xml":"<x/>"}).to_string());
        let _ = tool.execute(&json!({"game":"poe2","xml":"<x/>"}).to_string());
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
        let tool = PobTool::with_factory(
            Box::new(LuaFactory::new(find_luajit(), materialize_adapter())),
            root,
        );
        // Minimal per-game XML: targetVersion namespaces differ (law 3 —
        // never cross-feed builds between the games' engines).
        for (game, target) in [("poe1", "3_0"), ("poe2", "0_1")] {
            let xml = format!(
                "<PathOfBuilding><Build level=\"1\" targetVersion=\"{target}\"/></PathOfBuilding>"
            );
            let result = tool
                .execute(&json!({"game": game, "xml": xml, "stats": ["Life", "Mana"]}).to_string())
                .expect("live engine computes");
            let value: Value = serde_json::from_str(&result).expect("valid JSON");
            assert!(
                value["build"]["stats"]["Life"].as_f64().unwrap_or(0.0) > 0.0,
                "{game}: no Life in {value}"
            );
        }
    }
}

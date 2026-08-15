//! Supervision of a warm headless engine child: one `luajit <adapter>`
//! process per game, spoken to over the JSON-lines protocol the adapter
//! defines. Booting an engine costs seconds and ~1GB RAM, so the child
//! stays alive across requests; on any protocol failure it is killed and
//! the next request respawns it.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Engines answer instantly once booted; boot itself loads the full
/// tree + mod DB. Generous ceilings so a cold start on a slow disk
/// fails loudly rather than flakily.
const BOOT_TIMEOUT: Duration = Duration::from_mins(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One request/response exchange with a build engine. Implemented by the
/// live `LuaJIT` child and by test fakes.
pub trait Engine: Send {
    /// Send one JSON request line, receive one JSON response line.
    fn request(&mut self, line: &str) -> Result<String, String>;
}

/// Spawns engines on demand. The factory seam keeps `PobTool` testable
/// without `LuaJIT` or engine checkouts.
pub trait EngineFactory: Send + Sync {
    /// Boot an engine for the checkout at `engine_dir`.
    fn spawn(&self, engine_dir: &Path) -> Result<Box<dyn Engine>, String>;
}

/// Live factory: `luajit <adapter> ` with cwd `<checkout>/src`.
pub struct LuaFactory {
    luajit: PathBuf,
    adapter: PathBuf,
}

impl LuaFactory {
    /// A factory using the given interpreter and adapter script paths.
    #[must_use]
    pub fn new(luajit: PathBuf, adapter: PathBuf) -> Self {
        Self { luajit, adapter }
    }
}

impl EngineFactory for LuaFactory {
    fn spawn(&self, engine_dir: &Path) -> Result<Box<dyn Engine>, String> {
        let src = engine_dir.join("src");
        if !src.join("HeadlessWrapper.lua").is_file() {
            return Err(format!(
                "no engine checkout at {} — clone the stock Path of Building repo there \
                 (see crates/exile-tools/exile-pob/README.md)",
                engine_dir.display()
            ));
        }
        let mut child = Command::new(&self.luajit)
            .arg(&self.adapter)
            .current_dir(&src)
            // CI=true silently disables the engine's ModCache and forces a
            // slow full mod parse — never inherit it.
            .env_remove("CI")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("spawning {} failed: {err}", self.luajit.display()))?;

        let stdout = child.stdout.take().ok_or("child stdout not captured")?;
        let (sender, receiver) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });

        let mut engine = LuaEngine { child, receiver };
        // The engine prints boot noise to stderr; stdout stays silent
        // until the adapter's ready banner.
        let banner = engine.receive(BOOT_TIMEOUT).map_err(|err| {
            engine.kill();
            format!("engine did not become ready: {err}")
        })?;
        if !banner.contains("\"ready\":true") {
            engine.kill();
            return Err(format!("unexpected ready banner: {banner}"));
        }
        Ok(Box::new(engine))
    }
}

struct LuaEngine {
    child: Child,
    receiver: mpsc::Receiver<String>,
}

impl LuaEngine {
    fn receive(&mut self, timeout: Duration) -> Result<String, String> {
        match self.receiver.recv_timeout(timeout) {
            Ok(line) => Ok(line),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("no response within {}s", timeout.as_secs()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("engine exited".to_owned()),
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Engine for LuaEngine {
    fn request(&mut self, line: &str) -> Result<String, String> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or("child stdin not captured")?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .map_err(|err| format!("writing to engine failed: {err}"))?;
        self.receive(REQUEST_TIMEOUT)
    }
}

impl Drop for LuaEngine {
    fn drop(&mut self) {
        self.kill();
    }
}

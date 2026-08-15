//! TOML configuration: named model profiles.
//!
//! The real config (`exile.toml`) is gitignored because endpoints are
//! private infrastructure; `exile.example.toml` in the repo root shows the
//! shape with placeholders. API keys never live in the file — profiles
//! name an environment variable instead.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// Default idle-stream timeout: long enough for local hardware to chew a
/// large prompt before the first token, short enough to catch true stalls.
pub(crate) const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_mins(3);

/// Whole config file: a default profile name plus named profiles.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Profile used when none is named on the command line.
    pub default_profile: String,
    /// Named endpoint profiles.
    pub profiles: BTreeMap<String, Profile>,
    /// Harness-level limits (apply regardless of profile).
    #[serde(default)]
    pub limits: Limits,
}

/// Harness-level limits.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Max model→tools→model rounds per turn (default 8).
    #[serde(default)]
    pub max_tool_rounds: Option<usize>,
}

/// One model endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// OpenAI-compatible base URL, e.g. `http://host:11434/v1`.
    pub base_url: String,
    /// Model name as the server knows it.
    pub model: String,
    /// Environment variable holding the API key, if the endpoint needs one.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// How tool definitions reach the model.
    #[serde(default)]
    pub tool_mode: ToolMode,
    /// Sampling temperature; omit for the server default. The eval forces
    /// 0 for reproducibility unless a profile sets one explicitly.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Seconds of stream *silence* before a request fails (default 180).
    /// Active streaming is never interrupted by this.
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
    /// Optional wall-clock ceiling in seconds for one whole completion
    /// request. The eval forces 600 unless a profile sets one.
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

/// How tool definitions reach the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolMode {
    /// OpenAI-native `tools` + `tool_calls` protocol.
    #[default]
    Native,
    /// Tools described in the prompt; calls parsed from the reply. For
    /// models whose native function calling is unreliable.
    Prompted,
}

impl Config {
    /// Load and validate a config file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .map_err(|err| format!("invalid config {}: {err}", path.display()))?;
        if !config.profiles.contains_key(&config.default_profile) {
            return Err(format!(
                "default_profile `{}` is not defined in [profiles]",
                config.default_profile
            ));
        }
        for (name, profile) in &config.profiles {
            if let Some(temperature) = profile.temperature
                && !(temperature.is_finite() && (0.0..=2.0).contains(&temperature))
            {
                return Err(format!(
                    "profile `{name}`: temperature must be a finite number between 0 and 2"
                ));
            }
            if profile.idle_timeout_secs == Some(0) {
                return Err(format!("profile `{name}`: idle_timeout_secs must be > 0"));
            }
            if profile.request_timeout_secs == Some(0) {
                return Err(format!(
                    "profile `{name}`: request_timeout_secs must be > 0"
                ));
            }
        }
        if config.limits.max_tool_rounds == Some(0) {
            return Err("limits.max_tool_rounds must be > 0".to_owned());
        }
        Ok(config)
    }

    /// Resolve a profile by name, or the default when `name` is `None`.
    pub fn profile(&self, name: Option<&str>) -> Result<(&str, &Profile), String> {
        let requested = name.unwrap_or(&self.default_profile);
        self.profiles
            .get_key_value(requested)
            .map(|(key, profile)| (key.as_str(), profile))
            .ok_or_else(|| {
                let known: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
                format!(
                    "unknown profile `{requested}` (available: {})",
                    known.join(", ")
                )
            })
    }
}

impl Profile {
    /// Resolve the API key from the configured environment variable.
    ///
    /// A profile that names an `api_key_env` declares the endpoint needs
    /// auth — an unreadable variable is a hard error, never a silent
    /// unauthenticated request (which would surface as an opaque 401).
    pub fn api_key(&self) -> Result<Option<String>, String> {
        match &self.api_key_env {
            None => Ok(None),
            Some(var) => std::env::var(var).map(Some).map_err(|err| {
                format!("api_key_env `{var}` cannot be read ({err}); export it or remove it from the profile")
            }),
        }
    }

    /// The idle-stream timeout (default 180s).
    #[must_use]
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout_secs
            .map_or(DEFAULT_IDLE_TIMEOUT, Duration::from_secs)
    }

    /// The whole-request ceiling, when configured.
    #[must_use]
    pub fn request_timeout(&self) -> Option<Duration> {
        self.request_timeout_secs.map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
default_profile = "local"

[profiles.local]
base_url = "http://localhost:11434/v1"
model = "some-model"

[profiles.hosted]
base_url = "https://example.invalid/v1"
model = "other-model"
api_key_env = "EXAMPLE_API_KEY"
tool_mode = "prompted"
"#;

    #[test]
    fn parses_and_resolves_profiles() {
        let config: Config = toml::from_str(EXAMPLE).expect("parses");
        let (name, profile) = config.profile(None).expect("default resolves");
        assert_eq!(name, "local");
        assert_eq!(profile.tool_mode, ToolMode::Native);

        let (_, hosted) = config.profile(Some("hosted")).expect("named resolves");
        assert_eq!(hosted.tool_mode, ToolMode::Prompted);
        assert_eq!(hosted.api_key_env.as_deref(), Some("EXAMPLE_API_KEY"));

        let err = config.profile(Some("missing")).expect_err("unknown");
        assert!(err.contains("available: hosted, local"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let bad = format!("{EXAMPLE}\n[profiles.local.extra]\nx = 1\n");
        assert!(toml::from_str::<Config>(&bad).is_err());
    }

    #[test]
    fn unset_api_key_env_is_a_hard_error() {
        let config: Config = toml::from_str(EXAMPLE).expect("parses");
        let (_, local) = config.profile(None).expect("resolves");
        assert_eq!(local.api_key(), Ok(None), "no api_key_env means no key");

        let mut hosted = config.profiles["hosted"].clone();
        hosted.api_key_env = Some("EXILE_TEST_DEFINITELY_UNSET_VARIABLE".to_owned());
        let err = hosted.api_key().expect_err("unset variable must error");
        assert!(err.contains("EXILE_TEST_DEFINITELY_UNSET_VARIABLE"));
    }

    #[test]
    fn limits_and_timeouts_parse_and_validate() {
        let with_limits = format!("{EXAMPLE}\n[limits]\nmax_tool_rounds = 4\n");
        let config: Config = toml::from_str(&with_limits).expect("parses");
        assert_eq!(config.limits.max_tool_rounds, Some(4));

        let (_, local) = config.profile(None).expect("resolves");
        assert_eq!(local.idle_timeout(), Duration::from_mins(3));
        assert_eq!(local.request_timeout(), None);

        let mut tweaked = config.profiles["local"].clone();
        tweaked.idle_timeout_secs = Some(30);
        tweaked.request_timeout_secs = Some(120);
        assert_eq!(tweaked.idle_timeout(), Duration::from_secs(30));
        assert_eq!(tweaked.request_timeout(), Some(Duration::from_mins(2)));
    }

    #[test]
    fn load_reports_missing_file_and_bad_default_profile() {
        let missing = Path::new("definitely-not-here.toml");
        let err = Config::load(missing).expect_err("missing file");
        assert!(err.contains("cannot read"));

        let path =
            std::env::temp_dir().join(format!("exile-config-test-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "default_profile = \"nope\"\n[profiles.local]\nbase_url = \"http://x/v1\"\nmodel = \"m\"\n",
        )
        .expect("write temp config");
        let err = Config::load(&path).expect_err("bad default profile");
        assert!(err.contains("`nope` is not defined"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn shipped_example_config_parses_strictly() {
        // deny_unknown_fields means the example can silently rot; pin it.
        let example = include_str!("../../../exile.example.toml");
        let config: Config = toml::from_str(example).expect("exile.example.toml parses");
        assert!(
            config.profiles.contains_key(&config.default_profile),
            "example default_profile must exist"
        );
    }
}

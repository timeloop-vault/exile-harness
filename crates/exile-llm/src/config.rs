//! TOML configuration: named model profiles.
//!
//! The real config (`exile.toml`) is gitignored because endpoints are
//! private infrastructure; `exile.example.toml` in the repo root shows the
//! shape with placeholders. API keys never live in the file — profiles
//! name an environment variable instead.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// Whole config file: a default profile name plus named profiles.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Profile used when none is named on the command line.
    pub default_profile: String,
    /// Named endpoint profiles.
    pub profiles: BTreeMap<String, Profile>,
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

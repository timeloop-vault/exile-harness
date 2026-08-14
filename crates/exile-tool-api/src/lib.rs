//! Contract between the harness and its tools: the [`Tool`] trait and the
//! [`ToolRegistry`].
//!
//! Every capability (league resolver, game-data lookup, wiki retrieval,
//! Path of Building math) implements this contract as a lib crate with a
//! thin CLI wrapper — callable in-process by the harness and standalone for
//! manual testing.
//!
//! The tool boundary is JSON carried as raw strings: arguments arrive as a
//! JSON string and results are returned as a JSON string. This matches how
//! OpenAI-protocol tool calls deliver `arguments`, keeps this crate free of
//! dependencies, and lets each tool parse its own arguments with whatever
//! it likes. Game-scoped tools declare a `game` property (`poe1` | `poe2`)
//! in their parameter schema, and data-returning tools include `source` and
//! `fetched_at` fields in their results (see CLAUDE.md, project laws).

use std::collections::BTreeMap;
use std::fmt;

/// A capability the harness can invoke.
///
/// Implementations must be cheap to call repeatedly and must not assume a
/// terminal, a working directory, or any frontend. `Send + Sync` is required
/// so registries can be shared once multi-threaded frontends exist.
pub trait Tool: Send + Sync {
    /// Unique tool name, stable across versions (e.g. `league`).
    fn name(&self) -> &str;

    /// One or two sentences a model uses to decide when to call this tool.
    fn description(&self) -> &str;

    /// JSON Schema (as a JSON string) describing the arguments object.
    fn parameters_schema(&self) -> &str;

    /// Run the tool with raw JSON arguments, returning a raw JSON result.
    fn execute(&self, args_json: &str) -> Result<String, ToolError>;
}

/// Why a tool invocation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// The arguments did not match the tool's parameter schema.
    InvalidArgs(String),
    /// The tool ran but could not produce a result.
    Failed(String),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgs(reason) => write!(f, "invalid arguments: {reason}"),
            Self::Failed(reason) => write!(f, "tool failed: {reason}"),
        }
    }
}

impl std::error::Error for ToolError {}

/// The set of tools available to a session, looked up by name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. Fails if a tool with the same name is already registered.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), DuplicateTool> {
        let name = tool.name().to_owned();
        if self.tools.contains_key(&name) {
            return Err(DuplicateTool(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(AsRef::as_ref)
    }

    /// All registered tools, ordered by name.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.values().map(AsRef::as_ref)
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry has no tools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Returned by [`ToolRegistry::register`] when the name is already taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateTool(pub String);

impl fmt::Display for DuplicateTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a tool named `{}` is already registered", self.0)
    }
}

impl std::error::Error for DuplicateTool {}

#[cfg(test)]
mod tests {
    use super::*;

    struct Upper;

    impl Tool for Upper {
        fn name(&self) -> &'static str {
            "upper"
        }

        fn description(&self) -> &'static str {
            "Uppercases the raw argument string."
        }

        fn parameters_schema(&self) -> &'static str {
            r#"{"type":"object"}"#
        }

        fn execute(&self, args_json: &str) -> Result<String, ToolError> {
            Ok(args_json.to_uppercase())
        }
    }

    struct Failing;

    impl Tool for Failing {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn description(&self) -> &'static str {
            "Always fails."
        }

        fn parameters_schema(&self) -> &'static str {
            r#"{"type":"object"}"#
        }

        fn execute(&self, _args_json: &str) -> Result<String, ToolError> {
            Err(ToolError::Failed("intentional".to_owned()))
        }
    }

    #[test]
    fn register_and_execute() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(Upper)).expect("first register");
        assert_eq!(registry.len(), 1);
        let tool = registry.get("upper").expect("tool present");
        assert_eq!(tool.execute(r#"{"x":1}"#), Ok(r#"{"X":1}"#.to_owned()));
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(Upper)).expect("first register");
        let err = registry.register(Box::new(Upper)).expect_err("duplicate");
        assert_eq!(err, DuplicateTool("upper".to_owned()));
    }

    #[test]
    fn unknown_tool_is_none() {
        let registry = ToolRegistry::new();
        assert!(registry.get("nope").is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn errors_display_cleanly() {
        let failing = Failing;
        let err = failing.execute("{}").expect_err("must fail");
        assert_eq!(err.to_string(), "tool failed: intentional");
        assert_eq!(
            ToolError::InvalidArgs("bad game".to_owned()).to_string(),
            "invalid arguments: bad game"
        );
    }

    #[test]
    fn iter_is_ordered_by_name() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(Upper)).expect("register upper");
        registry
            .register(Box::new(Failing))
            .expect("register failing");
        let names: Vec<&str> = registry.iter().map(Tool::name).collect();
        assert_eq!(names, vec!["failing", "upper"]);
    }
}

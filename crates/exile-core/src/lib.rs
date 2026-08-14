//! Core of the exile harness: the agent loop, session state, and the typed
//! event stream every frontend renders.
//!
//! Rules this crate enforces by construction:
//!
//! - **No I/O assumptions.** No stdin/stdout, no terminal, no network here.
//!   Frontends (`exile-cli` now, TUI/GUI/web later) depend on this crate and
//!   render its events; this crate never depends on a frontend.
//! - **No game facts.** Knowledge enters through tools at runtime.
//!
//! Events are delivered synchronously through a sink callback. When a
//! streaming LLM client lands (milestone 4), the same [`Event`] vocabulary
//! carries real token deltas; frontends do not change.

use exile_tool_api::ToolRegistry;

/// What the harness emits while processing a turn. Frontends render these;
/// nothing else crosses the core/frontend boundary.
///
/// Every turn ends with exactly one terminal event: [`Event::TurnComplete`]
/// on success or [`Event::TurnFailed`] on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A chunk of assistant text, in order. Render incrementally.
    TokenDelta(String),
    /// A tool invocation is starting.
    ToolCallStarted {
        /// Tool name as registered.
        name: String,
        /// Raw JSON arguments.
        args_json: String,
    },
    /// A tool invocation finished successfully.
    ToolCallFinished {
        /// Tool name as registered.
        name: String,
        /// Raw JSON result.
        result_json: String,
    },
    /// A tool invocation failed (unknown tool or execution error).
    ToolCallFailed {
        /// Tool name as requested.
        name: String,
        /// Human-readable reason.
        error: String,
    },
    /// The turn failed before completing (e.g. the model backend errored).
    /// Terminal: emitted instead of [`Event::TurnComplete`]. Nothing emits
    /// this until the LLM client lands (milestone 4), but frontends must
    /// render it from day one.
    TurnFailed {
        /// Human-readable reason.
        error: String,
    },
    /// The turn is over; no more events until the next submission.
    TurnComplete,
}

/// One entry in a session's conversation transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Text the user submitted.
    User(String),
    /// Text the assistant produced.
    Assistant(String),
    /// A tool invocation made during a turn.
    ToolCall {
        /// Tool name as registered.
        name: String,
        /// Raw JSON arguments.
        args_json: String,
    },
    /// The result of a tool invocation.
    ToolResult {
        /// Tool name as registered.
        name: String,
        /// Raw JSON result.
        result_json: String,
    },
    /// A tool invocation that failed. Every [`Message::ToolCall`] is
    /// followed by exactly one [`Message::ToolResult`] or
    /// [`Message::ToolFailure`], so transcripts always pair calls with
    /// outcomes — required for OpenAI-protocol replay, where an unanswered
    /// tool call rejects the whole conversation, and it lets the model see
    /// the error and recover.
    ToolFailure {
        /// Tool name as requested.
        name: String,
        /// Human-readable reason.
        error: String,
    },
}

/// A conversation with the harness: transcript plus the tools available to
/// it. One session per conversation; frontends own the lifetime.
pub struct Session {
    registry: ToolRegistry,
    transcript: Vec<Message>,
}

impl Session {
    /// Create a session over a set of tools.
    #[must_use]
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            transcript: Vec::new(),
        }
    }

    /// The tools this session can use.
    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Everything said and done so far, in order.
    #[must_use]
    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }

    /// Submit one user turn. Events are delivered to `sink` as they occur;
    /// the turn ends with exactly one terminal event ([`Event::TurnComplete`]
    /// or [`Event::TurnFailed`]).
    ///
    /// Until the LLM client lands (milestone 4) the loop answers with a
    /// fixed notice, streamed in chunks to exercise the event path.
    pub fn submit(&mut self, input: &str, sink: &mut dyn FnMut(&Event)) {
        self.transcript.push(Message::User(input.to_owned()));

        let reply = "No model is connected yet; the LLM client lands in milestone 4. \
                     Tool calls already work end to end.";
        for chunk in split_into_chunks(reply) {
            sink(&Event::TokenDelta(chunk.to_owned()));
        }
        self.transcript.push(Message::Assistant(reply.to_owned()));
        sink(&Event::TurnComplete);
    }

    /// Invoke a registered tool directly, outside a model turn. Used by
    /// frontends for manual tool runs; the model-driven path (milestone 4)
    /// reuses the same transcript entries and events.
    ///
    /// The turn ends with exactly one terminal event ([`Event::TurnComplete`]
    /// or [`Event::TurnFailed`]). A failed tool call is recorded in the
    /// transcript as [`Message::ToolFailure`] and still completes the turn.
    pub fn call_tool(&mut self, name: &str, args_json: &str, sink: &mut dyn FnMut(&Event)) {
        sink(&Event::ToolCallStarted {
            name: name.to_owned(),
            args_json: args_json.to_owned(),
        });
        self.transcript.push(Message::ToolCall {
            name: name.to_owned(),
            args_json: args_json.to_owned(),
        });

        let outcome = match self.registry.get(name) {
            None => Err(format!("unknown tool `{name}`")),
            Some(tool) => tool.execute(args_json).map_err(|err| err.to_string()),
        };

        match outcome {
            Ok(result_json) => {
                self.transcript.push(Message::ToolResult {
                    name: name.to_owned(),
                    result_json: result_json.clone(),
                });
                sink(&Event::ToolCallFinished {
                    name: name.to_owned(),
                    result_json,
                });
            }
            Err(error) => {
                self.transcript.push(Message::ToolFailure {
                    name: name.to_owned(),
                    error: error.clone(),
                });
                sink(&Event::ToolCallFailed {
                    name: name.to_owned(),
                    error,
                });
            }
        }
        sink(&Event::TurnComplete);
    }
}

/// Split text into small chunks on word boundaries, preserving spacing, so
/// the placeholder reply exercises incremental rendering like a real stream.
fn split_into_chunks(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive(' ')
}

#[cfg(test)]
mod tests {
    use super::*;
    use exile_tool_api::{Tool, ToolError};

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

    fn session_with_upper() -> Session {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(Upper)).expect("register");
        Session::new(registry)
    }

    fn collect_events(run: impl FnOnce(&mut dyn FnMut(&Event))) -> Vec<Event> {
        let mut events = Vec::new();
        let mut sink = |event: &Event| events.push(event.clone());
        run(&mut sink);
        events
    }

    #[test]
    fn submit_streams_text_and_completes() {
        let mut session = session_with_upper();
        let events = collect_events(|sink| session.submit("hello", sink));

        assert!(matches!(events.first(), Some(Event::TokenDelta(_))));
        assert_eq!(events.last(), Some(&Event::TurnComplete));

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                Event::TokenDelta(chunk) => Some(chunk.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("milestone 4"));

        assert!(
            matches!(session.transcript().first(), Some(Message::User(input)) if input == "hello")
        );
        assert!(matches!(
            session.transcript().last(),
            Some(Message::Assistant(_))
        ));
    }

    #[test]
    fn call_tool_success_emits_started_finished_complete() {
        let mut session = session_with_upper();
        let events = collect_events(|sink| session.call_tool("upper", r#"{"a":1}"#, sink));

        assert_eq!(
            events,
            vec![
                Event::ToolCallStarted {
                    name: "upper".to_owned(),
                    args_json: r#"{"a":1}"#.to_owned(),
                },
                Event::ToolCallFinished {
                    name: "upper".to_owned(),
                    result_json: r#"{"A":1}"#.to_owned(),
                },
                Event::TurnComplete,
            ]
        );
        assert!(matches!(
            session.transcript().last(),
            Some(Message::ToolResult { name, .. }) if name == "upper"
        ));
    }

    #[test]
    fn call_tool_unknown_fails_and_completes() {
        let mut session = session_with_upper();
        let events = collect_events(|sink| session.call_tool("nope", "{}", sink));

        assert!(matches!(
            events.get(1),
            Some(Event::ToolCallFailed { name, error }) if name == "nope" && error.contains("unknown tool")
        ));
        assert_eq!(events.last(), Some(&Event::TurnComplete));
        // The failure is recorded, pairing the ToolCall with an outcome.
        assert!(matches!(
            session.transcript().last(),
            Some(Message::ToolFailure { name, error }) if name == "nope" && error.contains("unknown tool")
        ));
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
    fn call_tool_execution_error_records_failure() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(Failing)).expect("register");
        let mut session = Session::new(registry);

        let events = collect_events(|sink| session.call_tool("failing", "{}", sink));

        assert!(matches!(
            events.get(1),
            Some(Event::ToolCallFailed { name, error })
                if name == "failing" && error == "tool failed: intentional"
        ));
        assert_eq!(events.last(), Some(&Event::TurnComplete));
        assert!(matches!(
            session.transcript().last(),
            Some(Message::ToolFailure { name, error })
                if name == "failing" && error == "tool failed: intentional"
        ));
    }
}

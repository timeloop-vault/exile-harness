//! Core of the exile harness: the agent loop, session state, and the typed
//! event stream every frontend renders.
//!
//! Rules this crate enforces by construction:
//!
//! - **No I/O assumptions.** No stdin/stdout, no terminal, no network here.
//!   Frontends (`exile-cli` now, TUI/GUI/web later) depend on this crate and
//!   render its events; this crate never depends on a frontend. The model
//!   lives behind [`ModelDriver`], implemented by `exile-llm` and injected
//!   by the composition root.
//! - **No game facts.** Knowledge enters through tools at runtime.
//!
//! Events are delivered synchronously through a sink callback; the same
//! [`Event`] vocabulary carries placeholder text (no model configured) and
//! real streamed tokens alike, so frontends never change.

use exile_tool_api::ToolRegistry;
use std::fmt;

/// Upper bound on model→tools→model rounds within one turn, so a model
/// that keeps requesting tools cannot loop forever.
const MAX_TOOL_ROUNDS: usize = 8;

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
    /// Terminal: emitted instead of [`Event::TurnComplete`].
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
        /// Call id correlating the invocation with its outcome (assigned
        /// by the model, or synthesized for manual calls).
        id: String,
        /// Tool name as registered.
        name: String,
        /// Raw JSON arguments.
        args_json: String,
    },
    /// The result of a tool invocation.
    ToolResult {
        /// Call id this result answers.
        id: String,
        /// Tool name as registered.
        name: String,
        /// Raw JSON result.
        result_json: String,
    },
    /// A tool invocation that failed. Every [`Message::ToolCall`] is
    /// followed by exactly one [`Message::ToolResult`] or
    /// [`Message::ToolFailure`], so transcripts always pair calls with
    /// outcomes — required for OpenAI-protocol replay, and it lets the
    /// model see the error and recover.
    ToolFailure {
        /// Call id this failure answers.
        id: String,
        /// Tool name as requested.
        name: String,
        /// Human-readable reason.
        error: String,
    },
}

/// Tool description handed to the model driver, mirroring the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// Tool name as registered.
    pub name: String,
    /// Model-facing description.
    pub description: String,
    /// JSON Schema for the arguments, as a raw JSON string.
    pub parameters_schema: String,
}

/// A tool invocation the model requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRequest {
    /// Call id from the model (or synthesized by the driver).
    pub id: String,
    /// Requested tool name.
    pub name: String,
    /// Raw JSON arguments.
    pub args_json: String,
}

/// One completed model response within a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTurn {
    /// Assistant text (may be empty when the model only calls tools).
    pub text: String,
    /// Tool invocations the model requested, in order.
    pub tool_calls: Vec<ToolCallRequest>,
}

/// Why a model completion failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError(pub String);

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelError {}

/// Drives a language model for one completion over the transcript.
///
/// Implemented by `exile-llm`; core stays free of I/O and protocol
/// details. `on_token` streams assistant text as it arrives; the returned
/// [`ModelTurn`] is the complete response including tool calls.
pub trait ModelDriver: Send {
    /// Produce the next assistant response for the conversation.
    fn complete(
        &self,
        system_prompt: &str,
        transcript: &[Message],
        tools: &[ToolSpec],
        on_token: &mut dyn FnMut(&str),
    ) -> Result<ModelTurn, ModelError>;
}

/// A conversation with the harness: transcript plus the tools available to
/// it, and optionally a model driving the agent loop. One session per
/// conversation; frontends own the lifetime.
pub struct Session {
    registry: ToolRegistry,
    transcript: Vec<Message>,
    model: Option<Box<dyn ModelDriver>>,
    system_prompt: String,
    manual_calls: u64,
}

impl Session {
    /// Session over a set of tools, without a model (tool-only mode).
    #[must_use]
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            transcript: Vec::new(),
            model: None,
            system_prompt: String::new(),
            manual_calls: 0,
        }
    }

    /// Session with a model driving the agent loop.
    #[must_use]
    pub fn with_model(
        registry: ToolRegistry,
        model: Box<dyn ModelDriver>,
        system_prompt: String,
    ) -> Self {
        Self {
            registry,
            transcript: Vec::new(),
            model: Some(model),
            system_prompt,
            manual_calls: 0,
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

    /// Whether a model is driving this session.
    #[must_use]
    pub fn has_model(&self) -> bool {
        self.model.is_some()
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.registry
            .iter()
            .map(|tool| ToolSpec {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                parameters_schema: tool.parameters_schema().to_owned(),
            })
            .collect()
    }

    /// Submit one user turn. Events are delivered to `sink` as they occur;
    /// the turn ends with exactly one terminal event ([`Event::TurnComplete`]
    /// or [`Event::TurnFailed`]).
    ///
    /// With a model attached, this runs the agent loop: the model streams
    /// text and may request tool calls, whose results are fed back until it
    /// answers without tools (bounded by an internal round limit). Without
    /// a model, a fixed notice is streamed instead.
    pub fn submit(&mut self, input: &str, sink: &mut dyn FnMut(&Event)) {
        self.transcript.push(Message::User(input.to_owned()));

        if self.model.is_none() {
            let reply = "No model is configured; chat is disabled. \
                         Tool calls work — create a config to enable the model.";
            for chunk in reply.split_inclusive(' ') {
                sink(&Event::TokenDelta(chunk.to_owned()));
            }
            self.transcript.push(Message::Assistant(reply.to_owned()));
            sink(&Event::TurnComplete);
            return;
        }

        let specs = self.tool_specs();
        let mut round = 0;
        loop {
            // Buffer streamed text so a mid-stream failure still records
            // what the user already saw — the model must remember it too.
            let mut partial = String::new();
            let outcome = {
                let model = self.model.as_deref().expect("model presence checked");
                let mut on_token = |token: &str| {
                    partial.push_str(token);
                    sink(&Event::TokenDelta(token.to_owned()));
                };
                model.complete(&self.system_prompt, &self.transcript, &specs, &mut on_token)
            };
            let turn = match outcome {
                Ok(turn) => turn,
                Err(err) => {
                    if !partial.is_empty() {
                        self.transcript.push(Message::Assistant(partial));
                    }
                    sink(&Event::TurnFailed {
                        error: err.to_string(),
                    });
                    return;
                }
            };

            let text_empty = turn.text.is_empty();
            if !text_empty {
                self.transcript.push(Message::Assistant(turn.text));
            }
            if turn.tool_calls.is_empty() {
                if text_empty && partial.is_empty() {
                    // Neither text nor tool calls: surface it instead of
                    // silently completing a blank turn.
                    sink(&Event::TurnFailed {
                        error: "model returned an empty response".to_owned(),
                    });
                } else {
                    sink(&Event::TurnComplete);
                }
                return;
            }
            if round == MAX_TOOL_ROUNDS {
                // Do not execute calls the model can never see answered.
                sink(&Event::TurnFailed {
                    error: format!(
                        "model exceeded the limit of {MAX_TOOL_ROUNDS} tool rounds in one turn"
                    ),
                });
                return;
            }
            round += 1;
            for call in turn.tool_calls {
                self.run_tool(&call.id, &call.name, &call.args_json, sink);
            }
        }
    }

    /// Invoke a registered tool directly, outside a model turn. Used by
    /// frontends for manual tool runs; the model-driven path reuses the
    /// same transcript entries and events.
    ///
    /// The turn ends with exactly one terminal event ([`Event::TurnComplete`]
    /// or [`Event::TurnFailed`]). A failed tool call is recorded in the
    /// transcript as [`Message::ToolFailure`] and still completes the turn.
    pub fn call_tool(&mut self, name: &str, args_json: &str, sink: &mut dyn FnMut(&Event)) {
        self.manual_calls += 1;
        let id = format!("manual-{}", self.manual_calls);
        self.run_tool(&id, name, args_json, sink);
        sink(&Event::TurnComplete);
    }

    /// Execute one tool call, recording it and its outcome in the
    /// transcript and emitting the corresponding events. Never emits a
    /// terminal event — callers own turn termination.
    fn run_tool(&mut self, id: &str, name: &str, args_json: &str, sink: &mut dyn FnMut(&Event)) {
        sink(&Event::ToolCallStarted {
            name: name.to_owned(),
            args_json: args_json.to_owned(),
        });
        self.transcript.push(Message::ToolCall {
            id: id.to_owned(),
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
                    id: id.to_owned(),
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
                    id: id.to_owned(),
                    name: name.to_owned(),
                    error: error.clone(),
                });
                sink(&Event::ToolCallFailed {
                    name: name.to_owned(),
                    error,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exile_tool_api::{Tool, ToolError};
    use std::cell::RefCell;
    use std::collections::VecDeque;

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

    fn upper_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(Upper)).expect("register");
        registry
    }

    fn collect_events(run: impl FnOnce(&mut dyn FnMut(&Event))) -> Vec<Event> {
        let mut events = Vec::new();
        let mut sink = |event: &Event| events.push(event.clone());
        run(&mut sink);
        events
    }

    /// Model returning pre-scripted turns; streams each turn's text through
    /// `on_token` first, like a real driver.
    struct ScriptedModel {
        turns: RefCell<VecDeque<Result<ModelTurn, ModelError>>>,
    }

    impl ScriptedModel {
        fn new(turns: Vec<Result<ModelTurn, ModelError>>) -> Self {
            Self {
                turns: RefCell::new(turns.into()),
            }
        }
    }

    impl ModelDriver for ScriptedModel {
        fn complete(
            &self,
            _system_prompt: &str,
            _transcript: &[Message],
            _tools: &[ToolSpec],
            on_token: &mut dyn FnMut(&str),
        ) -> Result<ModelTurn, ModelError> {
            let turn = self
                .turns
                .borrow_mut()
                .pop_front()
                .expect("script exhausted");
            if let Ok(turn) = &turn
                && !turn.text.is_empty()
            {
                on_token(&turn.text);
            }
            turn
        }
    }

    fn text_turn(text: &str) -> ModelTurn {
        ModelTurn {
            text: text.to_owned(),
            tool_calls: Vec::new(),
        }
    }

    fn tool_turn(id: &str, name: &str, args: &str) -> ModelTurn {
        ModelTurn {
            text: String::new(),
            tool_calls: vec![ToolCallRequest {
                id: id.to_owned(),
                name: name.to_owned(),
                args_json: args.to_owned(),
            }],
        }
    }

    #[test]
    fn submit_without_model_streams_notice() {
        let mut session = Session::new(upper_registry());
        let events = collect_events(|sink| session.submit("hello", sink));

        assert!(matches!(events.first(), Some(Event::TokenDelta(_))));
        assert_eq!(events.last(), Some(&Event::TurnComplete));
        assert!(!session.has_model());
        assert!(matches!(
            session.transcript().last(),
            Some(Message::Assistant(_))
        ));
    }

    #[test]
    fn submit_with_model_text_only() {
        let model = ScriptedModel::new(vec![Ok(text_turn("the answer"))]);
        let mut session =
            Session::with_model(upper_registry(), Box::new(model), "system".to_owned());
        let events = collect_events(|sink| session.submit("question", sink));

        assert_eq!(
            events,
            vec![
                Event::TokenDelta("the answer".to_owned()),
                Event::TurnComplete,
            ]
        );
        assert!(matches!(
            session.transcript(),
            [Message::User(_), Message::Assistant(text)] if text == "the answer"
        ));
    }

    #[test]
    fn submit_runs_tool_round_trip() {
        let model = ScriptedModel::new(vec![
            Ok(tool_turn("call-1", "upper", r#"{"a":1}"#)),
            Ok(text_turn("done")),
        ]);
        let mut session =
            Session::with_model(upper_registry(), Box::new(model), "system".to_owned());
        let events = collect_events(|sink| session.submit("go", sink));

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
                Event::TokenDelta("done".to_owned()),
                Event::TurnComplete,
            ]
        );
        assert!(matches!(
            session.transcript(),
            [
                Message::User(_),
                Message::ToolCall { id, .. },
                Message::ToolResult { id: rid, .. },
                Message::Assistant(_),
            ] if id == "call-1" && rid == "call-1"
        ));
    }

    #[test]
    fn submit_feeds_tool_failure_back_to_model() {
        let model = ScriptedModel::new(vec![
            Ok(tool_turn("call-1", "nope", "{}")),
            Ok(text_turn("recovered")),
        ]);
        let mut session =
            Session::with_model(upper_registry(), Box::new(model), "system".to_owned());
        let events = collect_events(|sink| session.submit("go", sink));

        assert!(matches!(
            events.get(1),
            Some(Event::ToolCallFailed { name, error }) if name == "nope" && error.contains("unknown tool")
        ));
        assert_eq!(events.last(), Some(&Event::TurnComplete));
        assert!(matches!(
            session.transcript().get(2),
            Some(Message::ToolFailure { id, .. }) if id == "call-1"
        ));
    }

    #[test]
    fn submit_model_error_fails_turn() {
        let model = ScriptedModel::new(vec![Err(ModelError("connection refused".to_owned()))]);
        let mut session =
            Session::with_model(upper_registry(), Box::new(model), "system".to_owned());
        let events = collect_events(|sink| session.submit("go", sink));

        assert_eq!(
            events,
            vec![Event::TurnFailed {
                error: "connection refused".to_owned(),
            }]
        );
    }

    #[test]
    fn submit_empty_model_response_fails_turn() {
        let model = ScriptedModel::new(vec![Ok(text_turn(""))]);
        let mut session =
            Session::with_model(upper_registry(), Box::new(model), "system".to_owned());
        let events = collect_events(|sink| session.submit("go", sink));

        assert!(matches!(
            events.last(),
            Some(Event::TurnFailed { error }) if error.contains("empty response")
        ));
    }

    /// Streams some text, then fails — the partial text must be recorded
    /// so the next turn's model remembers what the user already saw.
    struct StreamThenFail;

    impl ModelDriver for StreamThenFail {
        fn complete(
            &self,
            _system_prompt: &str,
            _transcript: &[Message],
            _tools: &[ToolSpec],
            on_token: &mut dyn FnMut(&str),
        ) -> Result<ModelTurn, ModelError> {
            on_token("half an answer");
            Err(ModelError("stream read failed: reset".to_owned()))
        }
    }

    #[test]
    fn submit_records_partial_text_when_stream_fails() {
        let mut session = Session::with_model(
            upper_registry(),
            Box::new(StreamThenFail),
            "system".to_owned(),
        );
        let events = collect_events(|sink| session.submit("go", sink));

        assert!(
            matches!(events.first(), Some(Event::TokenDelta(text)) if text == "half an answer")
        );
        assert!(matches!(events.last(), Some(Event::TurnFailed { .. })));
        assert!(matches!(
            session.transcript().last(),
            Some(Message::Assistant(text)) if text == "half an answer"
        ));
    }

    #[test]
    fn submit_enforces_tool_round_limit() {
        let turns = (0..20)
            .map(|i| Ok(tool_turn(&format!("call-{i}"), "upper", "{}")))
            .collect();
        let model = ScriptedModel::new(turns);
        let mut session =
            Session::with_model(upper_registry(), Box::new(model), "system".to_owned());
        let events = collect_events(|sink| session.submit("go", sink));

        assert!(matches!(
            events.last(),
            Some(Event::TurnFailed { error }) if error.contains("limit")
        ));
    }

    #[test]
    fn call_tool_success_emits_started_finished_complete() {
        let mut session = Session::new(upper_registry());
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
            Some(Message::ToolResult { id, name, .. }) if name == "upper" && id.starts_with("manual-")
        ));
    }

    #[test]
    fn call_tool_unknown_fails_and_completes() {
        let mut session = Session::new(upper_registry());
        let events = collect_events(|sink| session.call_tool("nope", "{}", sink));

        assert!(matches!(
            events.get(1),
            Some(Event::ToolCallFailed { name, error }) if name == "nope" && error.contains("unknown tool")
        ));
        assert_eq!(events.last(), Some(&Event::TurnComplete));
        assert!(matches!(
            session.transcript().last(),
            Some(Message::ToolFailure { name, error, .. }) if name == "nope" && error.contains("unknown tool")
        ));
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
            Some(Message::ToolFailure { name, error, .. })
                if name == "failing" && error == "tool failed: intentional"
        ));
    }
}

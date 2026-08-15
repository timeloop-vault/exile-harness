//! OpenAI-compatible chat-completions client for the harness.
//!
//! vLLM, Ollama, and `OpenRouter` all speak this protocol, so one
//! hand-written client with a configurable base URL covers local inference
//! and hosted fallback with zero provider abstractions. Implements
//! `exile-core`'s `ModelDriver`: streaming SSE responses (with a fallback
//! for endpoints that answer a plain chat completion), native
//! `tools`/`tool_calls`, and a prompted tool-call fallback for models whose
//! native function calling is unreliable — including reasoning models that
//! wrap replies in `<think>` blocks.
//!
//! Transport is behind [`ChatTransport`] so unit tests feed canned SSE
//! streams and never touch the network.

mod config;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read};
use std::sync::mpsc;
use std::time::Duration;

use exile_core::{Message, ModelDriver, ModelError, ModelTurn, ToolCallRequest, ToolSpec};
use serde_json::{Value, json};

pub use config::{Config, Profile, ToolMode};

/// Instructions appended to the system prompt in prompted tool mode.
/// Maintained as Markdown in `prompts/` (project convention: prompts are
/// content, embedded at build time — never inline strings).
const PROMPTED_TOOL_INSTRUCTIONS: &str = include_str!("../../../prompts/prompted-tool-calling.md");

/// Hard cap on distinct tool calls accepted from one streamed response;
/// the accumulator is sized by server-controlled data, so it must be
/// bounded.
const MAX_STREAM_TOOL_CALLS: usize = 32;

/// Cap on how much of a non-SSE response body is buffered for the
/// plain-completion fallback and error reporting.
const MAX_FALLBACK_BODY: usize = 256 * 1024;

/// POSTs a JSON body and returns the (streaming) response body reader.
pub trait ChatTransport: Send + Sync {
    /// Send `body` to `url`; the reader yields the raw response bytes.
    fn post_json(
        &self,
        url: &str,
        api_key: Option<&str>,
        body: &str,
    ) -> Result<Box<dyn Read + Send>, String>;
}

/// Live transport via ureq. No global timeout: streamed completions run
/// long by design; connect problems still fail fast at the OS level.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    /// Build the live transport without a request ceiling.
    #[must_use]
    pub fn new() -> Self {
        Self::with_ceiling(None)
    }

    /// Build the live transport with an optional wall-clock ceiling for
    /// the whole request (connect + headers + body). Connecting always has
    /// its own short timeout: a blackholed endpoint must fail fast rather
    /// than hang before the idle watchdog can even engage.
    #[must_use]
    pub fn with_ceiling(request_timeout: Option<Duration>) -> Self {
        let agent_config = ureq::Agent::config_builder()
            .timeout_global(request_timeout)
            .timeout_connect(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .user_agent(exile_toolkit::USER_AGENT)
            .build();
        Self {
            agent: agent_config.into(),
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatTransport for UreqTransport {
    fn post_json(
        &self,
        url: &str,
        api_key: Option<&str>,
        body: &str,
    ) -> Result<Box<dyn Read + Send>, String> {
        let mut request = self
            .agent
            .post(url)
            .header("Content-Type", "application/json");
        if let Some(key) = api_key {
            request = request.header("Authorization", &format!("Bearer {key}"));
        }
        let response = request
            .send(body)
            .map_err(|err| format!("POST {url} failed: {err}"))?;
        let status = response.status();
        if status.as_u16() >= 400 {
            // The server's JSON body carries the actual diagnosis (wrong
            // model name, bad key, context overflow) — surface it.
            let mut detail = String::new();
            let _ = response
                .into_body()
                .into_reader()
                .take(2048)
                .read_to_string(&mut detail);
            return Err(format!(
                "POST {url} failed: HTTP {status}: {}",
                detail.trim()
            ));
        }
        Ok(Box::new(response.into_body().into_reader()))
    }
}

/// The OpenAI-compatible client; implements `ModelDriver`.
pub struct OpenAiClient {
    transport: Box<dyn ChatTransport>,
    base_url: String,
    model: String,
    api_key: Option<String>,
    tool_mode: ToolMode,
    temperature: Option<f64>,
    idle_timeout: Duration,
}

impl OpenAiClient {
    /// Client for a config profile, with the live transport. Fails when the
    /// profile names an `api_key_env` that cannot be read — a declared key
    /// must never silently degrade to an unauthenticated request.
    pub fn for_profile(profile: &Profile) -> Result<Self, String> {
        let api_key = profile.api_key()?;
        let mut client = Self::with_transport(
            Box::new(UreqTransport::with_ceiling(profile.request_timeout())),
            &profile.base_url,
            &profile.model,
            api_key,
            profile.tool_mode,
        );
        client.temperature = profile.temperature;
        client.idle_timeout = profile.idle_timeout();
        Ok(client)
    }

    /// Client with an injected transport (tests).
    #[must_use]
    pub fn with_transport(
        transport: Box<dyn ChatTransport>,
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        tool_mode: ToolMode,
    ) -> Self {
        Self {
            transport,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model: model.to_owned(),
            api_key,
            tool_mode,
            temperature: None,
            idle_timeout: config::DEFAULT_IDLE_TIMEOUT,
        }
    }

    /// The model name this client requests.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    fn request_body(
        &self,
        system_prompt: &str,
        transcript: &[Message],
        tools: &[ToolSpec],
    ) -> Value {
        let mut body = match self.tool_mode {
            ToolMode::Native => {
                let mut body = json!({
                    "model": self.model,
                    "stream": true,
                    "messages": build_messages(system_prompt, transcript, false),
                });
                if !tools.is_empty() {
                    body["tools"] = Value::Array(tools.iter().map(tool_definition).collect());
                }
                body
            }
            ToolMode::Prompted => {
                let mut prompt = format!(
                    "{system_prompt}\n\n{}\n",
                    PROMPTED_TOOL_INSTRUCTIONS.trim_end()
                );
                for tool in tools {
                    let _ = writeln!(
                        prompt,
                        "- {}: {} (arguments schema: {})",
                        tool.name, tool.description, tool.parameters_schema
                    );
                }
                json!({
                    "model": self.model,
                    "stream": true,
                    "messages": build_messages(&prompt, transcript, true),
                })
            }
        };
        if let Some(temperature) = self.temperature {
            body["temperature"] = json!(temperature);
        }
        body
    }
}

impl ModelDriver for OpenAiClient {
    fn complete(
        &self,
        system_prompt: &str,
        transcript: &[Message],
        tools: &[ToolSpec],
        on_token: &mut dyn FnMut(&str),
    ) -> Result<ModelTurn, ModelError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self
            .request_body(system_prompt, transcript, tools)
            .to_string();
        let reader = self
            .transport
            .post_json(&url, self.api_key.as_deref(), &body)
            .map_err(ModelError)?;
        // Idle watchdog: an actively-streaming completion is never
        // interrupted, but silence beyond the configured window fails the
        // turn instead of hanging forever.
        let reader: Box<dyn Read + Send> =
            Box::new(IdleTimeoutReader::new(reader, self.idle_timeout));

        match self.tool_mode {
            ToolMode::Native => read_stream(reader, on_token),
            ToolMode::Prompted => {
                // A prompted tool call arrives as reply text; buffer the
                // stream so tool-call JSON is never rendered as chat.
                let mut silent = |_: &str| {};
                let turn = read_stream(reader, &mut silent)?;
                if let Some(call) = extract_prompted_tool_call(&turn.text) {
                    Ok(ModelTurn {
                        text: String::new(),
                        tool_calls: vec![call],
                    })
                } else {
                    on_token(&turn.text);
                    Ok(turn)
                }
            }
        }
    }
}

fn tool_definition(spec: &ToolSpec) -> Value {
    // Schemas are raw strings at the tool boundary; parse here, at the
    // protocol edge. An unparseable schema falls back to a permissive one
    // rather than failing the whole turn.
    let parameters: Value =
        serde_json::from_str(&spec.parameters_schema).unwrap_or_else(|_| json!({"type": "object"}));
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": parameters,
        }
    })
}

/// Map the flat transcript onto protocol messages. Each `ToolCall` becomes
/// an assistant message carrying one `tool_calls` entry, each outcome a
/// `tool` message answering its id — a valid (if verbose) `OpenAI` shape
/// that keeps the builder trivially correct. In prompted mode tool traffic
/// is rendered as plain text instead, since the model never saw the
/// native protocol. All content goes through `json!` so quotes and
/// backslashes in errors or arguments are always escaped.
fn build_messages(system_prompt: &str, transcript: &[Message], prompted: bool) -> Vec<Value> {
    let mut messages = vec![json!({"role": "system", "content": system_prompt})];
    for message in transcript {
        messages.push(match message {
            Message::User(text) => json!({"role": "user", "content": text}),
            Message::Assistant(text) => json!({"role": "assistant", "content": text}),
            Message::ToolCall {
                id,
                name,
                args_json,
            } => {
                if prompted {
                    let args: Value = serde_json::from_str(args_json).unwrap_or_else(|_| json!({}));
                    let rendered = json!({"tool_call": {"name": name, "arguments": args}});
                    json!({"role": "assistant", "content": rendered.to_string()})
                } else {
                    json!({"role": "assistant", "content": Value::Null,
                           "tool_calls": [{"id": id, "type": "function",
                                            "function": {"name": name, "arguments": args_json}}]})
                }
            }
            Message::ToolResult {
                id,
                name,
                result_json,
            } => {
                if prompted {
                    json!({"role": "user",
                           "content": format!("Tool result ({name}): {result_json}")})
                } else {
                    json!({"role": "tool", "tool_call_id": id, "content": result_json})
                }
            }
            Message::ToolFailure { id, name, error } => {
                if prompted {
                    json!({"role": "user", "content": format!("Tool error ({name}): {error}")})
                } else {
                    let content = json!({"error": error}).to_string();
                    json!({"role": "tool", "tool_call_id": id, "content": content})
                }
            }
        });
    }
    messages
}

/// Wraps a blocking reader with an idle watchdog: a pump thread forwards
/// chunks over a channel and the consumer fails with `TimedOut` when no
/// data arrives within the window. Active streaming is never interrupted —
/// only silence is. The pump thread may linger parked on a dead read until
/// the connection drops; that is the accepted cost of a true idle timeout
/// over blocking I/O.
struct IdleTimeoutReader {
    receiver: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    idle: Duration,
    buffer: Vec<u8>,
    offset: usize,
    done: bool,
}

impl IdleTimeoutReader {
    fn new(mut inner: Box<dyn Read + Send>, idle: Duration) -> Self {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match inner.read(&mut chunk) {
                    Ok(0) => {
                        // EOF marker: an empty chunk.
                        let _ = sender.send(Ok(Vec::new()));
                        break;
                    }
                    Ok(count) => {
                        if sender.send(Ok(chunk[..count].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = sender.send(Err(err));
                        break;
                    }
                }
            }
        });
        Self {
            receiver,
            idle,
            buffer: Vec::new(),
            offset: 0,
            done: false,
        }
    }
}

impl Read for IdleTimeoutReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.offset >= self.buffer.len() {
            if self.done {
                return Ok(0);
            }
            match self.receiver.recv_timeout(self.idle) {
                Ok(Ok(chunk)) if chunk.is_empty() => {
                    self.done = true;
                    return Ok(0);
                }
                Ok(Ok(chunk)) => {
                    self.buffer = chunk;
                    self.offset = 0;
                }
                Ok(Err(err)) => {
                    self.done = true;
                    return Err(err);
                }
                Err(_) => {
                    self.done = true;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "no data received for {}s (idle timeout; the endpoint may be stalled)",
                            self.idle.as_secs()
                        ),
                    ));
                }
            }
        }
        let count = (self.buffer.len() - self.offset).min(buf.len());
        buf[..count].copy_from_slice(&self.buffer[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

/// Accumulates one streamed tool call across SSE fragments.
#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    args: String,
}

/// Merge one streamed `tool_calls` fragment into the accumulator.
///
/// Spec-compliant servers send dense indexes with the name once per call;
/// several compat servers omit `index` or reuse `0` while emitting each
/// call complete. Rules: indexes never allocate past the end (append
/// instead), a fragment restating identity over a populated slot starts a
/// new call, and `name` is overwritten rather than appended.
fn merge_tool_fragment(
    calls: &mut Vec<PartialToolCall>,
    fragment: &Value,
) -> Result<(), ModelError> {
    let fragment_id = fragment["id"].as_str();
    let fragment_name = fragment["function"]["name"].as_str();
    let fragment_args = fragment["function"]["arguments"].as_str();
    if fragment_id.is_none() && fragment_name.is_none() && fragment_args.is_none() {
        return Ok(());
    }

    let mut index = fragment["index"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_else(|| calls.len().saturating_sub(1));
    if index > calls.len() {
        index = calls.len();
    }
    if index < calls.len() {
        let existing = &calls[index];
        let starts_new_call = match fragment_id {
            Some(id) => !existing.id.is_empty() && existing.id != id,
            None => {
                fragment_name.is_some() && !existing.name.is_empty() && !existing.args.is_empty()
            }
        };
        if starts_new_call {
            index = calls.len();
        }
    }
    if index == calls.len() {
        if calls.len() >= MAX_STREAM_TOOL_CALLS {
            return Err(ModelError(format!(
                "server streamed more than {MAX_STREAM_TOOL_CALLS} tool calls in one turn"
            )));
        }
        calls.push(PartialToolCall::default());
    }

    let call = &mut calls[index];
    if let Some(id) = fragment_id {
        id.clone_into(&mut call.id);
    }
    if let Some(name) = fragment_name {
        name.clone_into(&mut call.name);
    }
    if let Some(args) = fragment_args {
        call.args.push_str(args);
    }
    Ok(())
}

fn finalize_calls(calls: Vec<PartialToolCall>) -> Vec<ToolCallRequest> {
    calls
        .into_iter()
        .enumerate()
        .filter(|(_, call)| !call.name.is_empty())
        .map(|(index, call)| ToolCallRequest {
            id: if call.id.is_empty() {
                format!("call-{index}")
            } else {
                call.id
            },
            name: call.name,
            args_json: if call.args.is_empty() {
                "{}".to_owned()
            } else {
                call.args
            },
        })
        .collect()
}

/// Read an OpenAI-style response to completion, streaming text deltas
/// through `on_token`. Handles SSE streams and, when the endpoint ignored
/// `stream: true`, falls back to parsing a plain chat completion.
fn read_stream(
    reader: Box<dyn Read + Send>,
    on_token: &mut dyn FnMut(&str),
) -> Result<ModelTurn, ModelError> {
    let mut text = String::new();
    let mut calls: Vec<PartialToolCall> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut saw_data = false;
    let mut fallback_body = String::new();

    for line in BufReader::new(reader).lines() {
        let line = line.map_err(|err| ModelError(format!("stream read failed: {err}")))?;
        let Some(payload) = line.strip_prefix("data: ") else {
            if !saw_data && fallback_body.len() < MAX_FALLBACK_BODY {
                fallback_body.push_str(&line);
            }
            continue;
        };
        saw_data = true;
        if payload.trim() == "[DONE]" {
            break;
        }
        let chunk: Value = serde_json::from_str(payload)
            .map_err(|err| ModelError(format!("bad stream chunk: {err} in {payload}")))?;
        if let Some(message) = chunk.get("error") {
            return Err(ModelError(format!("server error: {message}")));
        }
        let choice = &chunk["choices"][0];
        if let Some(reason) = choice["finish_reason"].as_str() {
            finish_reason = Some(reason.to_owned());
        }
        let delta = &choice["delta"];
        if let Some(content) = delta["content"].as_str()
            && !content.is_empty()
        {
            text.push_str(content);
            on_token(content);
        }
        if let Some(fragments) = delta["tool_calls"].as_array() {
            for fragment in fragments {
                merge_tool_fragment(&mut calls, fragment)?;
            }
        }
    }

    if !saw_data {
        return parse_plain_completion(&fallback_body, on_token);
    }
    if finish_reason.as_deref() == Some("length") {
        return Err(ModelError(
            "model response was truncated (finish_reason=length); raise the server's output \
             token limit"
                .to_owned(),
        ));
    }
    Ok(ModelTurn {
        text,
        tool_calls: finalize_calls(calls),
    })
}

/// Parse a non-streaming `chat.completion` body — returned by gateways
/// that ignore `stream: true`.
fn parse_plain_completion(
    body: &str,
    on_token: &mut dyn FnMut(&str),
) -> Result<ModelTurn, ModelError> {
    let value: Value = serde_json::from_str(body.trim()).map_err(|_| {
        ModelError(
            "response contained no SSE data and is not a chat completion — the endpoint may \
             not be OpenAI-compatible"
                .to_owned(),
        )
    })?;
    if let Some(message) = value.get("error") {
        return Err(ModelError(format!("server error: {message}")));
    }
    let message = &value["choices"][0]["message"];
    if message.is_null() {
        return Err(ModelError("response contained no choices".to_owned()));
    }
    let text = message["content"].as_str().unwrap_or_default().to_owned();
    if !text.is_empty() {
        on_token(&text);
    }
    let mut tool_calls = Vec::new();
    if let Some(entries) = message["tool_calls"].as_array() {
        for (index, entry) in entries.iter().enumerate() {
            if let Some(name) = entry["function"]["name"].as_str() {
                tool_calls.push(ToolCallRequest {
                    id: entry["id"]
                        .as_str()
                        .map_or_else(|| format!("call-{index}"), str::to_owned),
                    name: name.to_owned(),
                    args_json: entry["function"]["arguments"]
                        .as_str()
                        .unwrap_or("{}")
                        .to_owned(),
                });
            }
        }
    }
    Ok(ModelTurn { text, tool_calls })
}

/// Parse a prompted-mode reply as a tool call. Tolerates `<think>` blocks
/// (reasoning models emit them regardless of instructions), code fences,
/// and surrounding prose.
fn extract_prompted_tool_call(text: &str) -> Option<ToolCallRequest> {
    let cleaned = strip_think_blocks(text);
    let trimmed = cleaned.trim();
    let candidate = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map_or(trimmed, |rest| rest.trim_end_matches("```"))
        .trim();
    parse_tool_call_value(candidate).or_else(|| find_embedded_tool_call(candidate))
}

fn parse_tool_call_value(candidate: &str) -> Option<ToolCallRequest> {
    let value: Value = serde_json::from_str(candidate).ok()?;
    let call = value.get("tool_call")?;
    let name = call.get("name")?.as_str()?;
    let args = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Some(ToolCallRequest {
        id: "prompted-1".to_owned(),
        name: name.to_owned(),
        args_json: args.to_string(),
    })
}

/// Remove `<think>...</think>` spans; an unterminated block drops the rest.
fn strip_think_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + "</think>".len()..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Scan prose for the first balanced JSON object that parses as a
/// tool call.
fn find_embedded_tool_call(text: &str) -> Option<ToolCallRequest> {
    let mut search = text;
    while let Some(position) = search.find('{') {
        let slice = &search[position..];
        if let Some(candidate) = balanced_object_prefix(slice)
            && let Some(call) = parse_tool_call_value(candidate)
        {
            return Some(call);
        }
        search = &search[position + 1..];
    }
    None
}

/// The balanced `{...}` prefix of `text` (which must start with `{`),
/// respecting strings and escapes.
fn balanced_object_prefix(text: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in text.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&text[..=offset]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    /// One captured request: `(url, api_key, body)`.
    type Captured = Arc<Mutex<Vec<(String, Option<String>, String)>>>;

    struct FixtureTransport {
        body: &'static str,
        captured: Captured,
    }

    impl FixtureTransport {
        fn new(body: &'static str) -> (Self, Captured) {
            let captured: Captured = Arc::default();
            (
                Self {
                    body,
                    captured: Arc::clone(&captured),
                },
                captured,
            )
        }
    }

    impl ChatTransport for FixtureTransport {
        fn post_json(
            &self,
            url: &str,
            api_key: Option<&str>,
            body: &str,
        ) -> Result<Box<dyn Read + Send>, String> {
            self.captured.lock().expect("lock").push((
                url.to_owned(),
                api_key.map(str::to_owned),
                body.to_owned(),
            ));
            Ok(Box::new(Cursor::new(self.body)))
        }
    }

    const TEXT_STREAM: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\
data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\
data: [DONE]\n";

    const TOOL_STREAM: &str = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"function\":{\"name\":\"league\",\"arguments\":\"{\\\"ga\"}}]}}]}\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"me\\\":\\\"poe1\\\"}\"}}]}}]}\n\
data: [DONE]\n";

    /// Two complete calls, no `index` on either — the compat-server shape
    /// that must not merge into one garbage call.
    const INDEXLESS_TWO_CALLS: &str = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"a\",\"function\":{\"name\":\"league\",\"arguments\":\"{\\\"game\\\":\\\"poe1\\\"}\"}}]}}]}\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"b\",\"function\":{\"name\":\"echo\",\"arguments\":\"{\\\"x\\\":1}\"}}]}}]}\n\
data: [DONE]\n";

    fn client_with(body: &'static str, mode: ToolMode) -> (OpenAiClient, Captured) {
        let (transport, captured) = FixtureTransport::new(body);
        (
            OpenAiClient::with_transport(
                Box::new(transport),
                "http://model.invalid/v1/",
                "test-model",
                Some("secret-key".to_owned()),
                mode,
            ),
            captured,
        )
    }

    fn league_spec() -> ToolSpec {
        ToolSpec {
            name: "league".to_owned(),
            description: "Resolve leagues.".to_owned(),
            parameters_schema: r#"{"type":"object"}"#.to_owned(),
        }
    }

    #[test]
    fn streams_text_deltas() {
        let (client, _) = client_with(TEXT_STREAM, ToolMode::Native);
        let mut tokens = Vec::new();
        let turn = client
            .complete("sys", &[], &[], &mut |token| tokens.push(token.to_owned()))
            .expect("completes");
        assert_eq!(tokens, vec!["Hel", "lo"]);
        assert_eq!(turn.text, "Hello");
        assert!(turn.tool_calls.is_empty());
    }

    #[test]
    fn accumulates_streamed_tool_calls() {
        let (client, _) = client_with(TOOL_STREAM, ToolMode::Native);
        let turn = client
            .complete("sys", &[], &[league_spec()], &mut |_| {})
            .expect("completes");
        assert_eq!(
            turn.tool_calls,
            vec![ToolCallRequest {
                id: "call_9".to_owned(),
                name: "league".to_owned(),
                args_json: r#"{"game":"poe1"}"#.to_owned(),
            }]
        );
    }

    #[test]
    fn indexless_complete_fragments_stay_separate_calls() {
        let (client, _) = client_with(INDEXLESS_TWO_CALLS, ToolMode::Native);
        let turn = client
            .complete("sys", &[], &[league_spec()], &mut |_| {})
            .expect("completes");
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].name, "league");
        assert_eq!(turn.tool_calls[0].args_json, r#"{"game":"poe1"}"#);
        assert_eq!(turn.tool_calls[1].name, "echo");
        assert_eq!(turn.tool_calls[1].args_json, r#"{"x":1}"#);
    }

    #[test]
    fn huge_index_appends_instead_of_allocating() {
        let stream = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":4000000000,\"id\":\"z\",\"function\":{\"name\":\"league\",\"arguments\":\"{}\"}}]}}]}\n\
data: [DONE]\n";
        let (client, _) = client_with(stream, ToolMode::Native);
        let turn = client
            .complete("sys", &[], &[league_spec()], &mut |_| {})
            .expect("completes without allocating billions of slots");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "league");
    }

    #[test]
    fn too_many_streamed_tool_calls_fail() {
        let mut stream = String::new();
        for index in 0..40 {
            let _ = writeln!(
                stream,
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"id\":\"c{index}\",\"function\":{{\"name\":\"t{index}\",\"arguments\":\"{{}}\"}}}}]}}}}]}}"
            );
        }
        let leaked: &'static str = Box::leak(stream.into_boxed_str());
        let (client, _) = client_with(leaked, ToolMode::Native);
        let err = client
            .complete("sys", &[], &[], &mut |_| {})
            .expect_err("must cap");
        assert!(err.to_string().contains("more than"));
    }

    #[test]
    fn finish_reason_length_is_an_error() {
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"par\"},\"finish_reason\":null}]}\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\
data: [DONE]\n";
        let (client, _) = client_with(stream, ToolMode::Native);
        let err = client
            .complete("sys", &[], &[], &mut |_| {})
            .expect_err("truncation is an error");
        assert!(err.to_string().contains("truncated"));
    }

    #[test]
    fn plain_completion_fallback_when_endpoint_ignores_stream() {
        let body = "{\"choices\":[{\"message\":{\"content\":\"plain answer\",\"tool_calls\":[{\"id\":\"t1\",\"function\":{\"name\":\"league\",\"arguments\":\"{\\\"game\\\":\\\"poe2\\\"}\"}}]}}]}";
        let leaked: &'static str = Box::leak(body.to_owned().into_boxed_str());
        let (client, _) = client_with(leaked, ToolMode::Native);
        let mut tokens = Vec::new();
        let turn = client
            .complete("sys", &[], &[], &mut |token| tokens.push(token.to_owned()))
            .expect("fallback parses");
        assert_eq!(tokens, vec!["plain answer"]);
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].args_json, r#"{"game":"poe2"}"#);
    }

    #[test]
    fn empty_non_sse_response_is_an_error() {
        let (client, _) = client_with("<html>captive portal</html>", ToolMode::Native);
        let err = client
            .complete("sys", &[], &[], &mut |_| {})
            .expect_err("not a completion");
        assert!(err.to_string().contains("not a chat completion"));
    }

    #[test]
    fn server_error_chunks_fail_the_turn() {
        let (client, _) = client_with(
            "data: {\"error\":{\"message\":\"model not found\"}}\n",
            ToolMode::Native,
        );
        let err = client
            .complete("sys", &[], &[], &mut |_| {})
            .expect_err("fails");
        assert!(err.to_string().contains("model not found"));
    }

    #[test]
    fn request_carries_url_auth_prompt_tools_and_replay() {
        let (client, captured) = client_with(TEXT_STREAM, ToolMode::Native);
        let transcript = vec![
            Message::User("what league?".to_owned()),
            Message::ToolCall {
                id: "call_1".to_owned(),
                name: "league".to_owned(),
                args_json: r#"{"game":"poe1"}"#.to_owned(),
            },
            Message::ToolResult {
                id: "call_1".to_owned(),
                name: "league".to_owned(),
                result_json: r#"{"ok":true}"#.to_owned(),
            },
            Message::ToolFailure {
                id: "call_2".to_owned(),
                name: "league".to_owned(),
                error: r#"invalid arguments: unknown game "poe3""#.to_owned(),
            },
        ];
        client
            .complete(
                "the system prompt",
                &transcript,
                &[league_spec()],
                &mut |_| {},
            )
            .expect("completes");

        let requests = captured.lock().expect("lock");
        let (url, api_key, body) = &requests[0];
        assert_eq!(url, "http://model.invalid/v1/chat/completions");
        assert_eq!(api_key.as_deref(), Some("secret-key"));

        let body: Value = serde_json::from_str(body).expect("body is JSON");
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["tools"][0]["function"]["name"], "league");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "the system prompt");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        // The failure content must be *valid JSON* despite quotes in the
        // error — regression guard for hand-rolled escaping.
        let failure_content: Value =
            serde_json::from_str(messages[4]["content"].as_str().expect("content string"))
                .expect("failure content is valid JSON");
        assert_eq!(
            failure_content["error"],
            r#"invalid arguments: unknown game "poe3""#
        );
    }

    #[test]
    fn prompted_request_has_no_tools_field_and_renders_transcript_as_text() {
        let (client, captured) = client_with(TEXT_STREAM, ToolMode::Prompted);
        let transcript = vec![
            Message::User("q".to_owned()),
            Message::ToolCall {
                id: "prompted-1".to_owned(),
                name: "league".to_owned(),
                args_json: r#"{"game":"poe2"}"#.to_owned(),
            },
            Message::ToolResult {
                id: "prompted-1".to_owned(),
                name: "league".to_owned(),
                result_json: r#"{"ok":true}"#.to_owned(),
            },
        ];
        client
            .complete("sys", &transcript, &[league_spec()], &mut |_| {})
            .expect("completes");

        let requests = captured.lock().expect("lock");
        let body: Value = serde_json::from_str(&requests[0].2).expect("body is JSON");
        assert!(body.get("tools").is_none(), "prompted mode sends no tools");
        let system = body["messages"][0]["content"].as_str().expect("system");
        assert!(system.contains("To call a tool"));
        assert!(system.contains("- league: Resolve leagues."));
        let call_message = body["messages"][2]["content"].as_str().expect("rendered");
        let rendered: Value = serde_json::from_str(call_message).expect("valid JSON");
        assert_eq!(rendered["tool_call"]["name"], "league");
        assert!(
            body["messages"][3]["content"]
                .as_str()
                .expect("result")
                .starts_with("Tool result (league):")
        );
    }

    #[test]
    fn prompted_mode_parses_tool_call_without_streaming_it() {
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"tool_call\\\": {\\\"name\\\": \\\"league\\\", \\\"arguments\\\": {\\\"game\\\": \\\"poe2\\\"}}}\"}}]}\n\
data: [DONE]\n";
        let (client, _) = client_with(stream, ToolMode::Prompted);
        let mut tokens = Vec::new();
        let turn = client
            .complete("sys", &[], &[league_spec()], &mut |token| {
                tokens.push(token.to_owned());
            })
            .expect("completes");
        assert!(tokens.is_empty(), "tool-call JSON must not stream as chat");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "league");
        assert_eq!(turn.tool_calls[0].args_json, r#"{"game":"poe2"}"#);
    }

    #[test]
    fn prompted_mode_plain_text_is_emitted_once() {
        let (client, _) = client_with(TEXT_STREAM, ToolMode::Prompted);
        let mut tokens = Vec::new();
        let turn = client
            .complete("sys", &[], &[league_spec()], &mut |token| {
                tokens.push(token.to_owned());
            })
            .expect("completes");
        assert_eq!(tokens, vec!["Hello"]);
        assert_eq!(turn.text, "Hello");
    }

    #[test]
    fn prompted_extraction_handles_think_blocks_and_prose() {
        let thinking = "<think>The user wants leagues, I should call the league tool with \
                        {\"game\":\"poe2\"}...</think>\n{\"tool_call\": {\"name\": \"league\", \
                        \"arguments\": {\"game\": \"poe2\"}}}";
        let call = extract_prompted_tool_call(thinking).expect("parses past think block");
        assert_eq!(call.name, "league");
        assert_eq!(call.args_json, r#"{"game":"poe2"}"#);

        let prose = "Sure! I'll check that: {\"tool_call\": {\"name\": \"league\", \
                     \"arguments\": {}}} — one moment.";
        let call = extract_prompted_tool_call(prose).expect("finds embedded object");
        assert_eq!(call.name, "league");

        let unterminated = "<think>never closed";
        assert!(extract_prompted_tool_call(unterminated).is_none());
    }

    #[test]
    fn idle_timeout_fires_on_silence_but_not_on_slow_streams() {
        use std::io::Read as _;

        /// Yields one chunk, then blocks forever.
        struct StallAfterFirst {
            served: bool,
        }
        impl Read for StallAfterFirst {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.served {
                    std::thread::park();
                    unreachable!("parked forever");
                }
                self.served = true;
                buf[0] = b'x';
                Ok(1)
            }
        }

        let mut reader = IdleTimeoutReader::new(
            Box::new(StallAfterFirst { served: false }),
            Duration::from_millis(50),
        );
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).expect("first chunk arrives"), 1);
        let err = reader.read(&mut buf).expect_err("silence must time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("idle timeout"));

        // A normal finite stream passes through untouched.
        let mut ok_reader = IdleTimeoutReader::new(
            Box::new(std::io::Cursor::new(b"hello".to_vec())),
            Duration::from_millis(50),
        );
        let mut out = String::new();
        ok_reader.read_to_string(&mut out).expect("reads to end");
        assert_eq!(out, "hello");
    }

    #[test]
    fn prompted_extraction_tolerates_code_fences() {
        let fenced = "```json\n{\"tool_call\": {\"name\": \"league\", \"arguments\": {}}}\n```";
        let call = extract_prompted_tool_call(fenced).expect("parses");
        assert_eq!(call.name, "league");
        assert_eq!(call.args_json, "{}");
        assert!(extract_prompted_tool_call("just some prose").is_none());
    }
}

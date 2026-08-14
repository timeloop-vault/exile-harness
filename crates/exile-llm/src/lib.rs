//! OpenAI-compatible chat-completions client for the harness.
//!
//! vLLM, Ollama, and `OpenRouter` all speak this protocol, so one hand-written
//! client with a configurable base URL covers local inference and hosted
//! fallback with zero provider abstractions. Supports native tool calls plus
//! a prompted tool-call fallback for models with unreliable function calling.
//!
//! The client lands in milestone 4.

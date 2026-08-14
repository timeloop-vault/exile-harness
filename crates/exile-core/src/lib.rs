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
//! The agent loop and event types land in milestone 2.

//! Contract between the harness and its tools: the `Tool` trait, the tool
//! registry, and JSON-schema parameter types.
//!
//! Every capability (league resolver, game-data lookup, wiki retrieval,
//! Path of Building math) implements this contract as a lib crate with a
//! thin CLI wrapper — callable in-process by the harness and standalone for
//! manual testing. Tools take a `game` parameter (`poe1` | `poe2`) and stamp
//! results with `source` + `fetched_at`.
//!
//! The trait and registry land in milestone 2.

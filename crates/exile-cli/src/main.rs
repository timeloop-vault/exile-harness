//! `exile` — CLI frontend for the exile harness.
//!
//! Frontend #1: a thin shell over `exile-core` that renders the harness
//! event stream to a terminal. The REPL lands in milestone 2.

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    println!("exile {version} — harness REPL lands in milestone 2");
}

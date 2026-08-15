//! Shared runtime plumbing for harness tools.
//!
//! Tools implement the `exile-tool-api` contract; this crate carries what
//! their implementations share, so it is written once instead of drifting
//! per copy: the [`HttpGet`] abstraction with the live [`UreqHttp`] client
//! (project User-Agent, request timeout), UTC timestamps for `fetched_at`
//! stamps, the shared [`Game`] parameter type (project law 3), URL
//! [`percent_encode`], and reusable HTTP test doubles in [`testing`].
//!
//! Deliberately separate from `exile-tool-api`: the contract crate stays
//! dependency-free (`exile-core` depends on it and must carry no I/O),
//! while this crate owns the heavier runtime deps (ureq, jiff) that only
//! tool implementations need.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

/// User-Agent for all outbound requests: poe.ninja and GGG both require a
/// descriptive UA; the repo URL is the contact channel.
pub const USER_AGENT: &str = "exile-harness/0.1 (+https://github.com/timeloop-vault/exile-harness)";

/// Which game a tool call targets. Project law 3: Path of Exile 1 and 2
/// are first-class and separate — every game-scoped tool takes this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Game {
    /// Path of Exile 1.
    #[serde(rename = "poe1")]
    Poe1,
    /// Path of Exile 2.
    #[serde(rename = "poe2")]
    Poe2,
}

impl Game {
    /// The lowercase API identifier (`poe1` | `poe2`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Poe1 => "poe1",
            Self::Poe2 => "poe2",
        }
    }
}

impl fmt::Display for Game {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Percent-encode a string for use inside a URL query value (RFC 3986
/// unreserved characters pass through; everything else is `%XX`-encoded,
/// including spaces).
#[must_use]
pub fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            other => {
                let _ = fmt::Write::write_fmt(&mut out, format_args!("%{other:02X}"));
            }
        }
    }
    out
}

/// Minimal HTTP-GET abstraction so tools can be tested with canned
/// responses and never touch the network in unit tests.
pub trait HttpGet: Send + Sync {
    /// Fetch `url`, returning the response body on success.
    fn get(&self, url: &str) -> Result<String, String>;
}

/// Live HTTP via ureq, with the project User-Agent and a request timeout.
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    /// Build the live client.
    #[must_use]
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .user_agent(USER_AGENT)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpGet for UreqHttp {
    fn get(&self, url: &str) -> Result<String, String> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|err| describe_get_error(url, &err))?;
        response
            .body_mut()
            .read_to_string()
            .map_err(|err| format!("reading body of {url} failed: {err}"))
    }
}

/// Render a request error, normalizing HTTP status failures to the
/// stable `http status: NNN` form. Downstream code matches on that
/// substring (e.g. the price tool's 404 endpoint fallback), so the
/// format is a contract pinned here and by test — not an accident of
/// ureq's error `Display`.
fn describe_get_error(url: &str, err: &ureq::Error) -> String {
    match err {
        ureq::Error::StatusCode(code) => format!("GET {url} failed: http status: {code}"),
        other => format!("GET {url} failed: {other}"),
    }
}

/// TTL response cache over an [`HttpGet`], for endpoints whose data is
/// server-cached anyway (e.g. poe.ninja's ~5-minute economy snapshots):
/// repeat lookups within the window are served locally, which is both
/// faster and the polite client behavior the API docs ask for.
/// Successful bodies are cached per URL; errors are never cached. Expired
/// entries are evicted on insert, bounding the cache to URLs fetched
/// within one TTL window (bodies run to hundreds of kilobytes).
pub struct CachedHttp {
    inner: Box<dyn HttpGet>,
    ttl: Duration,
    entries: std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>,
}

impl CachedHttp {
    /// Wrap `inner` with a per-URL TTL cache.
    #[must_use]
    pub fn new(inner: Box<dyn HttpGet>, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl HttpGet for CachedHttp {
    fn get(&self, url: &str) -> Result<String, String> {
        // Poisoning is recovered, not propagated: the map only ever holds
        // complete (Instant, body) pairs, so the worst a panicking peer
        // leaves behind is a valid cache we can keep using.
        {
            let entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((stored_at, body)) = entries.get(url)
                && stored_at.elapsed() < self.ttl
            {
                return Ok(body.clone());
            }
        }
        let body = self.inner.get(url)?;
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, (stored_at, _)| stored_at.elapsed() < self.ttl);
        entries.insert(url.to_owned(), (std::time::Instant::now(), body.clone()));
        Ok(body)
    }
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`, for `fetched_at` stamps.
#[must_use]
pub fn now_utc() -> String {
    jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// HTTP test doubles for tool crates' unit tests. Compiled into the normal
/// build so downstream `#[cfg(test)]` code can import them; never used on
/// live paths.
pub mod testing {
    use super::HttpGet;

    /// Serves canned bodies by URL substring; unmatched URLs fail.
    pub struct FakeHttp {
        /// `(url substring, response body)` pairs, first match wins.
        pub routes: Vec<(&'static str, &'static str)>,
    }

    impl HttpGet for FakeHttp {
        fn get(&self, url: &str) -> Result<String, String> {
            self.routes
                .iter()
                .find(|(fragment, _)| url.contains(fragment))
                .map(|(_, body)| (*body).to_owned())
                .ok_or_else(|| format!("GET {url} failed: no route"))
        }
    }

    /// Fails every request, connection-refused style.
    pub struct FailHttp;

    impl HttpGet for FailHttp {
        fn get(&self, url: &str) -> Result<String, String> {
            Err(format!("GET {url} failed: connection refused"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FailHttp, FakeHttp};
    use super::{HttpGet, now_utc};

    #[test]
    fn now_utc_is_iso_like() {
        let stamp = now_utc();
        assert_eq!(stamp.len(), 20, "unexpected stamp: {stamp}");
        assert!(stamp.ends_with('Z'));
        assert_eq!(stamp.as_bytes()[10], b'T');
    }

    #[test]
    fn fake_http_routes_by_substring() {
        let fake = FakeHttp {
            routes: vec![("example.com", "hello")],
        };
        assert_eq!(fake.get("https://example.com/x"), Ok("hello".to_owned()));
        assert!(fake.get("https://other.net/").is_err());
    }

    #[test]
    fn fail_http_always_fails() {
        assert!(FailHttp.get("https://example.com").is_err());
    }

    #[test]
    fn status_errors_have_a_stable_format() {
        // Contract test: the price tool's 404 endpoint fallback matches
        // this exact substring.
        assert_eq!(
            super::describe_get_error("https://x/y", &ureq::Error::StatusCode(404)),
            "GET https://x/y failed: http status: 404"
        );
    }

    #[test]
    fn cached_http_serves_repeats_and_never_caches_errors() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        struct Counting {
            calls: Arc<AtomicUsize>,
            fail: bool,
        }
        impl HttpGet for Counting {
            fn get(&self, url: &str) -> Result<String, String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if self.fail {
                    Err("down".to_owned())
                } else {
                    Ok(format!("body for {url}"))
                }
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let cache = super::CachedHttp::new(
            Box::new(Counting {
                calls: Arc::clone(&calls),
                fail: false,
            }),
            Duration::from_mins(1),
        );
        assert!(cache.get("https://x/a").is_ok());
        assert!(cache.get("https://x/a").is_ok());
        assert!(cache.get("https://x/b").is_ok());
        // Two distinct URLs → two upstream calls; the repeat was cached.
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let failures = Arc::new(AtomicUsize::new(0));
        let cache = super::CachedHttp::new(
            Box::new(Counting {
                calls: Arc::clone(&failures),
                fail: true,
            }),
            Duration::from_mins(1),
        );
        assert!(cache.get("https://x/a").is_err());
        assert!(cache.get("https://x/a").is_err());
        // Errors are never cached: both attempts hit upstream.
        assert_eq!(failures.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cached_http_evicts_expired_entries_on_insert() {
        use std::time::Duration;
        struct Ok200;
        impl HttpGet for Ok200 {
            fn get(&self, _url: &str) -> Result<String, String> {
                Ok("body".to_owned())
            }
        }

        // Zero TTL: every entry is expired by the next insert, so the map
        // never holds more than the entry just written.
        let cache = super::CachedHttp::new(Box::new(Ok200), Duration::ZERO);
        for url in ["https://x/a", "https://x/b", "https://x/c"] {
            assert!(cache.get(url).is_ok());
            assert_eq!(cache.entries.lock().expect("lock").len(), 1);
        }
    }

    #[test]
    fn percent_encoding_covers_reserved_and_utf8() {
        assert_eq!(super::percent_encode("plain-safe_1.0~"), "plain-safe_1.0~");
        assert_eq!(super::percent_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(super::percent_encode("Kitava's"), "Kitava%27s");
        assert_eq!(super::percent_encode("é"), "%C3%A9");
    }

    #[test]
    fn game_ids_are_stable() {
        assert_eq!(super::Game::Poe1.to_string(), "poe1");
        assert_eq!(super::Game::Poe2.as_str(), "poe2");
    }
}

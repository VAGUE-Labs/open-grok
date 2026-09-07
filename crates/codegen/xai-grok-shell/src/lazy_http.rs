use std::sync::Arc;
use std::sync::OnceLock;

/// A `reqwest::Client` whose construction is deferred to first use.
///
/// Building a client loads the OS trust store. On macOS that runs
/// `SecTrustSettingsCopyCertificates`, which can take seconds — and
/// intermittently much longer while CoreFoundation resolves preferences —
/// so constructing one eagerly inside `ModelsManager` (built for every
/// session actor, including tests with tight deadlines) stalls actor
/// startup behind keychain I/O the actor may never need. Deferring keeps
/// real network fetches on a fully configured client with native roots
/// while making construction cheap everywhere else.
#[derive(Clone, Debug, Default)]
pub(crate) struct LazyHttpClient {
    inner: Arc<OnceLock<reqwest::Client>>,
}

impl LazyHttpClient {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The shared client, built on first call.
    pub(crate) fn client(&self) -> &reqwest::Client {
        self.inner.get_or_init(reqwest::Client::new)
    }
}

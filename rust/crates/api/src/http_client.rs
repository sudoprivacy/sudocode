use std::time::Duration;

use crate::error::ApiError;

const HTTP_PROXY_KEYS: [&str; 2] = ["HTTP_PROXY", "http_proxy"];
const HTTPS_PROXY_KEYS: [&str; 2] = ["HTTPS_PROXY", "https_proxy"];
const NO_PROXY_KEYS: [&str; 2] = ["NO_PROXY", "no_proxy"];

const CONNECT_TIMEOUT_ENV: &str = "SUDOCODE_API_CONNECT_TIMEOUT";
const READ_TIMEOUT_ENV: &str = "SUDOCODE_API_READ_TIMEOUT";
const POOL_IDLE_TIMEOUT_ENV: &str = "SUDOCODE_API_POOL_IDLE_TIMEOUT";
const TCP_KEEPALIVE_ENV: &str = "SUDOCODE_API_TCP_KEEPALIVE";

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(300);
/// How long we may keep an unused connection before discarding it.
///
/// This has to stay below the shortest idle timeout anywhere on the path, and
/// a client cannot discover what that is — so the default is set from a
/// measurement and left conservative. Against `api.sudorouter.ai`, reusing an
/// idle kept-alive connection succeeds after a 45s pause and fails *instantly*
/// after 55s with `Remote end closed connection without response`; the peer
/// hangs up somewhere around 50s. hyper's own default is 90 seconds, i.e.
/// wider than that window, which is exactly how a client ends up writing a
/// request onto a socket the peer closed forty seconds ago.
///
/// 20s leaves room for a peer stricter than the one measured, and the cost is
/// at most one extra TCP+TLS handshake per 20s of genuine idleness — paid only
/// when the alternative was a dead socket anyway.
const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// TCP keepalive probe interval.
///
/// Insurance rather than a fix, and worth naming as such: keepalive probes put
/// packets on a connection that is established but quiet, which resets the
/// timers of stateful middleboxes (NAT tables, TCP relays) counting silent
/// seconds. They do *not* reset an application-layer idle timer, so they
/// cannot be relied on to rescue a request that is waiting on a slow first
/// byte — that is what streaming is for.
const DEFAULT_TCP_KEEPALIVE: Duration = Duration::from_secs(15);

/// Timeout configuration for the outbound HTTP client.
///
/// Deliberately *not* a whole-request timeout: streaming responses may
/// legitimately run for many minutes while tokens arrive, so an overall
/// deadline would kill long answers. Instead we bound the three ways a dead
/// connection can hang or fail a request: establishing the TCP connection,
/// waiting for the next byte to arrive, and reusing a pooled connection the
/// peer has already closed.
///
/// A `None` field disables that timeout entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutConfig {
    /// Maximum time to establish a TCP connection. Defaults to 30 seconds.
    pub connect_timeout: Option<Duration>,
    /// Maximum time between successive reads from the socket (an idle
    /// timeout, reset every time a byte arrives — safe for long streaming
    /// responses). Defaults to 300 seconds, which comfortably exceeds the
    /// keep-alive ping cadence of streaming providers while still unblocking
    /// a session stuck on a dead connection.
    pub read_timeout: Option<Duration>,
    /// How long an unused connection may sit in the pool before we drop it.
    /// Defaults to 20 seconds; see [`DEFAULT_POOL_IDLE_TIMEOUT`] for why that
    /// number and not hyper's 90. `None` keeps connections until the peer or
    /// the OS ends them, which is the behaviour that produced the failure the
    /// default exists to prevent.
    pub pool_idle_timeout: Option<Duration>,
    /// TCP keepalive probe interval for established connections. Defaults to
    /// 15 seconds; `None` leaves the OS default (typically hours, i.e. off in
    /// practice).
    pub tcp_keepalive: Option<Duration>,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Some(DEFAULT_CONNECT_TIMEOUT),
            read_timeout: Some(DEFAULT_READ_TIMEOUT),
            pool_idle_timeout: Some(DEFAULT_POOL_IDLE_TIMEOUT),
            tcp_keepalive: Some(DEFAULT_TCP_KEEPALIVE),
        }
    }
}

impl TimeoutConfig {
    /// Read timeout settings from the process environment.
    /// - `SUDOCODE_API_CONNECT_TIMEOUT` — connect timeout in whole seconds
    /// - `SUDOCODE_API_READ_TIMEOUT` — idle read timeout in whole seconds
    /// - `SUDOCODE_API_POOL_IDLE_TIMEOUT` — how long to keep an idle pooled
    ///   connection, in whole seconds
    /// - `SUDOCODE_API_TCP_KEEPALIVE` — TCP keepalive probe interval, in whole
    ///   seconds
    ///
    /// A value of `0` disables that timeout. Unset or unparseable values
    /// fall back to the defaults.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    fn from_lookup<F>(mut lookup: F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        let mut parse = |key: &str, default: Duration| -> Option<Duration> {
            match lookup(key).and_then(|value| value.trim().parse::<u64>().ok()) {
                Some(0) => None,
                Some(seconds) => Some(Duration::from_secs(seconds)),
                None => Some(default),
            }
        };
        Self {
            connect_timeout: parse(CONNECT_TIMEOUT_ENV, DEFAULT_CONNECT_TIMEOUT),
            read_timeout: parse(READ_TIMEOUT_ENV, DEFAULT_READ_TIMEOUT),
            pool_idle_timeout: parse(POOL_IDLE_TIMEOUT_ENV, DEFAULT_POOL_IDLE_TIMEOUT),
            tcp_keepalive: parse(TCP_KEEPALIVE_ENV, DEFAULT_TCP_KEEPALIVE),
        }
    }

    /// Create from explicit second values. `0` disables that timeout.
    ///
    /// Connection-pool settings are intentionally not parameters: they protect
    /// against a property of the network path rather than of the caller, so a
    /// caller that only wants a short read timeout should not silently lose
    /// them. Override [`TimeoutConfig::pool_idle_timeout`] on the returned
    /// value if you really need to.
    #[must_use]
    pub fn from_seconds(connect_secs: u64, read_secs: u64) -> Self {
        let to_timeout = |seconds: u64| (seconds > 0).then(|| Duration::from_secs(seconds));
        Self {
            connect_timeout: to_timeout(connect_secs),
            read_timeout: to_timeout(read_secs),
            ..Self::default()
        }
    }
}

/// Snapshot of the proxy-related environment variables that influence the
/// outbound HTTP client. Captured up front so callers can inspect, log, and
/// test the resolved configuration without re-reading the process environment.
///
/// When `proxy_url` is set it acts as a single catch-all proxy for both
/// HTTP and HTTPS traffic, taking precedence over the per-scheme fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
    /// Optional unified proxy URL that applies to both HTTP and HTTPS.
    /// When set, this takes precedence over `http_proxy` and `https_proxy`.
    pub proxy_url: Option<String>,
}

impl ProxyConfig {
    /// Read proxy settings from the live process environment, honouring both
    /// the upper- and lower-case spellings used by curl, git, and friends.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Create a proxy configuration from a single URL that applies to both
    /// HTTP and HTTPS traffic. This is the config-file alternative to setting
    /// `HTTP_PROXY` and `HTTPS_PROXY` environment variables separately.
    #[must_use]
    pub fn from_proxy_url(url: impl Into<String>) -> Self {
        Self {
            proxy_url: Some(url.into()),
            ..Self::default()
        }
    }

    fn from_lookup<F>(mut lookup: F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self {
            http_proxy: first_non_empty(&HTTP_PROXY_KEYS, &mut lookup),
            https_proxy: first_non_empty(&HTTPS_PROXY_KEYS, &mut lookup),
            no_proxy: first_non_empty(&NO_PROXY_KEYS, &mut lookup),
            proxy_url: None,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.proxy_url.is_none() && self.http_proxy.is_none() && self.https_proxy.is_none()
    }
}

/// Build a `reqwest::Client` that honours the standard `HTTP_PROXY`,
/// `HTTPS_PROXY`, and `NO_PROXY` environment variables. When no proxy is
/// configured the client behaves identically to `reqwest::Client::new()`.
pub fn build_http_client() -> Result<reqwest::Client, ApiError> {
    build_http_client_with(&ProxyConfig::from_env())
}

/// Build a `reqwest::Client` from explicit [`ProxyConfig`] and
/// [`TimeoutConfig`]. Used by callers (and tests) that need to control both
/// proxy routing and timeout behaviour.
pub fn build_http_client_with_opts(
    config: &ProxyConfig,
    timeout: &TimeoutConfig,
) -> Result<reqwest::Client, ApiError> {
    let mut builder = reqwest::Client::builder().no_proxy();

    if let Some(connect_timeout) = timeout.connect_timeout {
        builder = builder.connect_timeout(connect_timeout);
    }
    if let Some(read_timeout) = timeout.read_timeout {
        builder = builder.read_timeout(read_timeout);
    }

    // The pool hands out an idle connection without probing it first, so a
    // connection the peer closed while it sat idle is only discovered when a
    // request is written onto it. That surfaces as
    // `client error (SendRequest): peer closed connection without sending TLS
    // close_notify` — no status code, returned instantly, and identically for
    // every retry, because each retry draws another equally stale socket from
    // the same pool. Expiring our own idle connections before the peer does is
    // the only defence a client has; nothing in the response tells us the
    // peer's limit.
    //
    // Passed as `Option` on purpose: reqwest takes `Into<Option<Duration>>`, so
    // `None` means "never expire" rather than "leave hyper's 90s default", and
    // `SUDOCODE_API_POOL_IDLE_TIMEOUT=0` therefore means what it says.
    builder = builder
        .pool_idle_timeout(timeout.pool_idle_timeout)
        .tcp_keepalive(timeout.tcp_keepalive);

    let no_proxy = config
        .no_proxy
        .as_deref()
        .and_then(reqwest::NoProxy::from_string);

    let (http_proxy_url, https_url) = match config.proxy_url.as_deref() {
        Some(unified) => (Some(unified), Some(unified)),
        None => (config.http_proxy.as_deref(), config.https_proxy.as_deref()),
    };

    if let Some(url) = https_url {
        let mut proxy = reqwest::Proxy::https(url)?;
        if let Some(filter) = no_proxy.clone() {
            proxy = proxy.no_proxy(Some(filter));
        }
        builder = builder.proxy(proxy);
    }

    if let Some(url) = http_proxy_url {
        let mut proxy = reqwest::Proxy::http(url)?;
        if let Some(filter) = no_proxy.clone() {
            proxy = proxy.no_proxy(Some(filter));
        }
        builder = builder.proxy(proxy);
    }

    Ok(builder.build()?)
}

/// Infallible counterpart to [`build_http_client`] for constructors that
/// historically returned `Self` rather than `Result<Self, _>`. When the proxy
/// configuration is malformed we fall back to a default client so that
/// callers retain the previous behaviour and the failure surfaces on the
/// first outbound request instead of at construction time.
#[must_use]
pub fn build_http_client_or_default() -> reqwest::Client {
    build_http_client().unwrap_or_else(|_| {
        // Proxy config was malformed — drop the proxy but keep the timeout
        // protection so a dead connection still cannot hang the session.
        build_http_client_with_opts(&ProxyConfig::default(), &TimeoutConfig::from_env())
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Build a `reqwest::Client` from an explicit [`ProxyConfig`]. Used by tests
/// and by callers that want to override process-level environment lookups.
/// Timeouts come from the environment ([`TimeoutConfig::from_env`]).
///
/// When `config.proxy_url` is set it overrides the per-scheme `http_proxy`
/// and `https_proxy` fields and is registered as both an HTTP and HTTPS
/// proxy so a single value can route every outbound request.
pub fn build_http_client_with(config: &ProxyConfig) -> Result<reqwest::Client, ApiError> {
    build_http_client_with_opts(config, &TimeoutConfig::from_env())
}

fn first_non_empty<F>(keys: &[&str], lookup: &mut F) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    keys.iter()
        .find_map(|key| lookup(key).filter(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::{
        build_http_client_with, build_http_client_with_opts, ProxyConfig, TimeoutConfig,
        DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT,
    };

    fn config_from_map(pairs: &[(&str, &str)]) -> ProxyConfig {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        ProxyConfig::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn proxy_config_is_empty_when_no_env_vars_are_set() {
        // given
        let config = config_from_map(&[]);

        // when
        let empty = config.is_empty();

        // then
        assert!(empty);
        assert_eq!(config, ProxyConfig::default());
    }

    #[test]
    fn proxy_config_reads_uppercase_http_https_and_no_proxy() {
        // given
        let pairs = [
            ("HTTP_PROXY", "http://proxy.internal:3128"),
            ("HTTPS_PROXY", "http://secure.internal:3129"),
            ("NO_PROXY", "localhost,127.0.0.1,.corp"),
        ];

        // when
        let config = config_from_map(&pairs);

        // then
        assert_eq!(
            config.http_proxy.as_deref(),
            Some("http://proxy.internal:3128")
        );
        assert_eq!(
            config.https_proxy.as_deref(),
            Some("http://secure.internal:3129")
        );
        assert_eq!(
            config.no_proxy.as_deref(),
            Some("localhost,127.0.0.1,.corp")
        );
        assert!(!config.is_empty());
    }

    #[test]
    fn proxy_config_falls_back_to_lowercase_keys() {
        // given
        let pairs = [
            ("http_proxy", "http://lower.internal:3128"),
            ("https_proxy", "http://lower-secure.internal:3129"),
            ("no_proxy", ".lower"),
        ];

        // when
        let config = config_from_map(&pairs);

        // then
        assert_eq!(
            config.http_proxy.as_deref(),
            Some("http://lower.internal:3128")
        );
        assert_eq!(
            config.https_proxy.as_deref(),
            Some("http://lower-secure.internal:3129")
        );
        assert_eq!(config.no_proxy.as_deref(), Some(".lower"));
    }

    #[test]
    fn proxy_config_prefers_uppercase_over_lowercase_when_both_set() {
        // given
        let pairs = [
            ("HTTP_PROXY", "http://upper.internal:3128"),
            ("http_proxy", "http://lower.internal:3128"),
        ];

        // when
        let config = config_from_map(&pairs);

        // then
        assert_eq!(
            config.http_proxy.as_deref(),
            Some("http://upper.internal:3128")
        );
    }

    #[test]
    fn proxy_config_treats_empty_strings_as_unset() {
        // given
        let pairs = [("HTTP_PROXY", ""), ("http_proxy", "")];

        // when
        let config = config_from_map(&pairs);

        // then
        assert!(config.http_proxy.is_none());
    }

    #[test]
    fn build_http_client_succeeds_when_no_proxy_is_configured() {
        // given
        let config = ProxyConfig::default();

        // when
        let result = build_http_client_with(&config);

        // then
        assert!(result.is_ok());
    }

    #[test]
    fn build_http_client_succeeds_with_valid_http_and_https_proxies() {
        // given
        let config = ProxyConfig {
            http_proxy: Some("http://proxy.internal:3128".to_string()),
            https_proxy: Some("http://secure.internal:3129".to_string()),
            no_proxy: Some("localhost,127.0.0.1".to_string()),
            proxy_url: None,
        };

        // when
        let result = build_http_client_with(&config);

        // then
        assert!(result.is_ok());
    }

    #[test]
    fn build_http_client_returns_http_error_for_invalid_proxy_url() {
        // given
        let config = ProxyConfig {
            http_proxy: None,
            https_proxy: Some("not a url".to_string()),
            no_proxy: None,
            proxy_url: None,
        };

        // when
        let result = build_http_client_with(&config);

        // then
        let error = result.expect_err("invalid proxy URL must be reported as a build failure");
        assert!(
            matches!(error, crate::error::ApiError::Http(_)),
            "expected ApiError::Http for invalid proxy URL, got: {error:?}"
        );
    }

    #[test]
    fn from_proxy_url_sets_unified_field_and_leaves_per_scheme_empty() {
        // given / when
        let config = ProxyConfig::from_proxy_url("http://unified.internal:3128");

        // then
        assert_eq!(
            config.proxy_url.as_deref(),
            Some("http://unified.internal:3128")
        );
        assert!(config.http_proxy.is_none());
        assert!(config.https_proxy.is_none());
        assert!(!config.is_empty());
    }

    #[test]
    fn build_http_client_succeeds_with_unified_proxy_url() {
        // given
        let config = ProxyConfig {
            proxy_url: Some("http://unified.internal:3128".to_string()),
            no_proxy: Some("localhost".to_string()),
            ..ProxyConfig::default()
        };

        // when
        let result = build_http_client_with(&config);

        // then
        assert!(result.is_ok());
    }

    #[test]
    fn proxy_url_takes_precedence_over_per_scheme_fields() {
        // given – both per-scheme and unified are set
        let config = ProxyConfig {
            http_proxy: Some("http://per-scheme.internal:1111".to_string()),
            https_proxy: Some("http://per-scheme.internal:2222".to_string()),
            no_proxy: None,
            proxy_url: Some("http://unified.internal:3128".to_string()),
        };

        // when – building succeeds (the unified URL is valid)
        let result = build_http_client_with(&config);

        // then
        assert!(result.is_ok());
    }

    #[test]
    fn build_http_client_returns_error_for_invalid_unified_proxy_url() {
        // given
        let config = ProxyConfig::from_proxy_url("not a url");

        // when
        let result = build_http_client_with(&config);

        // then
        assert!(
            matches!(result, Err(crate::error::ApiError::Http(_))),
            "invalid unified proxy URL should fail: {result:?}"
        );
    }

    fn timeouts_from_map(pairs: &[(&str, &str)]) -> TimeoutConfig {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        TimeoutConfig::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn default_pool_idle_timeout_stays_under_the_measured_close_threshold() {
        // given – the measurement this default exists to respect. Against
        // api.sudorouter.ai, reuse after a 45s idle gap succeeded and reuse
        // after 55s failed instantly, so the peer closes somewhere between.
        let longest_gap_that_survived = Duration::from_secs(45);
        let shortest_gap_that_died = Duration::from_secs(55);

        // when
        let pool_idle_timeout = TimeoutConfig::default()
            .pool_idle_timeout
            .expect("the default must expire idle connections, not keep them forever");

        // then – strictly under the surviving gap, not merely under the fatal
        // one: equal-to-45s would be betting that the peer we measured is the
        // strictest hop on every path, which is not something a client can know.
        assert!(
            pool_idle_timeout < longest_gap_that_survived,
            "pool idle timeout {pool_idle_timeout:?} must leave margin below the {longest_gap_that_survived:?} \
             gap that was observed to still work (peer closed by {shortest_gap_that_died:?})"
        );
    }

    #[test]
    fn timeout_config_defaults_when_no_env_vars_are_set() {
        // given / when
        let timeouts = timeouts_from_map(&[]);

        // then
        assert_eq!(timeouts, TimeoutConfig::default());
        assert!(timeouts.tcp_keepalive.is_some());
    }

    #[test]
    fn timeout_config_reads_pool_idle_and_keepalive_from_env() {
        // given
        let pairs = [
            ("SUDOCODE_API_POOL_IDLE_TIMEOUT", "7"),
            ("SUDOCODE_API_TCP_KEEPALIVE", "3"),
        ];

        // when
        let timeouts = timeouts_from_map(&pairs);

        // then
        assert_eq!(timeouts.pool_idle_timeout, Some(Duration::from_secs(7)));
        assert_eq!(timeouts.tcp_keepalive, Some(Duration::from_secs(3)));
        // untouched keys keep their defaults
        assert_eq!(timeouts.connect_timeout, Some(DEFAULT_CONNECT_TIMEOUT));
        assert_eq!(timeouts.read_timeout, Some(DEFAULT_READ_TIMEOUT));
    }

    #[test]
    fn zero_disables_pool_idle_timeout_and_keepalive() {
        // given – the escape hatch for a path where expiring connections is
        // the wrong trade (a peer that never closes, an expensive handshake).
        let pairs = [
            ("SUDOCODE_API_POOL_IDLE_TIMEOUT", "0"),
            ("SUDOCODE_API_TCP_KEEPALIVE", "0"),
        ];

        // when
        let timeouts = timeouts_from_map(&pairs);

        // then
        assert_eq!(timeouts.pool_idle_timeout, None);
        assert_eq!(timeouts.tcp_keepalive, None);
    }

    #[test]
    fn from_seconds_keeps_the_connection_pool_defaults() {
        // given / when – a caller tuning only connect and read behaviour
        let timeouts = TimeoutConfig::from_seconds(5, 1);

        // then – it must not silently inherit hyper's 90s pool idle timeout,
        // which is the setting that made stale-connection reuse possible.
        assert_eq!(timeouts.connect_timeout, Some(Duration::from_secs(5)));
        assert_eq!(timeouts.read_timeout, Some(Duration::from_secs(1)));
        assert_eq!(
            timeouts.pool_idle_timeout,
            TimeoutConfig::default().pool_idle_timeout
        );
        assert_eq!(
            timeouts.tcp_keepalive,
            TimeoutConfig::default().tcp_keepalive
        );
    }

    #[test]
    fn build_http_client_succeeds_with_pool_settings_disabled() {
        // given
        let timeouts = TimeoutConfig {
            pool_idle_timeout: None,
            tcp_keepalive: None,
            ..TimeoutConfig::default()
        };

        // when
        let result = build_http_client_with_opts(&ProxyConfig::default(), &timeouts);

        // then
        assert!(result.is_ok());
    }
}

//! Connection-lifetime policy (GH#28): PostgreSQL-compatible
//! `authentication_timeout`, `idle_session_timeout` and
//! `idle_in_transaction_session_timeout`, TCP keepalive on accepted sockets,
//! and the `max_connections` utilisation warning threshold.
//!
//! The policy is shared by the PostgreSQL TCP / Unix-socket listeners and the
//! MySQL listeners; the replication listener takes only the keepalive half
//! (a standby is legitimately idle between WAL segments).
//!
//! RULE (do not relax): a deadline is armed ONLY around an await that is
//! blocked on the peer, never around work. `read_exact`, a TLS accept and
//! `write_all` are not cancel-safe, so an expired deadline always means the
//! connection is CLOSED — no code path may keep using the stream afterwards.
//!
//! Units: every duration is PostgreSQL GUC syntax verbatim — a bare integer in
//! the parameter's PostgreSQL base unit (milliseconds for the two idle GUCs,
//! seconds for `authentication_timeout` and the keepalive timers) or an integer
//! followed by `us|ms|s|min|h|d`. `0` disables. The same token therefore works
//! in `SET`, `SHOW`, `config.toml` and on the command line, and is what
//! libpq-based tooling and poolers already parse.

use std::time::Duration;

/// What the connection is doing while the server waits on the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActivity {
    /// Pre-authentication: startup packet, password / SCRAM exchange, TLS.
    Authenticating,
    /// Authenticated, no transaction open, waiting for the next command.
    Idle,
    /// Authenticated, inside a transaction block (open or failed), waiting
    /// for the next command.
    IdleInTransaction,
    /// A statement is executing, a COPY stream is in flight, or a partially
    /// received message is being completed: never subject to a deadline.
    Busy,
}

/// TCP keepalive timers. `Duration::ZERO` / `0` on a field leaves that timer
/// at the operating-system default (PostgreSQL semantics for
/// `tcp_keepalives_idle` / `tcp_keepalives_interval` / `tcp_keepalives_count`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TcpKeepaliveSettings {
    /// Seconds of inactivity before the first probe (`tcp_keepalives_idle`).
    pub idle: Duration,
    /// Seconds between probes (`tcp_keepalives_interval`).
    pub interval: Duration,
    /// Probes before the connection is declared dead (`tcp_keepalives_count`).
    pub retries: u32,
}

/// The connection-lifetime policy for one listener.
#[derive(Debug, Clone)]
pub struct ConnectionTimeouts {
    /// Bound on the WHOLE handshake (startup packet, TLS, password / SCRAM).
    /// PostgreSQL default 60 s; `ZERO` disables (a Nano extension).
    pub authentication_timeout: Duration,
    /// Close an authenticated session idle outside a transaction for longer
    /// than this. `ZERO` = disabled (the PostgreSQL default).
    pub idle_session_timeout: Duration,
    /// Close a session idle INSIDE a transaction block for longer than this.
    /// `ZERO` = disabled (the PostgreSQL default).
    pub idle_in_transaction_session_timeout: Duration,
    /// `Some` enables `SO_KEEPALIVE` on every accepted socket (with the given
    /// timers, `0` = OS default); `None` leaves the socket untouched.
    pub tcp_keepalive: Option<TcpKeepaliveSettings>,
    /// Log a WARN when in-use connections reach this percentage of
    /// `max_connections`. `0` disables the warning.
    pub connection_warn_threshold_percent: u8,
}

impl Default for ConnectionTimeouts {
    /// PostgreSQL defaults: `authentication_timeout` 60 s, both idle timeouts
    /// disabled, keepalive on with OS timers, warn at 80 %.
    fn default() -> Self {
        Self {
            authentication_timeout: Duration::from_secs(60),
            idle_session_timeout: Duration::ZERO,
            idle_in_transaction_session_timeout: Duration::ZERO,
            tcp_keepalive: Some(TcpKeepaliveSettings::default()),
            connection_warn_threshold_percent: 80,
        }
    }
}

fn nonzero(d: Duration) -> Option<Duration> {
    if d.is_zero() {
        None
    } else {
        Some(d)
    }
}

impl ConnectionTimeouts {
    /// Every timeout disabled, no keepalive, no warning — the pre-GH#28
    /// behaviour. Used by in-process handlers that have no listener policy
    /// (embedders, in-crate wire tests); every server listener overrides it.
    pub fn disabled() -> Self {
        Self {
            authentication_timeout: Duration::ZERO,
            idle_session_timeout: Duration::ZERO,
            idle_in_transaction_session_timeout: Duration::ZERO,
            tcp_keepalive: None,
            connection_warn_threshold_percent: 0,
        }
    }

    /// Build the policy from the `[server]` config section. Infallible: it is
    /// called after [`crate::config::ServerConfig::validate_connection_policy`],
    /// and treats an unparsable string as that key's default.
    ///
    /// The deprecated `idle_timeout_secs` key is honoured as
    /// `idle_session_timeout` only when the new key is unset (`"0"`) AND the
    /// old one was explicitly changed from its untouched 300 s default — the
    /// default must not start closing sessions on upgrade.
    pub fn from_server_config(cfg: &crate::config::ServerConfig) -> Self {
        let defaults = Self::default();
        let auth_ms = parse_guc_duration_ms(&cfg.authentication_timeout, 1_000)
            .unwrap_or(defaults.authentication_timeout.as_millis() as u64);
        let mut idle_ms = parse_guc_duration_ms(&cfg.idle_session_timeout, 1).unwrap_or(0);
        if idle_ms == 0 && Self::legacy_idle_alias_in_effect(cfg) {
            idle_ms = cfg.idle_timeout_secs.saturating_mul(1_000);
        }
        let in_txn_ms = parse_guc_duration_ms(&cfg.idle_in_transaction_session_timeout, 1).unwrap_or(0);
        let ka_idle = parse_guc_duration_ms(&cfg.tcp_keepalives_idle, 1_000).unwrap_or(0);
        let ka_interval = parse_guc_duration_ms(&cfg.tcp_keepalives_interval, 1_000).unwrap_or(0);
        Self {
            authentication_timeout: Duration::from_millis(auth_ms),
            idle_session_timeout: Duration::from_millis(idle_ms),
            idle_in_transaction_session_timeout: Duration::from_millis(in_txn_ms),
            tcp_keepalive: Some(TcpKeepaliveSettings {
                idle: Duration::from_millis(ka_idle),
                interval: Duration::from_millis(ka_interval),
                retries: cfg.tcp_keepalives_count,
            }),
            connection_warn_threshold_percent: cfg.max_connections_warn_percent,
        }
    }

    /// True when the deprecated `[server] idle_timeout_secs` key is what
    /// drives `idle_session_timeout` (see [`Self::from_server_config`]); the
    /// caller logs the deprecation warning once.
    pub fn legacy_idle_alias_in_effect(cfg: &crate::config::ServerConfig) -> bool {
        let new_key_unset = parse_guc_duration_ms(&cfg.idle_session_timeout, 1).unwrap_or(0) == 0;
        new_key_unset
            && cfg.idle_timeout_secs != 0
            && cfg.idle_timeout_secs != crate::config::LEGACY_IDLE_TIMEOUT_SECS_DEFAULT
    }

    /// How long the server may wait on the peer in the given state before
    /// closing the connection. `None` = wait forever.
    ///
    /// * `Busy` is never bounded — a running statement is not idle.
    /// * `IdleInTransaction` takes the SHORTER of the two idle limits: a
    ///   session sitting in a transaction must never live longer than one
    ///   sitting idle (a divergence from PostgreSQL, which arms only the
    ///   in-transaction timer inside a block; this can only close sooner).
    pub fn read_deadline(&self, activity: SessionActivity) -> Option<Duration> {
        self.armed_timer(activity).map(|(_, budget)| budget)
    }

    /// GH#28 (candidate 2): WHICH timer is armed in `activity`, and its
    /// budget. The first element names the timer that fires when the budget
    /// runs out — `Idle` for `idle_session_timeout`, `IdleInTransaction` for
    /// `idle_in_transaction_session_timeout`, `Authenticating` for
    /// `authentication_timeout` — so the FATAL a client receives carries the
    /// SQLSTATE of the budget that actually expired (57P05 vs 25P03), not of
    /// the state the session happened to be in. Inside a transaction block
    /// the shorter of the two idle limits is armed (a tie goes to the
    /// in-transaction timer, PostgreSQL's own timer for that state); when
    /// only `idle_session_timeout` is configured, THAT is the timer that
    /// fires and 57P05 is the right answer. Same `None` rules as
    /// [`Self::read_deadline`], which is defined in terms of this.
    pub fn armed_timer(&self, activity: SessionActivity) -> Option<(SessionActivity, Duration)> {
        match activity {
            SessionActivity::Busy => None,
            SessionActivity::Authenticating => {
                nonzero(self.authentication_timeout).map(|d| (SessionActivity::Authenticating, d))
            }
            SessionActivity::Idle => nonzero(self.idle_session_timeout).map(|d| (SessionActivity::Idle, d)),
            SessionActivity::IdleInTransaction => {
                match (
                    nonzero(self.idle_session_timeout),
                    nonzero(self.idle_in_transaction_session_timeout),
                ) {
                    (Some(idle), Some(in_txn)) if idle < in_txn => Some((SessionActivity::Idle, idle)),
                    (Some(_), Some(in_txn)) => Some((SessionActivity::IdleInTransaction, in_txn)),
                    (Some(idle), None) => Some((SessionActivity::Idle, idle)),
                    (None, Some(in_txn)) => Some((SessionActivity::IdleInTransaction, in_txn)),
                    (None, None) => None,
                }
            }
        }
    }

    /// True when `in_use` of `max` connections is at or above the configured
    /// threshold. Pure: edge-triggering (one WARN per crossing) is the
    /// caller's job. Never divides; never fires for a `0` threshold, a `0`
    /// limit or an empty server.
    pub fn should_warn_utilisation(&self, in_use: usize, max: usize) -> bool {
        let threshold = self.connection_warn_threshold_percent as usize;
        threshold != 0 && max != 0 && in_use != 0 && in_use.saturating_mul(100) >= threshold.saturating_mul(max)
    }
}

/// Apply the per-socket options every accepted PostgreSQL / MySQL TCP socket
/// gets: `TCP_NODELAY` (always — low-latency responses) and, when the policy
/// asks for it, `SO_KEEPALIVE` so the kernel reaps half-open peers.
///
/// Callers log a WARN and continue on `Err`: a `setsockopt` failure must
/// never become a denial of service.
pub fn apply_socket_options(stream: &tokio::net::TcpStream, timeouts: &ConnectionTimeouts) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    apply_tcp_keepalive(stream, &timeouts.tcp_keepalive)
}

/// Enable `SO_KEEPALIVE` with the given timers (`0` = OS default). `None`
/// leaves the socket exactly as accepted — opting out must really opt out.
pub fn apply_tcp_keepalive(
    stream: &tokio::net::TcpStream,
    keepalive: &Option<TcpKeepaliveSettings>,
) -> std::io::Result<()> {
    let Some(ka) = keepalive else {
        return Ok(());
    };
    let mut params = socket2::TcpKeepalive::new();
    if !ka.idle.is_zero() {
        params = params.with_time(ka.idle);
    }
    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "illumos",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "windows",
    ))]
    {
        if !ka.interval.is_zero() {
            params = params.with_interval(ka.interval);
        }
    }
    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "illumos",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "windows",
    ))]
    {
        if ka.retries != 0 {
            params = params.with_retries(ka.retries);
        }
    }
    // Targets without per-probe control still honour SO_KEEPALIVE + idle.
    let _ = (ka.interval, ka.retries);
    socket2::SockRef::from(stream).set_tcp_keepalive(&params)
}

/// The units PostgreSQL accepts for a time GUC, as rendered in the HINT of an
/// `invalid value for parameter` error.
pub const GUC_DURATION_UNITS_HINT: &str =
    "Valid units for this parameter are \"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\".";

/// Parse PostgreSQL GUC duration syntax into milliseconds.
///
/// `raw` is a bare integer (interpreted in `bare_unit_ms`, the parameter's
/// PostgreSQL base unit) or an integer followed by `us|ms|s|min|h|d`
/// (whitespace between the two is allowed, as in PostgreSQL). Surrounding
/// quotes are stripped. `Err` carries the offending text. Sub-millisecond
/// values round UP so that a non-zero request never silently becomes `0`
/// (= disabled).
pub fn parse_guc_duration_ms(raw: &str, bare_unit_ms: u64) -> Result<u64, String> {
    let s = raw.trim().trim_matches('\'').trim_matches('"').trim();
    if s.is_empty() {
        return Err(raw.to_string());
    }
    let digits_end = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits_end == 0 {
        return Err(raw.to_string());
    }
    let (digits, unit) = s.split_at(digits_end);
    let value: u64 = digits.parse().map_err(|_| raw.to_string())?;
    let unit = unit.trim().to_ascii_lowercase();
    let ms = match unit.as_str() {
        "" => value.checked_mul(bare_unit_ms),
        "us" => Some(value.saturating_add(999) / 1_000),
        "ms" => Some(value),
        "s" => value.checked_mul(1_000),
        "min" => value.checked_mul(60_000),
        "h" => value.checked_mul(3_600_000),
        "d" => value.checked_mul(86_400_000),
        _ => return Err(raw.to_string()),
    };
    ms.ok_or_else(|| raw.to_string())
}

/// Render milliseconds the way PostgreSQL's `SHOW` does
/// (`convert_int_from_base_unit`): the largest unit that divides evenly.
/// `0` renders as `"0"`, `30_000` as `"30s"`, `600_000` as `"10min"`.
pub fn format_guc_duration_ms(ms: u64) -> String {
    if ms == 0 {
        return "0".to_string();
    }
    if ms % 86_400_000 == 0 {
        format!("{}d", ms / 86_400_000)
    } else if ms % 3_600_000 == 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms % 60_000 == 0 {
        format!("{}min", ms / 60_000)
    } else if ms % 1_000 == 0 {
        format!("{}s", ms / 1_000)
    } else {
        format!("{}ms", ms)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn duration_parser_accepts_postgresql_syntax() {
        assert_eq!(parse_guc_duration_ms("0", 1).unwrap(), 0);
        assert_eq!(parse_guc_duration_ms("30000", 1).unwrap(), 30_000);
        assert_eq!(parse_guc_duration_ms("60", 1_000).unwrap(), 60_000);
        assert_eq!(parse_guc_duration_ms("'30s'", 1).unwrap(), 30_000);
        assert_eq!(parse_guc_duration_ms("5min", 1).unwrap(), 300_000);
        assert_eq!(parse_guc_duration_ms("2 h", 1).unwrap(), 7_200_000);
        assert_eq!(parse_guc_duration_ms("1d", 1).unwrap(), 86_400_000);
        assert_eq!(parse_guc_duration_ms("1500us", 1).unwrap(), 2);
        assert!(parse_guc_duration_ms("banana", 1).is_err());
        assert!(parse_guc_duration_ms("30x", 1).is_err());
        assert!(parse_guc_duration_ms("", 1).is_err());
        assert!(parse_guc_duration_ms("-5", 1).is_err());
    }

    #[test]
    fn duration_formatter_matches_postgresql_show() {
        assert_eq!(format_guc_duration_ms(0), "0");
        assert_eq!(format_guc_duration_ms(30_000), "30s");
        assert_eq!(format_guc_duration_ms(45_000), "45s");
        assert_eq!(format_guc_duration_ms(60_000), "1min");
        assert_eq!(format_guc_duration_ms(600_000), "10min");
        assert_eq!(format_guc_duration_ms(3_600_000), "1h");
        assert_eq!(format_guc_duration_ms(1_500), "1500ms");
    }

    #[test]
    fn legacy_idle_timeout_secs_is_only_honoured_when_changed() {
        let mut cfg = crate::config::ServerConfig::default();
        assert!(!ConnectionTimeouts::legacy_idle_alias_in_effect(&cfg));
        assert_eq!(
            ConnectionTimeouts::from_server_config(&cfg).idle_session_timeout,
            Duration::ZERO
        );
        cfg.idle_timeout_secs = 120;
        assert!(ConnectionTimeouts::legacy_idle_alias_in_effect(&cfg));
        assert_eq!(
            ConnectionTimeouts::from_server_config(&cfg).idle_session_timeout,
            Duration::from_secs(120)
        );
        // The new key wins over the alias.
        cfg.idle_session_timeout = "10s".to_string();
        assert!(!ConnectionTimeouts::legacy_idle_alias_in_effect(&cfg));
        assert_eq!(
            ConnectionTimeouts::from_server_config(&cfg).idle_session_timeout,
            Duration::from_secs(10)
        );
    }

    #[test]
    fn disabled_policy_has_no_deadlines() {
        let off = ConnectionTimeouts::disabled();
        for a in [
            SessionActivity::Authenticating,
            SessionActivity::Idle,
            SessionActivity::IdleInTransaction,
            SessionActivity::Busy,
        ] {
            assert_eq!(off.read_deadline(a), None);
            assert_eq!(off.armed_timer(a), None);
        }
        assert!(off.tcp_keepalive.is_none());
        assert!(!off.should_warn_utilisation(100, 100));
    }

    /// GH#28 (c2): the timer that is reported on expiry is the one whose
    /// budget actually ran out, never merely the session's state.
    #[test]
    fn armed_timer_names_the_budget_that_fires() {
        use SessionActivity::*;
        let s = |idle_ms: u64, in_txn_ms: u64| ConnectionTimeouts {
            idle_session_timeout: Duration::from_millis(idle_ms),
            idle_in_transaction_session_timeout: Duration::from_millis(in_txn_ms),
            ..ConnectionTimeouts::disabled()
        };
        // Only idle_session_timeout configured: inside a block it is STILL the
        // idle-session budget that fires (57P05), not 25P03.
        assert_eq!(
            s(2_000, 0).armed_timer(IdleInTransaction),
            Some((Idle, Duration::from_millis(2_000)))
        );
        assert_eq!(
            s(2_000, 0).armed_timer(Idle),
            Some((Idle, Duration::from_millis(2_000)))
        );
        // Only the in-transaction timer: nothing armed outside a block.
        assert_eq!(
            s(0, 500).armed_timer(IdleInTransaction),
            Some((IdleInTransaction, Duration::from_millis(500)))
        );
        assert_eq!(s(0, 500).armed_timer(Idle), None);
        // Both set: the min-of-both rule names the shorter one.
        assert_eq!(
            s(2_000, 500).armed_timer(IdleInTransaction),
            Some((IdleInTransaction, Duration::from_millis(500)))
        );
        assert_eq!(
            s(300, 500).armed_timer(IdleInTransaction),
            Some((Idle, Duration::from_millis(300)))
        );
        // A tie goes to PostgreSQL's own timer for that state.
        assert_eq!(
            s(500, 500).armed_timer(IdleInTransaction),
            Some((IdleInTransaction, Duration::from_millis(500)))
        );
        // Busy is never armed; the budget matches read_deadline everywhere.
        assert_eq!(s(300, 500).armed_timer(Busy), None);
        for a in [Authenticating, Idle, IdleInTransaction, Busy] {
            assert_eq!(s(300, 500).read_deadline(a), s(300, 500).armed_timer(a).map(|(_, d)| d));
        }
    }
}

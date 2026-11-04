//! Loopback TCP ports for daemons a test spawns (sprinter ffe805264aac, spec R6
//! §2(c)). Included by path (`#[path = "support/ports.rs"] mod ports;`, or
//! `#[path = "ports.rs"]` from `physical_pair.rs`); it needs nothing but std and
//! `socket2`, so any test target can use it.
//!
//! # Why a reservation, and which one
//!
//! The old `free_port` bound `127.0.0.1:0`, read the port and dropped the
//! listener: the port was free again the moment it was returned, while the
//! daemon it was meant for binds it seconds later — a standby's HTTP listener
//! only after its bootstrap. Under a parallel `cargo test` any other
//! `bind(…:0)` in that window (another test's `free_port`, any daemon's
//! ephemeral listener) could be handed the same port; the daemon that came
//! second then ran without its HTTP endpoint (a failed HTTP bind is logged and
//! not fatal) and a test polling that port saw nothing at all, or another
//! test's daemon answering. Two `free_port()` calls in a row could even
//! return the same port.
//!
//! The choice (of the three the spec lists): HOLD THE RESERVATION. Each port is
//! a socket bound to `127.0.0.1:<port>` with `SO_REUSEADDR` that NEVER listens,
//! kept open for the life of the test process (they are few: a handful per
//! test). On Linux that is exactly what a daemon can bind over and nothing
//! else can be given:
//!
//! * the daemon's own `bind` + `listen` succeeds: every listener of this crate
//!   binds through std/tokio/mio, which set `SO_REUSEADDR`, and the kernel
//!   (`inet_csk_bind_conflict`) lets a `SO_REUSEADDR` socket bind and listen
//!   beside `SO_REUSEADDR` sockets that are not listening;
//! * no `bind(…:0)` — of this process or any other — is handed a reserved port
//!   (the automatic port search treats a bound socket on the address as a
//!   conflict), and no outgoing `connect` takes it as its local port (that
//!   search skips ports a `bind` owns);
//! * a daemon stopped and started again on the same port finds it still
//!   reserved, so a restart cannot lose its port to another test either;
//! * a socket that only binds does not listen: a connection to a reserved port
//!   whose daemon is not up yet is refused, as before; it is not listed in
//!   `/proc/net/tcp` (which lists listening and connected sockets), so the
//!   suites that read the daemon's listening socket from there see only the
//!   daemon's.
//!
//! The alternatives were weaker here: a per-process port range keyed by pid
//! still races every `bind(…:0)` in the ephemeral range (and two pids can map to
//! one range), and port 0 read back from the daemon would need a port file the
//! binary does not write for its HTTP, PostgreSQL and replication listeners.
//!
//! Linux only. On BSD and macOS binding an address another socket holds needs
//! `SO_REUSEPORT`, and on Windows mio does not set `SO_REUSEADDR` at all, so a
//! reservation there would make the daemon's own bind fail: those targets bind
//! and release as before, and keep the race.

#![allow(dead_code, clippy::expect_used, clippy::panic)]

#[cfg(target_os = "linux")]
use std::net::{Ipv4Addr, SocketAddr};
#[cfg(target_os = "linux")]
use std::sync::Mutex;

/// The reservations: open for the life of the test process. Each socket is
/// close-on-exec (`socket2` sets it), so no daemon a test spawns inherits one.
#[cfg(target_os = "linux")]
static RESERVED: Mutex<Vec<socket2::Socket>> = Mutex::new(Vec::new());

/// A loopback port no other `bind(…:0)` can be given while this process runs,
/// that a daemon (any `SO_REUSEADDR` listener) can bind (module documentation).
#[cfg(target_os = "linux")]
pub fn free_port() -> u16 {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
        .unwrap_or_else(|error| panic!("free_port: socket: {error}"));
    socket
        .set_reuse_address(true)
        .unwrap_or_else(|error| panic!("free_port: SO_REUSEADDR: {error}"));
    socket
        .bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())
        .unwrap_or_else(|error| panic!("free_port: bind 127.0.0.1:0: {error}"));
    let port = socket
        .local_addr()
        .ok()
        .and_then(|address| address.as_socket())
        .map(|address| address.port())
        .expect("free_port: the reserved socket has an IPv4 address");
    // Never listened on: the daemon given `port` is the one listener on it.
    RESERVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(socket);
    port
}

/// A loopback port that was free a moment ago (module documentation: no
/// reservation off Linux).
#[cfg(not(target_os = "linux"))]
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free_port: bind 127.0.0.1:0")
}

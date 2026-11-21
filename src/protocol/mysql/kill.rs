//! `KILL [CONNECTION | QUERY] id` for the MySQL listener.
//!
//! A MySQL connection's id (the thread id of its handshake, `CONNECTION_ID()`
//! in `SHOW PROCESSLIST`) is its session's backend id, the `pid` of
//! `pg_stat_activity`: one number names a session on both wires.
//!
//! `KILL QUERY` asks the statement the target is running to stop, through the
//! same request a PostgreSQL `CancelRequest` makes. `KILL CONNECTION` also
//! ends the target's connection; that is only in reach for MySQL-wire
//! sessions, which are the ones registered here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;

use super::vars::{strip_keyword, VarError};

/// What ends one MySQL connection from another.
#[derive(Debug)]
pub(crate) struct KillSwitch {
    engine_instance: u64,
    killed: AtomicBool,
    notify: tokio::sync::Notify,
}

impl KillSwitch {
    /// Ask the connection to end. It closes when it next waits for a command.
    pub(crate) fn kill(&self) {
        self.killed.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    pub(crate) fn is_killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }

    /// Resolves when the connection was asked to end.
    pub(crate) async fn killed(&self) {
        if self.is_killed() {
            return;
        }
        self.notify.notified().await;
    }
}

fn registry() -> &'static DashMap<i32, Arc<KillSwitch>> {
    static REGISTRY: OnceLock<DashMap<i32, Arc<KillSwitch>>> = OnceLock::new();
    REGISTRY.get_or_init(DashMap::new)
}

/// Register the MySQL connection whose session has backend id `pid`.
pub(crate) fn register(pid: i32, engine_instance: u64) -> Arc<KillSwitch> {
    let switch = Arc::new(KillSwitch {
        engine_instance,
        killed: AtomicBool::new(false),
        notify: tokio::sync::Notify::new(),
    });
    registry().insert(pid, Arc::clone(&switch));
    switch
}

/// Forget a connection that ended.
pub(crate) fn unregister(pid: i32) {
    registry().remove(&pid);
}

/// The switch of the live MySQL connection `pid` of this engine.
pub(crate) fn find(pid: i32, engine_instance: u64) -> Option<Arc<KillSwitch>> {
    registry()
        .get(&pid)
        .map(|entry| Arc::clone(entry.value()))
        .filter(|switch| switch.engine_instance == engine_instance)
}

/// What a `KILL` statement asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillTarget {
    Connection,
    Query,
}

/// Read `KILL [CONNECTION | QUERY] id`. `None`: not a `KILL` statement.
pub(crate) fn parse_kill(sql: &str) -> Option<Result<(KillTarget, i64), VarError>> {
    let rest = strip_keyword(sql.trim().trim_end_matches(';').trim(), "KILL")?;
    let (target, id_text) = if let Some(rest) = strip_keyword(rest, "QUERY") {
        (KillTarget::Query, rest)
    } else if let Some(rest) = strip_keyword(rest, "CONNECTION") {
        (KillTarget::Connection, rest)
    } else {
        (KillTarget::Connection, rest)
    };
    Some(
        id_text
            .trim()
            .parse::<i64>()
            .map(|id| (target, id))
            .map_err(|_| VarError::syntax(sql)),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn kill_is_read_in_its_three_forms() {
        assert_eq!(parse_kill("KILL 7").unwrap().unwrap(), (KillTarget::Connection, 7));
        assert_eq!(parse_kill("kill connection 8;").unwrap().unwrap(), (KillTarget::Connection, 8));
        assert_eq!(parse_kill("KILL QUERY 9").unwrap().unwrap(), (KillTarget::Query, 9));
        assert_eq!(parse_kill("KILL QUERY x").unwrap().unwrap_err().code, 1064);
        assert!(parse_kill("KILLER 1").is_none());
        assert!(parse_kill("SELECT 1").is_none());
    }

    #[tokio::test]
    async fn a_switch_is_found_only_for_its_engine_and_wakes_its_waiter() {
        let switch = register(-4242, 77);
        assert!(find(-4242, 78).is_none());
        let found = find(-4242, 77).unwrap();
        assert!(!found.is_killed());
        found.kill();
        switch.killed().await;
        assert!(switch.is_killed());
        unregister(-4242);
        assert!(find(-4242, 77).is_none());
    }
}

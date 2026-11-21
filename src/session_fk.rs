//! Foreign-key checking for one session.
//!
//! `SET foreign_key_checks = 0` on the MySQL wire turns the checks off for the
//! statements of the session that set it, and for no other: a dump restored on
//! one connection (tables in any order, children before their parents) does
//! not loosen what every other connection is held to. The MySQL handler keeps
//! the setting as a session value under [`FOREIGN_KEY_CHECKS`]; every
//! foreign-key decision of the engine reads its mode through
//! [`crate::EmbeddedDatabase::session_fk_mode`], which lets that value win over
//! the server-wide mode for the statement running on this thread.

use crate::sql::SettingValue;
use crate::{EmbeddedDatabase, FkValidationMode};

/// The session value that turns foreign-key checking off (`false`).
pub(crate) const FOREIGN_KEY_CHECKS: &str = "mysql.foreign_key_checks";

impl EmbeddedDatabase {
    /// The foreign-key mode for the statement running on this thread: off when
    /// its session turned the checks off, the server-wide mode otherwise.
    pub(crate) fn session_fk_mode(&self) -> FkValidationMode {
        if matches!(crate::session_guc_tls(FOREIGN_KEY_CHECKS), Some(SettingValue::Boolean(false))) {
            return FkValidationMode::Off;
        }
        *self.fk_validation_mode.read()
    }
}

//! CLI module for HeliosDB-Lite commands

pub mod dump;
pub mod import_export;
pub mod restore;

pub use dump::DumpCommand;
pub use restore::RestoreCommand;

/// An encrypted data directory opened without a configuration fails with a
/// message about `[encryption]`; on the offline `dump` / `restore` path the
/// way to supply it is `-c/--config`, so say so (GH#45).
pub(crate) fn name_config_option(err: crate::Error) -> crate::Error {
    match err {
        crate::Error::Encryption(msg) if msg.contains("written with encryption enabled") => {
            crate::Error::Encryption(format!(
                "{msg} Pass the same configuration file the server was started with using --config <file> \
                 (its [encryption] section and key_source supply the key)."
            ))
        }
        other => other,
    }
}

#![allow(dead_code)]

/// MySQL Compatibility Modes
///
/// Provides compatibility modes for different MySQL versions:
/// - MySQL 5.7 mode
/// - MySQL 8.0 mode
/// - Behavior differences between versions
use std::collections::HashSet;

/// MySQL version compatibility mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MySQLVersion {
    MySQL57,
    MySQL80,
}

impl MySQLVersion {
    pub fn from_string(version: &str) -> Self {
        if version.starts_with("5.7") || version.starts_with("5_7") {
            MySQLVersion::MySQL57
        } else {
            MySQLVersion::MySQL80
        }
    }

    pub fn version_string(&self) -> &'static str {
        match self {
            MySQLVersion::MySQL57 => "5.7.44-HeliosDB-Nano",
            MySQLVersion::MySQL80 => "8.0.35-HeliosDB-Nano",
        }
    }

    pub fn protocol_version(&self) -> u8 {
        10 // Both use protocol version 10
    }
}

/// Compatibility mode handler
#[derive(Debug, Clone)]
pub struct CompatibilityMode {
    version: MySQLVersion,
    sql_mode: SqlMode,
}

impl CompatibilityMode {
    pub fn new(version: MySQLVersion) -> Self {
        let sql_mode = match version {
            MySQLVersion::MySQL57 => SqlMode::mysql_57_default(),
            MySQLVersion::MySQL80 => SqlMode::mysql_80_default(),
        };

        Self { version, sql_mode }
    }

    pub fn version(&self) -> MySQLVersion {
        self.version
    }

    pub fn sql_mode(&self) -> &SqlMode {
        &self.sql_mode
    }

    pub fn set_sql_mode(&mut self, mode: SqlMode) {
        self.sql_mode = mode;
    }

    /// Check if feature is supported in current mode
    pub fn supports_feature(&self, feature: Feature) -> bool {
        match feature {
            Feature::WindowFunctions => match self.version {
                MySQLVersion::MySQL57 => false,
                MySQLVersion::MySQL80 => true,
            },
            Feature::CTERecursive => match self.version {
                MySQLVersion::MySQL57 => false,
                MySQLVersion::MySQL80 => true,
            },
            Feature::JSONTable => match self.version {
                MySQLVersion::MySQL57 => false,
                MySQLVersion::MySQL80 => true,
            },
            Feature::DescendingIndexes => match self.version {
                MySQLVersion::MySQL57 => false,
                MySQLVersion::MySQL80 => true,
            },
            Feature::InvisibleIndexes => match self.version {
                MySQLVersion::MySQL57 => false,
                MySQLVersion::MySQL80 => true,
            },
            Feature::RolesAndPrivileges => match self.version {
                MySQLVersion::MySQL57 => false,
                MySQLVersion::MySQL80 => true,
            },
            Feature::AtomicDDL => match self.version {
                MySQLVersion::MySQL57 => false,
                MySQLVersion::MySQL80 => true,
            },
            Feature::DefaultExpressions => match self.version {
                MySQLVersion::MySQL57 => false,
                MySQLVersion::MySQL80 => true,
            },
            Feature::CheckConstraints => match self.version {
                MySQLVersion::MySQL57 => false,
                MySQLVersion::MySQL80 => true,
            },
            _ => true, // Most features supported in both
        }
    }

    /// Get default authentication plugin
    pub fn default_auth_plugin(&self) -> &'static str {
        match self.version {
            MySQLVersion::MySQL57 => "mysql_native_password",
            MySQLVersion::MySQL80 => "caching_sha2_password",
        }
    }

    /// Get default character set
    pub fn default_charset(&self) -> &'static str {
        match self.version {
            MySQLVersion::MySQL57 => "latin1",
            MySQLVersion::MySQL80 => "utf8mb4",
        }
    }

    /// Get default collation
    pub fn default_collation(&self) -> &'static str {
        match self.version {
            MySQLVersion::MySQL57 => "latin1_swedish_ci",
            MySQLVersion::MySQL80 => "utf8mb4_0900_ai_ci",
        }
    }

    /// Check if zero dates are allowed
    pub fn allows_zero_dates(&self) -> bool {
        !self.sql_mode.modes.contains(&SqlModeFlag::NoZeroDate)
    }

    /// Check if zero in dates are allowed
    pub fn allows_zero_in_dates(&self) -> bool {
        !self.sql_mode.modes.contains(&SqlModeFlag::NoZeroInDate)
    }

    /// Check if division by zero produces NULL
    pub fn error_on_division_by_zero(&self) -> bool {
        self.sql_mode.modes.contains(&SqlModeFlag::ErrorForDivisionByZero)
    }
}

/// MySQL feature flags
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Feature {
    WindowFunctions,
    CTERecursive,
    JSONTable,
    DescendingIndexes,
    InvisibleIndexes,
    RolesAndPrivileges,
    AtomicDDL,
    DefaultExpressions,
    CheckConstraints,
    Partitioning,
    Triggers,
    StoredProcedures,
    Views,
    ForeignKeys,
}

/// SQL_MODE settings
#[derive(Debug, Clone)]
pub struct SqlMode {
    modes: HashSet<SqlModeFlag>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlModeFlag {
    AllowInvalidDates,
    AnsiQuotes,
    ErrorForDivisionByZero,
    HighNotPrecedence,
    IgnoreSpace,
    NoAutoValueOnZero,
    NoBackslashEscapes,
    NoEngineSubstitution,
    NoUnsignedSubtraction,
    NoZeroDate,
    NoZeroInDate,
    OnlyFullGroupBy,
    PadCharToFullLength,
    PipesAsConcat,
    RealAsFloat,
    StrictAllTables,
    StrictTransTables,
    TimeTruncateFractional,
}

impl SqlMode {
    pub fn new() -> Self {
        Self { modes: HashSet::new() }
    }

    /// MySQL 5.7 default SQL_MODE
    pub fn mysql_57_default() -> Self {
        let mut modes = HashSet::new();
        modes.insert(SqlModeFlag::OnlyFullGroupBy);
        modes.insert(SqlModeFlag::StrictTransTables);
        modes.insert(SqlModeFlag::NoZeroInDate);
        modes.insert(SqlModeFlag::NoZeroDate);
        modes.insert(SqlModeFlag::ErrorForDivisionByZero);
        modes.insert(SqlModeFlag::NoAutoValueOnZero);
        modes.insert(SqlModeFlag::NoEngineSubstitution);

        Self { modes }
    }

    /// MySQL 8.0 default SQL_MODE
    pub fn mysql_80_default() -> Self {
        let mut modes = HashSet::new();
        modes.insert(SqlModeFlag::OnlyFullGroupBy);
        modes.insert(SqlModeFlag::StrictTransTables);
        modes.insert(SqlModeFlag::NoZeroInDate);
        modes.insert(SqlModeFlag::NoZeroDate);
        modes.insert(SqlModeFlag::ErrorForDivisionByZero);
        modes.insert(SqlModeFlag::NoEngineSubstitution);

        Self { modes }
    }

    /// Parse SQL_MODE from string
    pub fn parse(mode_string: &str) -> Self {
        let mut modes = HashSet::new();

        for mode in mode_string.split(',') {
            match mode.trim().to_uppercase().as_str() {
                "ALLOW_INVALID_DATES" => {
                    modes.insert(SqlModeFlag::AllowInvalidDates);
                }
                "ANSI_QUOTES" => {
                    modes.insert(SqlModeFlag::AnsiQuotes);
                }
                "ERROR_FOR_DIVISION_BY_ZERO" => {
                    modes.insert(SqlModeFlag::ErrorForDivisionByZero);
                }
                "HIGH_NOT_PRECEDENCE" => {
                    modes.insert(SqlModeFlag::HighNotPrecedence);
                }
                "IGNORE_SPACE" => {
                    modes.insert(SqlModeFlag::IgnoreSpace);
                }
                "NO_AUTO_VALUE_ON_ZERO" => {
                    modes.insert(SqlModeFlag::NoAutoValueOnZero);
                }
                "NO_BACKSLASH_ESCAPES" => {
                    modes.insert(SqlModeFlag::NoBackslashEscapes);
                }
                "NO_ENGINE_SUBSTITUTION" => {
                    modes.insert(SqlModeFlag::NoEngineSubstitution);
                }
                "NO_UNSIGNED_SUBTRACTION" => {
                    modes.insert(SqlModeFlag::NoUnsignedSubtraction);
                }
                "NO_ZERO_DATE" => {
                    modes.insert(SqlModeFlag::NoZeroDate);
                }
                "NO_ZERO_IN_DATE" => {
                    modes.insert(SqlModeFlag::NoZeroInDate);
                }
                "ONLY_FULL_GROUP_BY" => {
                    modes.insert(SqlModeFlag::OnlyFullGroupBy);
                }
                "PAD_CHAR_TO_FULL_LENGTH" => {
                    modes.insert(SqlModeFlag::PadCharToFullLength);
                }
                "PIPES_AS_CONCAT" => {
                    modes.insert(SqlModeFlag::PipesAsConcat);
                }
                "REAL_AS_FLOAT" => {
                    modes.insert(SqlModeFlag::RealAsFloat);
                }
                "STRICT_ALL_TABLES" => {
                    modes.insert(SqlModeFlag::StrictAllTables);
                }
                "STRICT_TRANS_TABLES" => {
                    modes.insert(SqlModeFlag::StrictTransTables);
                }
                "TIME_TRUNCATE_FRACTIONAL" => {
                    modes.insert(SqlModeFlag::TimeTruncateFractional);
                }
                _ => {}
            }
        }

        Self { modes }
    }

    /// Convert to string representation
    pub fn to_mode_string(&self) -> String {
        let mode_strings: Vec<&str> = self
            .modes
            .iter()
            .map(|mode| match mode {
                SqlModeFlag::AllowInvalidDates => "ALLOW_INVALID_DATES",
                SqlModeFlag::AnsiQuotes => "ANSI_QUOTES",
                SqlModeFlag::ErrorForDivisionByZero => "ERROR_FOR_DIVISION_BY_ZERO",
                SqlModeFlag::HighNotPrecedence => "HIGH_NOT_PRECEDENCE",
                SqlModeFlag::IgnoreSpace => "IGNORE_SPACE",
                SqlModeFlag::NoAutoValueOnZero => "NO_AUTO_VALUE_ON_ZERO",
                SqlModeFlag::NoBackslashEscapes => "NO_BACKSLASH_ESCAPES",
                SqlModeFlag::NoEngineSubstitution => "NO_ENGINE_SUBSTITUTION",
                SqlModeFlag::NoUnsignedSubtraction => "NO_UNSIGNED_SUBTRACTION",
                SqlModeFlag::NoZeroDate => "NO_ZERO_DATE",
                SqlModeFlag::NoZeroInDate => "NO_ZERO_IN_DATE",
                SqlModeFlag::OnlyFullGroupBy => "ONLY_FULL_GROUP_BY",
                SqlModeFlag::PadCharToFullLength => "PAD_CHAR_TO_FULL_LENGTH",
                SqlModeFlag::PipesAsConcat => "PIPES_AS_CONCAT",
                SqlModeFlag::RealAsFloat => "REAL_AS_FLOAT",
                SqlModeFlag::StrictAllTables => "STRICT_ALL_TABLES",
                SqlModeFlag::StrictTransTables => "STRICT_TRANS_TABLES",
                SqlModeFlag::TimeTruncateFractional => "TIME_TRUNCATE_FRACTIONAL",
            })
            .collect();

        mode_strings.join(",")
    }

    pub fn has_mode(&self, flag: SqlModeFlag) -> bool {
        self.modes.contains(&flag)
    }

    pub fn add_mode(&mut self, flag: SqlModeFlag) {
        self.modes.insert(flag);
    }

    pub fn remove_mode(&mut self, flag: SqlModeFlag) {
        self.modes.remove(&flag);
    }
}

impl Default for SqlMode {
    fn default() -> Self {
        Self::mysql_80_default()
    }
}

/// Behavior differences between versions
pub struct VersionBehavior;

impl VersionBehavior {
    /// Get reserved words for version
    pub fn reserved_words(version: MySQLVersion) -> HashSet<String> {
        let mut words = HashSet::new();

        // Common reserved words
        let common = vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "FROM", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "ON",
            "AS", "AND", "OR", "NOT", "NULL", "TRUE", "FALSE", "TABLE", "CREATE", "DROP", "ALTER", "INDEX",
        ];

        for word in common {
            words.insert(word.to_string());
        }

        // MySQL 8.0 specific
        if version == MySQLVersion::MySQL80 {
            let mysql80_words = vec!["WINDOW", "RECURSIVE", "LATERAL", "SYSTEM"];
            for word in mysql80_words {
                words.insert(word.to_string());
            }
        }

        words
    }

    /// Check if keyword is reserved in version
    pub fn is_reserved_word(version: MySQLVersion, word: &str) -> bool {
        Self::reserved_words(version).contains(&word.to_uppercase())
    }

    /// Get maximum identifier length
    pub fn max_identifier_length(_version: MySQLVersion) -> usize {
        64
    }

    /// Get maximum string length for VARCHAR
    pub fn max_varchar_length(_version: MySQLVersion) -> usize {
        65535
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mysql_version() {
        let v57 = MySQLVersion::from_string("5.7.44");
        assert_eq!(v57, MySQLVersion::MySQL57);
        assert_eq!(v57.version_string(), "5.7.44-HeliosDB-Nano");

        let v80 = MySQLVersion::from_string("8.0.35");
        assert_eq!(v80, MySQLVersion::MySQL80);
        assert_eq!(v80.version_string(), "8.0.35-HeliosDB-Nano");
    }

    #[test]
    fn test_compatibility_mode_features() {
        let mode57 = CompatibilityMode::new(MySQLVersion::MySQL57);
        let mode80 = CompatibilityMode::new(MySQLVersion::MySQL80);

        assert!(!mode57.supports_feature(Feature::WindowFunctions));
        assert!(mode80.supports_feature(Feature::WindowFunctions));

        assert!(!mode57.supports_feature(Feature::CTERecursive));
        assert!(mode80.supports_feature(Feature::CTERecursive));
    }

    #[test]
    fn test_sql_mode_parse() {
        let mode = SqlMode::parse("STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION");
        assert!(mode.has_mode(SqlModeFlag::StrictTransTables));
        assert!(mode.has_mode(SqlModeFlag::NoEngineSubstitution));
        assert!(!mode.has_mode(SqlModeFlag::AnsiQuotes));
    }

    #[test]
    fn test_sql_mode_to_string() {
        let mode = SqlMode::mysql_80_default();
        let mode_string = mode.to_mode_string();
        assert!(mode_string.contains("STRICT_TRANS_TABLES"));
        assert!(mode_string.contains("NO_ENGINE_SUBSTITUTION"));
    }

    #[test]
    fn test_default_auth_plugin() {
        let mode57 = CompatibilityMode::new(MySQLVersion::MySQL57);
        let mode80 = CompatibilityMode::new(MySQLVersion::MySQL80);

        assert_eq!(mode57.default_auth_plugin(), "mysql_native_password");
        assert_eq!(mode80.default_auth_plugin(), "caching_sha2_password");
    }

    #[test]
    fn test_default_charset() {
        let mode57 = CompatibilityMode::new(MySQLVersion::MySQL57);
        let mode80 = CompatibilityMode::new(MySQLVersion::MySQL80);

        assert_eq!(mode57.default_charset(), "latin1");
        assert_eq!(mode80.default_charset(), "utf8mb4");
    }

    #[test]
    fn test_reserved_words() {
        let words80 = VersionBehavior::reserved_words(MySQLVersion::MySQL80);
        assert!(words80.contains("SELECT"));
        assert!(words80.contains("WINDOW"));

        assert!(VersionBehavior::is_reserved_word(MySQLVersion::MySQL80, "RECURSIVE"));
    }
}

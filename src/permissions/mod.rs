use serde::{Deserialize, Serialize};
use std::fmt;

/// All available permissions a user can be granted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Filesystem access: read_file, write_file, list_directory
    Fs,
    // Future:
    // Network,
    // Exec,
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Permission::Fs => write!(f, "fs"),
        }
    }
}

impl Permission {
    #[allow(dead_code)]
    pub fn all() -> &'static [Permission] {
        &[Permission::Fs]
    }

    #[allow(dead_code)]
    pub fn description(&self) -> &'static str {
        match self {
            Permission::Fs => "Filesystem access (read, write, list)",
        }
    }
}

/// Check whether a given set of permissions contains the required one.
pub fn has_permission(granted: &[Permission], required: &Permission) -> bool {
    granted.contains(required)
}

//! `[xmarket]` — sandbox-wide settings of the xmarket runtime. Today: the
//! install-wide state directory (tracker convention 3). Later milestones add
//! calendars, venues and lifecycle knobs. `deny_unknown_fields`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `[xmarket]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XmarketConfig {
    /// Directory name under `<TENGU_HOME>/state/` that holds the install-wide
    /// stores (`ledger.db`, `runtime.db`, `history/`, …): `xmarket` for the
    /// main sandbox, `xmarket-weekend` for the weekend run. One path segment;
    /// `tengu prune` never deletes `state/` (only `state/flows`).
    #[serde(default = "default_state")]
    pub state: String,
}

impl Default for XmarketConfig {
    fn default() -> Self {
        Self {
            state: default_state(),
        }
    }
}

fn default_state() -> String {
    "xmarket".to_string()
}

impl XmarketConfig {
    /// `<tengu_home>/state/<state>`.
    pub fn state_dir(&self, tengu_home: &Path) -> PathBuf {
        tengu_home.join("state").join(&self.state)
    }

    pub fn validation_errors(&self) -> Vec<String> {
        let s = self.state.as_str();
        let ok = !s.is_empty()
            && s != "."
            && s != ".."
            && s != "flows"
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if ok {
            Vec::new()
        } else {
            vec![format!(
                "xmarket.state `{s}` must be one directory name ([A-Za-z0-9._-], not `.`, `..` or `flows`)"
            )]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_state_dir() {
        let x: XmarketConfig = toml::from_str("").unwrap();
        assert_eq!(x.state, "xmarket");
        assert_eq!(
            x.state_dir(Path::new("/h")),
            PathBuf::from("/h/state/xmarket")
        );
        assert!(x.validation_errors().is_empty());
    }

    #[test]
    fn rejects_paths_and_unknown_keys() {
        for bad in ["", "..", "a/b", "flows", "x y"] {
            let x = XmarketConfig {
                state: bad.to_string(),
            };
            assert_eq!(x.validation_errors().len(), 1, "{bad:?}");
        }
        assert!(toml::from_str::<XmarketConfig>("stat = \"x\"").is_err());
    }
}

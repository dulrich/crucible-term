//! The environment a spawned child inherits: never the CDE per-spawn variables.

use crate::client::{GRANT_VAR, SESSION_VAR};
use std::ffi::OsString;

/// `env` without `CRUCIBLE_WINDOW_GRANT` or `CRUCIBLE_TERM_SESSION`.
pub fn strip_cde_vars(env: Vec<(OsString, OsString)>) -> Vec<(OsString, OsString)> {
    env.into_iter()
        .filter(|(k, _)| k != GRANT_VAR && k != SESSION_VAR)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_is_stripped() {
        let env = vec![
            ("A".into(), "1".into()),
            (GRANT_VAR.into(), "secret".into()),
            ("CRUCIBLE_WINDOW_GRANTX".into(), "keep".into()),
        ];
        let out = strip_cde_vars(env);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|(k, _)| k != GRANT_VAR));
    }
}

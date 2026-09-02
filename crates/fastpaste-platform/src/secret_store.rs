//! Storing one secret in the OS credential store.
//!
//! | Alias | Linux | Windows |
//! |---|---|---|
//! | [`crate::SystemSecretStore`] | Secret Service (libsecret) | Credential Manager |
//!
//! Used to remember the database passphrase so the app can start without
//! prompting. Whether that is a good trade is the user's call, made in
//! the Options dialog — this module only carries it out.

use secrecy::SecretString;
use thiserror::Error;

pub mod keyring;

pub use keyring::KeyringSecretStore;

#[derive(Error, Debug)]
pub enum SecretStoreError {
    #[error("no credential store available: {0}")]
    Unavailable(String),

    #[error("credential store failed: {0}")]
    Backend(String),
}

/// One named secret in the OS credential store.
///
/// Implementations must treat "no such entry" as `Ok(None)` from
/// [`Self::get`] and as `Ok(())` from [`Self::delete`]: the caller clears
/// a possibly-absent stale entry without checking first.
pub trait SecretStore: Send + Sync {
    /// Whether this store can be used at all. False on a Linux session
    /// with no Secret Service daemon, which is a degraded mode rather
    /// than a failure — the app prompts every launch instead.
    fn is_available(&self) -> bool;

    fn get(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError>;
    fn set(&self, account: &str, secret: &SecretString) -> Result<(), SecretStoreError>;
    fn delete(&self, account: &str) -> Result<(), SecretStoreError>;
}

/// In-memory stand-in, for tests and for headless runs. Mirrors
/// [`crate::NullClipboard`]: it works, it just isn't the OS.
#[derive(Debug, Default)]
pub struct NullSecretStore {
    entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl NullSecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for NullSecretStore {
    fn is_available(&self) -> bool {
        true
    }

    fn get(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .map(|v| SecretString::from(v.clone())))
    }

    fn set(&self, account: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
        use secrecy::ExposeSecret;
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), secret.expose_secret().to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
        Ok(())
    }
}

/// What the app falls back to when the real store cannot be reached.
/// Every operation fails, and [`SecretStore::is_available`] says so up
/// front so the UI can explain itself instead of erroring on click.
#[derive(Debug)]
pub struct UnavailableSecretStore;

impl SecretStore for UnavailableSecretStore {
    fn is_available(&self) -> bool {
        false
    }

    fn get(&self, _account: &str) -> Result<Option<SecretString>, SecretStoreError> {
        Err(SecretStoreError::Unavailable(
            "no credential store on this session".into(),
        ))
    }

    fn set(&self, _account: &str, _secret: &SecretString) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::Unavailable(
            "no credential store on this session".into(),
        ))
    }

    fn delete(&self, _account: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::Unavailable(
            "no credential store on this session".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::{ExposeSecret, SecretString};

    #[test]
    fn the_null_store_round_trips_a_secret() {
        let store = NullSecretStore::new();
        assert!(store.is_available());
        assert!(store.get("db").unwrap().is_none(), "empty to start with");

        store
            .set("db", &SecretString::from("s3cret".to_string()))
            .unwrap();
        assert_eq!(store.get("db").unwrap().unwrap().expose_secret(), "s3cret");

        store.delete("db").unwrap();
        assert!(store.get("db").unwrap().is_none(), "delete must clear it");
    }

    #[test]
    fn deleting_a_secret_that_is_not_there_is_not_an_error() {
        // The stale-keyring-entry path calls delete without checking
        // first; it must not turn a missing entry into a failure.
        let store = NullSecretStore::new();
        store.delete("db").unwrap();
    }

    #[test]
    fn the_unavailable_store_reports_itself_and_fails_every_operation() {
        // Linux with no Secret Service daemon. The Options dialog reads
        // `is_available` to disable the Remember checkbox with a reason.
        let store = UnavailableSecretStore;
        assert!(!store.is_available());
        assert!(store.get("db").is_err());
        assert!(
            store
                .set("db", &SecretString::from("x".to_string()))
                .is_err()
        );
        assert!(store.delete("db").is_err());
    }
}

//! The real credential store, via the `keyring` crate.

use secrecy::{ExposeSecret, SecretString};

use super::{SecretStore, SecretStoreError};

/// Service name under which entries are filed. Visible to the user in
/// their keyring UI, so it is the product name and nothing more.
const SERVICE: &str = "fastpaste";

#[derive(Debug)]
pub struct KeyringSecretStore {
    available: bool,
}

impl KeyringSecretStore {
    /// Probe the store once at construction. A `get` on a nonexistent
    /// entry is the cheapest round trip that proves the daemon answers:
    /// `NoEntry` means it is there and the entry simply is not.
    pub fn new() -> Self {
        let available = match ::keyring::Entry::new(SERVICE, "__probe__") {
            Ok(entry) => !matches!(
                entry.get_password(),
                Err(::keyring::Error::PlatformFailure(_))
                    | Err(::keyring::Error::NoStorageAccess(_))
            ),
            Err(e) => {
                tracing::warn!("credential store unavailable: {e}");
                false
            }
        };
        Self { available }
    }

    fn entry(&self, account: &str) -> Result<::keyring::Entry, SecretStoreError> {
        ::keyring::Entry::new(SERVICE, account)
            .map_err(|e| SecretStoreError::Backend(e.to_string()))
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for KeyringSecretStore {
    fn is_available(&self) -> bool {
        self.available
    }

    fn get(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError> {
        match self.entry(account)?.get_password() {
            Ok(p) => Ok(Some(SecretString::from(p))),
            Err(::keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretStoreError::Backend(e.to_string())),
        }
    }

    fn set(&self, account: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
        self.entry(account)?
            .set_password(secret.expose_secret())
            .map_err(|e| SecretStoreError::Backend(e.to_string()))
    }

    fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(::keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretStoreError::Backend(e.to_string())),
        }
    }
}

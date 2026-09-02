//! The real credential store, via the `keyring` crate.

use std::sync::atomic::{AtomicBool, Ordering};

use secrecy::{ExposeSecret, SecretString};

use super::{SecretStore, SecretStoreError};

/// Service name under which entries are filed. Visible to the user in
/// their keyring UI, so it is the product name and nothing more.
const SERVICE: &str = "fastpaste";

/// ## Caching `is_available`
///
/// A `true` result is cached in `available` for the rest of the
/// process: a credential store that has answered once will not stop
/// existing mid-session, so there is no reason to pay for another round
/// trip. A `false` result is never cached. `keyring::Error::NoStorageAccess`
/// — the variant that makes a probe report "unavailable" — is documented
/// to fire both when there is truly no daemon *and* when the login
/// keyring exists but is still locked, which is transient (most often
/// early at login, before PAM unlocks it). Latching that as a permanent
/// "no" would leave the Options dialog's Remember checkbox disabled for
/// the rest of the session even after the keyring unlocks seconds
/// later, with no way back short of a restart. So every call while
/// `available` is still false re-probes live.
#[derive(Debug)]
pub struct KeyringSecretStore {
    available: AtomicBool,
}

impl KeyringSecretStore {
    pub fn new() -> Self {
        Self {
            available: AtomicBool::new(false),
        }
    }

    /// A `get` on a nonexistent entry is the cheapest round trip that
    /// proves the daemon answers: `NoEntry` means it is there and the
    /// entry simply is not. Never uses `set`, so a probe leaves no
    /// stray entry behind.
    fn probe() -> bool {
        match ::keyring::Entry::new(SERVICE, "__probe__") {
            Ok(entry) => !matches!(
                entry.get_password(),
                Err(::keyring::Error::PlatformFailure(_))
                    | Err(::keyring::Error::NoStorageAccess(_))
            ),
            Err(e) => {
                tracing::warn!("credential store unavailable: {e}");
                false
            }
        }
    }

    fn entry(&self, account: &str) -> Result<::keyring::Entry, SecretStoreError> {
        ::keyring::Entry::new(SERVICE, account).map_err(|e| SecretStoreError::Backend(Box::new(e)))
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for KeyringSecretStore {
    fn is_available(&self) -> bool {
        if self.available.load(Ordering::Relaxed) {
            return true;
        }
        let available = Self::probe();
        if available {
            self.available.store(true, Ordering::Relaxed);
        }
        available
    }

    fn get(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError> {
        match self.entry(account)?.get_password() {
            Ok(p) => Ok(Some(SecretString::from(p))),
            Err(::keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretStoreError::Backend(Box::new(e))),
        }
    }

    fn set(&self, account: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
        self.entry(account)?
            .set_password(secret.expose_secret())
            .map_err(|e| SecretStoreError::Backend(Box::new(e)))
    }

    fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(::keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretStoreError::Backend(Box::new(e))),
        }
    }
}

//! Master password storage in the system keyring.
//!
//! The password used to lock notes lives in the desktop keyring
//! (GNOME Keyring, KWallet, …) under the `pinlet` service — never on
//! disk, where anything running as the user could read it.

use crate::error::{AppError, AppResult};

/// Keyring service holding the master password.
const SERVICE: &str = "pinlet";

/// Keyring account holding the master password.
const ACCOUNT: &str = "master-password";

/// The keyring entry, or the reason it cannot be opened.
fn entry() -> AppResult<keyring::Entry> {
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(|err| AppError::Settings(err.to_string()))
}

/// The keyring entry for tests (a unique account, so test runs never
/// touch — or clobber — the user's real master password).
#[cfg(test)]
fn test_entry(account: &str) -> AppResult<keyring::Entry> {
    keyring::Entry::new("pinlet-test", account).map_err(|err| AppError::Settings(err.to_string()))
}

/// Read the master password, if one is set and the keyring answers.
pub fn get_master_password() -> AppResult<Option<String>> {
    match entry()?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(AppError::Settings(err.to_string())),
    }
}

/// Store (or replace) the master password.
pub fn set_master_password(password: &str) -> AppResult<()> {
    entry()?
        .set_password(password)
        .map_err(|err| AppError::Settings(err.to_string()))
}

/// Forget the master password. A missing entry is fine.
pub fn clear_master_password() -> AppResult<()> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(AppError::Settings(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keyring round-trip. Skips (rather than fails) where no secret
    /// service answers — headless CI has none.
    #[test]
    fn round_trips_when_keyring_available() {
        let account = format!("pinlet-test-{}", uuid::Uuid::new_v4());
        let entry = match test_entry(&account) {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("keyring unavailable, skipping: {err}");
                return;
            }
        };
        if let Err(err) = entry.set_password("s3cret") {
            eprintln!("keyring unavailable, skipping: {err}");
            return;
        }
        assert_eq!(entry.get_password().unwrap(), "s3cret");
        entry.delete_credential().unwrap();
        assert!(matches!(
            entry.get_password().unwrap_err(),
            keyring::Error::NoEntry
        ));
    }
}

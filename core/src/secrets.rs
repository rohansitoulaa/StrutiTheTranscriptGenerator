//! LLM API keys live in the system keyring (KWallet on this machine, via
//! the Secret Service), never in settings files, Postgres, logs or notes
//! (PDA LLM-5). Calls are blocking D-Bus round trips; async callers wrap
//! them in `spawn_blocking`.

use crate::llm::ProviderKind;
use crate::{Error, Result};

const SERVICE: &str = "shruti";

/// The stored key for a provider, or `None` if none is stored.
pub fn api_key(provider: ProviderKind) -> Result<Option<String>> {
    get(SERVICE, provider.as_str())
}

pub fn set_api_key(provider: ProviderKind, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        return Err(Error::Keyring("refusing to store an empty key".into()));
    }
    entry(SERVICE, provider.as_str())?
        .set_password(key)
        .map_err(keyring_err)
}

pub fn delete_api_key(provider: ProviderKind) -> Result<()> {
    delete(SERVICE, provider.as_str())
}

fn entry(service: &str, user: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service, user).map_err(keyring_err)
}

fn get(service: &str, user: &str) -> Result<Option<String>> {
    match entry(service, user)?.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(keyring_err(e)),
    }
}

fn delete(service: &str, user: &str) -> Result<()> {
    match entry(service, user)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(keyring_err(e)),
    }
}

/// The keyring's own messages never include the secret.
fn keyring_err(e: keyring::Error) -> Error {
    Error::Keyring(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to the real Secret Service (KWallet) with a throwaway entry,
    /// and removes it again.
    #[test]
    #[ignore = "needs the desktop keyring"]
    fn round_trips_a_dummy_entry_in_the_real_keyring() {
        let (service, user) = ("shruti-test", "dummy");
        delete(service, user).unwrap();
        assert_eq!(get(service, user).unwrap(), None);
        entry(service, user)
            .unwrap()
            .set_password("not-a-real-key")
            .unwrap();
        assert_eq!(
            get(service, user).unwrap().as_deref(),
            Some("not-a-real-key")
        );
        delete(service, user).unwrap();
        assert_eq!(get(service, user).unwrap(), None);
    }

    #[test]
    fn empty_keys_are_refused_before_touching_the_keyring() {
        assert!(matches!(
            set_api_key(ProviderKind::Gemini, "  "),
            Err(Error::Keyring(_))
        ));
    }
}

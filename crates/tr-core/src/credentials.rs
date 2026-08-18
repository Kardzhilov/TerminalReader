//! Sync credentials stored in the operating system keyring.
//!
//! Only the derived userkey (MD5 of the password, per the kosync protocol) is
//! stored; the plain password never touches disk.

use keyring::Entry;
use thiserror::Error;

const SERVICE: &str = "TerminalReader-kosync";

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("keyring unavailable: {0}")]
    Keyring(#[from] keyring::Error),
}

fn entry(server_url: &str, username: &str) -> Result<Entry, CredentialError> {
    let account = format!("{username}@{}", server_url.trim_end_matches('/'));
    Ok(Entry::new(SERVICE, &account)?)
}

/// Store the userkey for `username` on `server_url`.
pub fn store_userkey(
    server_url: &str,
    username: &str,
    userkey: &str,
) -> Result<(), CredentialError> {
    entry(server_url, username)?.set_password(userkey)?;
    Ok(())
}

/// Load the stored userkey; `Ok(None)` when no credential exists.
pub fn load_userkey(server_url: &str, username: &str) -> Result<Option<String>, CredentialError> {
    match entry(server_url, username)?.get_password() {
        Ok(userkey) => Ok(Some(userkey)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Remove the stored userkey; missing entries are not an error.
pub fn delete_userkey(server_url: &str, username: &str) -> Result<(), CredentialError> {
    match entry(server_url, username)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

//! SSH passwords in the system credential store, keyed by connection id.
//! Passwords never touch `connections.json`, the command line, or the
//! environment; ssh reads them back through Waku's askpass mode.

use anyhow::{Context as _, bail};

const KEYRING_SERVICE: &str = "sh.waku.ssh";

fn entry(connection_id: &str) -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, connection_id)
        .context("could not access the system credential store")
}

pub fn set_ssh_password(connection_id: &str, password: &str) -> anyhow::Result<()> {
    if password.is_empty() {
        bail!("SSH password cannot be empty");
    }
    entry(connection_id)?
        .set_password(password)
        .context("could not save the SSH password")
}

pub fn get_ssh_password(connection_id: &str) -> anyhow::Result<String> {
    entry(connection_id)?
        .get_password()
        .context("could not read the SSH password")
}

pub fn has_ssh_password(connection_id: &str) -> bool {
    entry(connection_id).is_ok_and(|entry| entry.get_password().is_ok())
}

pub fn delete_ssh_password(connection_id: &str) -> anyhow::Result<()> {
    match entry(connection_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error).context("could not remove the SSH password"),
    }
}

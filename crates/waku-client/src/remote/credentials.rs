//! SSH passwords in the system credential store, keyed by connection id.
//! Passwords never touch `connections.json`, the command line, or the
//! environment; ssh reads them back through Waku's askpass mode.

use anyhow::{Context as _, bail};

const KEYRING_SERVICE: &str = "sh.narel.ssh";
const LEGACY_KEYRING_SERVICE: &str = "sh.waku.ssh";
const LEGACY_DISABLED_SERVICE: &str = "sh.narel.ssh.legacy-disabled";

fn entry(connection_id: &str) -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, connection_id)
        .context("could not access the system credential store")
}

pub fn set_ssh_password(connection_id: &str, password: &str) -> anyhow::Result<()> {
    if password.is_empty() {
        bail!("SSH password cannot be empty");
    }
    #[cfg(windows)]
    if password.encode_utf16().count() * 2 > 2560 {
        bail!("SSH password exceeds Windows Credential Manager's 2560-byte UTF-16 limit");
    }
    entry(connection_id)?
        .set_password(password)
        .context("could not save the SSH password")
}

pub fn get_ssh_password(connection_id: &str) -> anyhow::Result<String> {
    password_with_legacy_fallback(entry(connection_id)?.get_password(), || {
        // Do not mask a locked/unavailable new keychain with an older password.
        // Clear in Narel must not remove Waku's credential or revive it later.
        match keyring::Entry::new(LEGACY_DISABLED_SERVICE, connection_id)?.get_password() {
            Ok(_) => Err(keyring::Error::NoEntry),
            Err(keyring::Error::NoEntry) => {
                keyring::Entry::new(LEGACY_KEYRING_SERVICE, connection_id)?.get_password()
            }
            Err(error) => Err(error),
        }
    })
    .context("could not read the SSH password")
}

fn password_with_legacy_fallback(
    current: Result<String, keyring::Error>,
    legacy: impl FnOnce() -> Result<String, keyring::Error>,
) -> Result<String, keyring::Error> {
    match current {
        Err(keyring::Error::NoEntry) => legacy(),
        result => result,
    }
}

pub fn has_ssh_password(connection_id: &str) -> bool {
    get_ssh_password(connection_id).is_ok()
}

pub fn delete_ssh_password(connection_id: &str) -> anyhow::Result<()> {
    // Write the opt-out before deleting: a failed write must not silently
    // reactivate Waku's password. Waku's entry is deliberately left untouched.
    keyring::Entry::new(LEGACY_DISABLED_SERVICE, connection_id)?
        .set_password("1")
        .context("could not disable the legacy SSH password")?;
    match entry(connection_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error).context("could not remove the SSH password"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestCredentialCleanup(String);

    impl Drop for TestCredentialCleanup {
        fn drop(&mut self) {
            let _ = delete_ssh_password(&self.0);
            if let Ok(entry) = keyring::Entry::new(LEGACY_DISABLED_SERVICE, &self.0) {
                let _ = entry.delete_credential();
            }
        }
    }

    #[test]
    fn current_password_wins_and_only_absent_entries_fall_back() {
        assert_eq!(
            password_with_legacy_fallback(Ok("new".into()), || panic!("must not read legacy"))
                .unwrap(),
            "new"
        );
        assert_eq!(
            password_with_legacy_fallback(Err(keyring::Error::NoEntry), || Ok("old".into()))
                .unwrap(),
            "old"
        );
    }

    #[test]
    fn empty_password_is_rejected() {
        let error = set_ssh_password("waku-test-empty", "").unwrap_err();
        assert!(error.to_string().contains("cannot be empty"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_passwords_exceeding_2560_utf16_bytes() {
        let id = format!("waku-test-{}", uuid::Uuid::new_v4());
        let _cleanup = TestCredentialCleanup(id.clone());

        let too_long_ascii = "a".repeat(1281);
        let error = set_ssh_password(&id, &too_long_ascii).unwrap_err();
        assert!(error.to_string().contains("2560"));
        assert!(!has_ssh_password(&id));

        // 641 supplementary-plane emoji take 1282 UTF-16 code units (2564 bytes).
        let too_long_surrogate = "🔑".repeat(641);
        let error = set_ssh_password(&id, &too_long_surrogate).unwrap_err();
        assert!(error.to_string().contains("2560"));
        assert!(!has_ssh_password(&id));
    }

    #[test]
    fn roundtrip_set_has_get_and_delete() {
        let id = format!("waku-test-{}", uuid::Uuid::new_v4());
        let _cleanup = TestCredentialCleanup(id.clone());

        assert!(!has_ssh_password(&id));
        assert!(get_ssh_password(&id).is_err());

        let secret = "pässw0rd-中-🔑";
        set_ssh_password(&id, secret).expect("save SSH password");
        assert!(has_ssh_password(&id));
        assert_eq!(get_ssh_password(&id).expect("read SSH password"), secret);

        delete_ssh_password(&id).expect("delete SSH password");
        assert!(!has_ssh_password(&id));
        assert!(get_ssh_password(&id).is_err());

        // Deleting an already-absent entry stays idempotent.
        delete_ssh_password(&id).expect("delete absent SSH password");
    }
}

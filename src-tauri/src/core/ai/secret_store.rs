use std::fmt;

use keyring::Entry;

use super::command_error;
use super::types::{AiCommandError, AiErrorCode, AiErrorKind};

const KEYRING_SERVICE: &str = "skills-manager-ai-analysis";
const KEYRING_ACCOUNT: &str = "default";
const MAX_API_KEY_BYTES: usize = 16_384;

/// The entry is intentionally kept private and Debug is redacted so neither a
/// diagnostic snapshot nor an error can expose a credential implementation.
pub struct SecretStore {
    entry: Entry,
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretStore([redacted])")
    }
}

impl SecretStore {
    pub fn new() -> Result<Self, AiCommandError> {
        let entry =
            Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).map_err(|_| keyring_error("open"))?;
        Ok(Self { entry })
    }

    pub fn set(&self, api_key: &str) -> Result<(), AiCommandError> {
        let byte_length = api_key.as_bytes().len();
        if byte_length == 0 || byte_length > MAX_API_KEY_BYTES || api_key.trim().is_empty() {
            return Err(command_error(
                AiErrorKind::Validation,
                AiErrorCode::InvalidConfig,
                "The API key must contain 1 to 16384 UTF-8 bytes and cannot be blank.",
                false,
            ));
        }
        // Preserve the exact submitted value because whitespace may be a valid
        // part of a provider credential even though an all-whitespace key is not.
        self.entry
            .set_password(api_key)
            .map_err(|_| keyring_error("store"))
    }

    pub fn load(&self) -> Result<Option<String>, AiCommandError> {
        match self.entry.get_password() {
            Ok(api_key) => Ok(Some(api_key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(keyring_error("read")),
        }
    }

    pub fn has(&self) -> Result<bool, AiCommandError> {
        // Reuse the exact NoEntry/error behavior so status checks cannot hide a
        // broken system keychain behind a misleading "not configured" state.
        self.load().map(|api_key| api_key.is_some())
    }

    pub fn delete(&self) -> Result<(), AiCommandError> {
        match self.entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(keyring_error("delete")),
        }
    }
}

fn keyring_error(operation: &str) -> AiCommandError {
    // Dependency errors are deliberately not formatted because platform
    // keychains may include the submitted secret in their native messages.
    command_error(
        AiErrorKind::Storage,
        AiErrorCode::Keyring,
        format!("Unable to {operation} the AI API key in the system keychain."),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyring::mock::MockCredential;
    use std::sync::Mutex;

    // keyring installs one process-wide credential builder, so all tests using
    // its in-memory backend must be serialized and must clean their entry.
    static KEYRING_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn mock_store() -> SecretStore {
        crate::core::git_credentials::use_mock_keyring();
        let store = SecretStore::new().unwrap();
        store.delete().unwrap();
        store
    }

    #[test]
    fn keyring_crud_is_exact_and_delete_is_idempotent() {
        let _guard = KEYRING_TEST_LOCK.lock().unwrap();
        let store = mock_store();
        let api_key = "  test-key-preserves-spaces  ";
        assert!(!store.has().unwrap());
        store.set(api_key).unwrap();
        assert!(store.has().unwrap());
        assert_eq!(store.load().unwrap().as_deref(), Some(api_key));
        store.delete().unwrap();
        store.delete().unwrap();
        assert!(!store.has().unwrap());
    }

    #[test]
    fn key_length_and_blank_rules_use_utf8_bytes() {
        let _guard = KEYRING_TEST_LOCK.lock().unwrap();
        let store = mock_store();
        assert_eq!(
            store.set(" \t ").unwrap_err().code,
            AiErrorCode::InvalidConfig
        );
        assert_eq!(
            store.set(&"é".repeat(8193)).unwrap_err().code,
            AiErrorCode::InvalidConfig
        );
        store.delete().unwrap();
    }

    #[test]
    fn dependency_errors_and_debug_never_contain_the_key() {
        let _guard = KEYRING_TEST_LOCK.lock().unwrap();
        let store = mock_store();
        let fake_key = "test-secret-that-must-not-leak";
        let mock: &MockCredential = store.entry.get_credential().downcast_ref().unwrap();
        mock.set_error(keyring::Error::Invalid(
            fake_key.to_string(),
            fake_key.to_string(),
        ));
        let error = store.set(fake_key).unwrap_err();
        assert!(!format!("{error}").contains(fake_key));
        assert!(!format!("{error:?}").contains(fake_key));
        assert!(!format!("{store:?}").contains(fake_key));
        store.delete().unwrap();
    }
}

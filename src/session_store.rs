/*
SPDX-License-Identifier: AGPL-3.0-only
*/

//! Login metadata on disk; authentication tokens in the desktop Secret Service.

use anyhow::{Context, Result};
use matrix_sdk::{
    authentication::{matrix::MatrixSession, SessionTokens},
    SessionMeta,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    future::Future,
    io::Write,
    path::{Path, PathBuf},
};

/// Non-secret information needed to reopen the same Matrix device and crypto store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub homeserver: String,
    pub store_dir: String,
    #[serde(flatten)]
    pub meta: SessionMeta,
}

impl SessionMetadata {
    pub fn new(homeserver: &str, store_dir: &str, session: &MatrixSession) -> Self {
        Self {
            homeserver: homeserver.into(),
            store_dir: store_dir.into(),
            meta: session.meta.clone(),
        }
    }

    fn attributes(&self) -> HashMap<&str, &str> {
        HashMap::from([
            ("application", "thrace"),
            ("purpose", "matrix-session"),
            ("homeserver", self.homeserver.as_str()),
            ("user_id", self.meta.user_id.as_str()),
            ("device_id", self.meta.device_id.as_str()),
        ])
    }
}

trait TokenStore {
    fn put(
        &self,
        meta: &SessionMetadata,
        tokens: &SessionTokens,
    ) -> impl Future<Output = Result<()>> + Send;
    fn get(&self, meta: &SessionMetadata) -> impl Future<Output = Result<SessionTokens>> + Send;
    fn delete(&self, meta: &SessionMetadata) -> impl Future<Output = Result<()>> + Send;
}

struct DesktopSecrets;

async fn connect() -> Result<secret_service::SecretService<'static>> {
    secret_service::SecretService::connect(secret_service::EncryptionType::Dh).await
        .context("Secret Service unavailable; start or unlock KWallet or another Secret Service provider")
}

impl TokenStore for DesktopSecrets {
    async fn put(&self, meta: &SessionMetadata, tokens: &SessionTokens) -> Result<()> {
        let service = connect().await?;
        let collection = service
            .get_default_collection()
            .await
            .context("No default wallet is available; create a default wallet in KWallet")?;
        if collection.is_locked().await? {
            collection
                .unlock()
                .await
                .context("Wallet unlock was cancelled or failed")?;
        }
        let secret = serde_json::to_vec(tokens)?;
        collection
            .create_item(
                "Thrace Matrix session",
                meta.attributes(),
                &secret,
                true,
                "application/json",
            )
            .await
            .context("Could not store login tokens in the wallet")?;
        Ok(())
    }

    async fn get(&self, meta: &SessionMetadata) -> Result<SessionTokens> {
        let service = connect().await?;
        let found = service.search_items(meta.attributes()).await?;
        let item = found
            .unlocked
            .first()
            .or_else(|| found.locked.first())
            .context("Login tokens are missing from the wallet; sign in again")?;
        if item.is_locked().await? {
            item.unlock()
                .await
                .context("Wallet unlock was cancelled or failed")?;
        }
        let secret = item
            .get_secret()
            .await
            .context("Could not read login tokens from the wallet")?;
        serde_json::from_slice(&secret).context("Invalid login token record in the wallet")
    }

    async fn delete(&self, meta: &SessionMetadata) -> Result<()> {
        let service = connect().await?;
        let found = service.search_items(meta.attributes()).await?;
        for item in found.unlocked.iter().chain(found.locked.iter()) {
            if item.is_locked().await? {
                item.unlock().await?;
            }
            item.delete()
                .await
                .context("Could not remove login tokens from the wallet")?;
        }
        Ok(())
    }
}

async fn wallet_timeout<T>(operation: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(std::time::Duration::from_secs(90), operation)
        .await
        .context("Timed out waiting for the wallet; unlock it and try again")?
}

/// Persist credentials first, then atomically publish metadata with no tokens.
pub async fn save(path: &Path, meta: &SessionMetadata, session: &MatrixSession) -> Result<()> {
    wallet_timeout(save_with(path, meta, &session.tokens, &DesktopSecrets)).await
}

async fn save_with(
    path: &Path,
    meta: &SessionMetadata,
    tokens: &SessionTokens,
    store: &impl TokenStore,
) -> Result<()> {
    store.put(meta, tokens).await?;
    let stored = store.get(meta).await?;
    anyhow::ensure!(
        stored == *tokens,
        "Wallet verification failed; existing session file left intact"
    );
    write_metadata(path, meta)
}

#[derive(Deserialize)]
struct LegacySession {
    homeserver: String,
    session: MatrixSession,
    store_dir: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DiskSession {
    Legacy(LegacySession),
    Metadata(SessionMetadata),
}

/// Restore wallet credentials, migrating the old plaintext format only after a verified save.
pub async fn load(path: &Path) -> Result<Option<(SessionMetadata, MatrixSession)>> {
    wallet_timeout(load_with(path, &DesktopSecrets)).await
}

async fn load_with(
    path: &Path,
    store: &impl TokenStore,
) -> Result<Option<(SessionMetadata, MatrixSession)>> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Read saved session"),
    };
    let disk: DiskSession = serde_json::from_slice(&raw).context("Invalid saved session")?;
    let (meta, tokens) = match disk {
        DiskSession::Metadata(meta) => {
            let tokens = store.get(&meta).await?;
            (meta, tokens)
        }
        DiskSession::Legacy(legacy) => {
            // Protect existing plaintext while the wallet prompts, without discarding it on failure.
            private_file(path)?;
            let store_dir = legacy.store_dir.unwrap_or_else(|| {
                let safe: String = legacy
                    .homeserver
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect();
                path.parent()
                    .unwrap_or(Path::new("."))
                    .join(format!("restore-{safe}.sqlite"))
                    .to_string_lossy()
                    .into_owned()
            });
            let meta = SessionMetadata::new(&legacy.homeserver, &store_dir, &legacy.session);
            save_with(path, &meta, &legacy.session.tokens, store).await?;
            (meta, legacy.session.tokens)
        }
    };
    let session = MatrixSession {
        meta: meta.meta.clone(),
        tokens,
    };
    Ok(Some((meta, session)))
}

/// Forget the local login marker immediately, even if the wallet is unavailable.
pub fn forget_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Remove saved session metadata"),
    }
}

/// Delete only this application's matching account/device credentials.
pub async fn delete_tokens(meta: &SessionMetadata) -> Result<()> {
    wallet_timeout(DesktopSecrets.delete(meta)).await
}

fn private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn write_metadata(path: &Path, meta: &SessionMetadata) -> Result<()> {
    let parent = path.parent().context("Session path has no parent")?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let temp: PathBuf = path.with_extension(format!("{}-{suffix}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .context("Create temporary session metadata")?;
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec_pretty(meta)?)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.context("Save session metadata")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockStore {
        tokens: Mutex<Option<SessionTokens>>,
        fail: bool,
        corrupt: bool,
    }

    impl TokenStore for MockStore {
        async fn put(&self, _: &SessionMetadata, tokens: &SessionTokens) -> Result<()> {
            anyhow::ensure!(!self.fail, "Wallet unavailable");
            *self.tokens.lock().unwrap() = Some(tokens.clone());
            Ok(())
        }
        async fn get(&self, _: &SessionMetadata) -> Result<SessionTokens> {
            anyhow::ensure!(!self.fail, "Wallet unavailable");
            let mut tokens = self
                .tokens
                .lock()
                .unwrap()
                .clone()
                .context("Missing tokens")?;
            if self.corrupt {
                tokens.access_token = "wrong-test-value".into();
            }
            Ok(tokens)
        }
        async fn delete(&self, _: &SessionMetadata) -> Result<()> {
            *self.tokens.lock().unwrap() = None;
            Ok(())
        }
    }

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            // Tests run in parallel and can read the same clock tick, so add a counter.
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let name = format!(
                "thrace-session-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(name);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("session.json")
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (SessionMetadata, MatrixSession) {
        let session = MatrixSession {
            meta: SessionMeta {
                user_id: "@test:example.org".try_into().unwrap(),
                device_id: "TESTDEVICE".into(),
            },
            tokens: SessionTokens {
                access_token: "test-access-secret".into(),
                refresh_token: Some("test-refresh-secret".into()),
            },
        };
        (
            SessionMetadata::new("https://example.org", "/tmp/test-matrix-store", &session),
            session,
        )
    }

    fn legacy(path: &Path, meta: &SessionMetadata, session: &MatrixSession) -> Vec<u8> {
        let data = serde_json::to_vec(&serde_json::json!({"homeserver": meta.homeserver, "store_dir": meta.store_dir, "session": session})).unwrap();
        std::fs::write(path, &data).unwrap();
        data
    }

    #[tokio::test]
    async fn metadata_has_no_tokens_and_restores_the_same_device() {
        let dir = TestDir::new();
        let (meta, session) = fixture();
        let store = MockStore::default();
        save_with(&dir.path(), &meta, &session.tokens, &store)
            .await
            .unwrap();
        let raw = std::fs::read_to_string(dir.path()).unwrap();
        for value in [
            "access_token",
            "refresh_token",
            "test-access-secret",
            "test-refresh-secret",
        ] {
            assert!(!raw.contains(value));
        }
        let (loaded_meta, loaded) = load_with(&dir.path(), &store).await.unwrap().unwrap();
        assert_eq!(loaded_meta, meta);
        assert_eq!(loaded, session);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[tokio::test]
    async fn successful_migration_removes_plaintext_only_after_wallet_roundtrip() {
        let dir = TestDir::new();
        let (meta, session) = fixture();
        legacy(&dir.path(), &meta, &session);
        let store = MockStore::default();
        let (_, restored) = load_with(&dir.path(), &store).await.unwrap().unwrap();
        assert_eq!(restored, session);
        assert!(!std::fs::read_to_string(dir.path())
            .unwrap()
            .contains("test-access-secret"));
        assert_eq!(
            store.tokens.lock().unwrap().as_ref().unwrap(),
            &session.tokens
        );
    }

    #[tokio::test]
    async fn failed_or_incorrect_wallet_save_preserves_legacy_session() {
        let dir = TestDir::new();
        let (meta, session) = fixture();
        let original = legacy(&dir.path(), &meta, &session);
        for store in [
            MockStore {
                fail: true,
                ..Default::default()
            },
            MockStore {
                corrupt: true,
                ..Default::default()
            },
        ] {
            assert!(load_with(&dir.path(), &store).await.is_err());
            assert_eq!(std::fs::read(dir.path()).unwrap(), original);
        }
    }

    #[tokio::test]
    async fn unavailable_wallet_never_creates_plaintext_or_deletes_metadata() {
        let dir = TestDir::new();
        let (meta, session) = fixture();
        let unavailable = MockStore {
            fail: true,
            ..Default::default()
        };
        assert!(save_with(&dir.path(), &meta, &session.tokens, &unavailable)
            .await
            .is_err());
        assert!(!dir.path().exists());
        write_metadata(&dir.path(), &meta).unwrap();
        let original = std::fs::read(dir.path()).unwrap();
        assert!(load_with(&dir.path(), &unavailable).await.is_err());
        assert_eq!(std::fs::read(dir.path()).unwrap(), original);
        forget_file(&dir.path()).unwrap();
        assert!(!dir.path().exists());
    }

    #[test]
    fn credential_attributes_separate_devices_and_accounts() {
        let (meta, _) = fixture();
        let mut other = meta.clone();
        other.meta.device_id = "OTHER".into();
        assert_ne!(meta.attributes(), other.attributes());
        other = meta.clone();
        other.homeserver = "https://other.example".into();
        assert_ne!(meta.attributes(), other.attributes());
        assert_eq!(meta.attributes()["application"], "thrace");
    }

    #[tokio::test]
    #[ignore = "Requires the user's desktop Secret Service; uses only disposable test credentials"]
    async fn desktop_secret_service_roundtrip() {
        let (mut meta, session) = fixture();
        meta.meta.user_id = "@thrace-credential-test:invalid".try_into().unwrap();
        meta.meta.device_id = format!("THRACE-TEST-{}", std::process::id()).into();
        let store = DesktopSecrets;
        store.put(&meta, &session.tokens).await.unwrap();
        let read = store.get(&meta).await;
        let deleted = store.delete(&meta).await;
        assert_eq!(read.unwrap(), session.tokens);
        deleted.unwrap();
        assert!(store.get(&meta).await.is_err());
    }
}

//! Where the Assistant's API key lives: the OS credential store (service `CADCraft`, user
//! `assistant-api-key`), falling back to the preferences file when there is none. [`ApiKey`] holds
//! the policy; [`KeyStore`] is the seam tests replace so they never touch the real store.

/// The credential's service name.
pub const SERVICE: &str = "CADCraft";
/// The credential's user name.
pub const USER: &str = "assistant-api-key";

/// One secret in some credential store. Errors are texts for the settings window.
pub trait KeyStore: Send {
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, key: &str) -> Result<(), String>;
    /// Remove the secret (absent is fine).
    fn delete(&self) -> Result<(), String>;
}

/// Windows Credential Manager, macOS Keychain or the Linux/BSD Secret Service.
#[cfg(not(target_arch = "wasm32"))]
pub struct OsStore;

#[cfg(not(target_arch = "wasm32"))]
impl OsStore {
    fn entry() -> Result<keyring::v1::Entry, String> {
        keyring::v1::Entry::new(SERVICE, USER).map_err(|e| e.to_string())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl KeyStore for OsStore {
    fn get(&self) -> Result<Option<String>, String> {
        match Self::entry()?.get_password() {
            Ok(k) => Ok(Some(k)),
            Err(keyring::v1::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, key: &str) -> Result<(), String> {
        Self::entry()?.set_password(key).map_err(|e| e.to_string())
    }

    fn delete(&self) -> Result<(), String> {
        match Self::entry()?.delete_credential() {
            Ok(()) | Err(keyring::v1::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The browser has no credential store: the key stays in the preferences.
#[cfg(target_arch = "wasm32")]
pub struct OsStore;

#[cfg(target_arch = "wasm32")]
impl KeyStore for OsStore {
    fn get(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn set(&self, _: &str) -> Result<(), String> {
        Err("the browser has no credential store".into())
    }
    fn delete(&self) -> Result<(), String> {
        Ok(())
    }
}

/// The key between the settings window and the credential store. The window keeps the key being
/// edited; this moves it to the store and says what the preferences may keep.
pub struct ApiKey {
    store: Box<dyn KeyStore>,
    /// The key as last handled (in the store, or in the fallback).
    synced: String,
    loaded: bool,
    /// The store failed: the preferences keep the key.
    plain: bool,
    warning: Option<String>,
}

impl ApiKey {
    pub fn new(store: Box<dyn KeyStore>) -> Self {
        ApiKey { store, synced: String::new(), loaded: false, plain: false, warning: None }
    }

    /// The OS credential store.
    pub fn os() -> Self {
        Self::new(Box::new(OsStore))
    }

    /// Why the key is kept in the preferences file instead of the credential store (one line), if it is.
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    fn fall_back(&mut self, e: &str) {
        self.plain = true;
        self.warning = Some(e.lines().next().unwrap_or_default().trim().to_string());
    }

    /// The preferences hold `key`: a plain-text key from an older version, or the fallback. Move it
    /// into the store now, so the preferences written next no longer carry it.
    pub fn adopt(&mut self, key: &str) {
        if !key.is_empty() {
            self.loaded = true;
            self.synced.clear();
            self.save(key);
        }
    }

    /// The key in the store, the first time this is called (later calls: `None`). Call before the
    /// first use of the key, then [`ApiKey::save`] whenever it was edited.
    pub fn load(&mut self) -> Option<String> {
        if std::mem::replace(&mut self.loaded, true) {
            return None;
        }
        match self.store.get() {
            Ok(Some(k)) => {
                self.synced = k.clone();
                Some(k)
            }
            Ok(None) => None,
            Err(e) => {
                self.fall_back(&e);
                None
            }
        }
    }

    /// `key` is what the user wants kept: write it to the store (an empty one removes it) unless it
    /// already is there. When the store refuses, the key goes with the preferences instead.
    pub fn save(&mut self, key: &str) {
        if key == self.synced {
            return;
        }
        let r = if key.is_empty() { self.store.delete() } else { self.store.set(key) };
        // A failed write is not retried until the key changes again.
        self.synced = key.to_string();
        match r {
            Ok(()) => {
                self.plain = false;
                self.warning = None;
            }
            Err(e) => self.fall_back(&e),
        }
    }

    /// What the preferences file may hold for `key`: nothing while the store has it.
    pub fn for_prefs<'a>(&self, key: &'a str) -> &'a str {
        if self.plain { key } else { "" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// An in-memory store (or a broken one) that records what it was asked.
    #[derive(Clone, Default)]
    struct Fake {
        secret: Arc<Mutex<Option<String>>>,
        broken: bool,
        calls: Arc<Mutex<u32>>,
    }

    impl Fake {
        fn tick(&self) -> Result<(), String> {
            *self.calls.lock().unwrap() += 1;
            if self.broken { Err("no secret service\nsecond line".into()) } else { Ok(()) }
        }
    }

    impl KeyStore for Fake {
        fn get(&self) -> Result<Option<String>, String> {
            self.tick().map(|()| self.secret.lock().unwrap().clone())
        }
        fn set(&self, key: &str) -> Result<(), String> {
            self.tick().map(|()| *self.secret.lock().unwrap() = Some(key.into()))
        }
        fn delete(&self) -> Result<(), String> {
            self.tick().map(|()| *self.secret.lock().unwrap() = None)
        }
    }

    #[test]
    fn a_plain_key_from_the_preferences_moves_into_the_store() {
        let fake = Fake::default();
        let mut k = ApiKey::new(Box::new(fake.clone()));
        k.adopt("sk-1");
        assert_eq!(fake.secret.lock().unwrap().as_deref(), Some("sk-1"));
        assert_eq!(k.for_prefs("sk-1"), "");
        assert!(k.warning().is_none());
        // Nothing new to load afterwards, and an unchanged key is not written again.
        assert_eq!(k.load(), None);
        let n = *fake.calls.lock().unwrap();
        k.save("sk-1");
        assert_eq!(*fake.calls.lock().unwrap(), n);
        // Edited and cleared keys follow.
        k.save("sk-2");
        assert_eq!(fake.secret.lock().unwrap().as_deref(), Some("sk-2"));
        k.save("");
        assert_eq!(*fake.secret.lock().unwrap(), None);
    }

    #[test]
    fn the_stored_key_is_loaded_once() {
        let fake = Fake { secret: Arc::new(Mutex::new(Some("stored".into()))), ..Fake::default() };
        let mut k = ApiKey::new(Box::new(fake));
        assert_eq!(k.load().as_deref(), Some("stored"));
        assert_eq!(k.load(), None);
        // Adopting an empty prefs key does nothing.
        k.adopt("");
        assert_eq!(k.for_prefs("stored"), "");
    }

    #[test]
    fn an_unavailable_store_falls_back_to_the_preferences_with_one_warning_line() {
        let fake = Fake { broken: true, ..Fake::default() };
        let mut k = ApiKey::new(Box::new(fake.clone()));
        assert_eq!(k.load(), None);
        assert!(k.warning().is_some_and(|w| w.contains("no secret service") && !w.contains('\n')), "{:?}", k.warning());
        k.save("typed");
        assert_eq!(k.for_prefs("typed"), "typed");
        // The failed write is not repeated every frame.
        let n = *fake.calls.lock().unwrap();
        k.save("typed");
        assert_eq!(*fake.calls.lock().unwrap(), n);
        // A key adopted from the preferences stays there.
        let mut k = ApiKey::new(Box::new(fake));
        k.adopt("old");
        assert_eq!(k.for_prefs("old"), "old");
        assert!(k.warning().is_some());
    }

    #[test]
    fn the_store_coming_back_ends_the_fallback() {
        let fake = Fake::default();
        let mut k = ApiKey::new(Box::new(Fake { broken: true, ..fake.clone() }));
        k.save("a");
        assert_eq!(k.for_prefs("a"), "a");
        k.store = Box::new(fake.clone());
        k.save("b");
        assert_eq!((k.for_prefs("b"), k.warning()), ("", None));
        assert_eq!(fake.secret.lock().unwrap().as_deref(), Some("b"));
    }
}

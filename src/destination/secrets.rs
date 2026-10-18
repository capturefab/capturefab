//! Credential-store writes that accompany a destination save: a typed S3
//! secret is stored under its access key ID while the destination list is
//! locked, and put back as it was when the list cannot be written, so a
//! failed save never leaves a changed credential behind. Undo and redo
//! restore it only if nothing else changed it since.
//!
//! Keychain calls can wait on a credential prompt, so they run off the GUI
//! thread. Without S3 support there is no bucket to store a secret for, and
//! the keychain calls do nothing.
use super::{Destination, Target};
use crate::volumes;
use anyhow::Result;
use std::{
    collections::BTreeMap,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Mutex, PoisonError},
};
use zeroize::Zeroizing;

/// For each access key ID, the ticket of the newest write that reached the
/// credential store.
static SECRET_WRITES: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
static NEXT_SECRET_WRITE: AtomicU64 = AtomicU64::new(1);

/// A secret stored by a save, and the one it replaced, for undo and redo.
/// Both are cleared from memory when the change drops, and left out of Debug.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SecretChange {
    access_key_id: String,
    before: Option<Zeroizing<String>>,
    after: Zeroizing<String>,
}

impl SecretChange {
    #[cfg(all(test, feature = "gui"))]
    pub(crate) fn new(access_key_id: &str, before: Option<&str>, after: &str) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            before: before.map(|secret| Zeroizing::new(secret.into())),
            after: Zeroizing::new(after.into()),
        }
    }
}

impl fmt::Debug for SecretChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretChange")
            .field("access_key_id", &self.access_key_id)
            .field("previously_stored", &self.before.is_some())
            .finish_non_exhaustive()
    }
}

/// A typed secret to store under its access key ID, numbered (on the GUI
/// thread, for the editor) in the order stores were asked for.
pub(crate) struct SecretWrite {
    access_key_id: String,
    secret: Zeroizing<String>,
    ticket: u64,
}

impl SecretWrite {
    pub(crate) fn queue(secret: Option<(String, Zeroizing<String>)>) -> Option<Self> {
        secret.map(|(access_key_id, secret)| Self {
            access_key_id,
            secret,
            ticket: NEXT_SECRET_WRITE.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Store with `store`, one write at a time, unless a store asked for
    /// later already wrote this key: an editor drops a store it no longer
    /// wants, but its thread may still be waiting on a keychain prompt, and
    /// must not land over the newer secret.
    fn store_in_order<T>(
        self,
        written: &Mutex<BTreeMap<String, u64>>,
        store: impl FnOnce(&str, &str) -> Result<T>,
    ) -> Result<T> {
        let mut written = written.lock().unwrap_or_else(PoisonError::into_inner);
        if written
            .get(&self.access_key_id)
            .is_some_and(|&ticket| ticket > self.ticket)
        {
            return Err(anyhow::anyhow!(
                "The credential write was superseded; save again"
            ));
        }
        let result = store(&self.access_key_id, &self.secret)?;
        written.insert(self.access_key_id, self.ticket);
        Ok(result)
    }
}

/// Store a typed secret under its access key ID in the OS credential store;
/// whether one was. It can wait on a keychain prompt, so off the UI thread.
pub(crate) fn store_secret(write: Option<SecretWrite>) -> Result<Option<SecretChange>> {
    // Without S3 support there is no bucket to store a secret for.
    let Some(write) = write.filter(|_| cfg!(feature = "s3")) else {
        return Ok(None);
    };
    write
        .store_in_order(&SECRET_WRITES, |access_key_id, secret| {
            let before = keychain_read(access_key_id)?.map(Zeroizing::new);
            keychain_store(access_key_id, secret)?;
            Ok(SecretChange {
                access_key_id: access_key_id.into(),
                before,
                after: Zeroizing::new(secret.into()),
            })
        })
        .map(Some)
}

/// Undo (or redo) a stored secret, unless the credential store no longer
/// holds the value the change left there.
pub(crate) fn restore_secret(change: &SecretChange, undo: bool) -> Result<()> {
    let mut written = SECRET_WRITES.lock().unwrap_or_else(PoisonError::into_inner);
    restore_secret_using(change, undo, keychain_read, |id, secret| match secret {
        Some(secret) => keychain_store(id, secret),
        None => keychain_delete(id),
    })?;
    written.insert(
        change.access_key_id.clone(),
        NEXT_SECRET_WRITE.fetch_add(1, Ordering::Relaxed),
    );
    Ok(())
}

fn restore_secret_using(
    change: &SecretChange,
    undo: bool,
    read: impl FnOnce(&str) -> Result<Option<String>>,
    write: impl FnOnce(&str, Option<&str>) -> Result<()>,
) -> Result<()> {
    let before = change.before.as_ref().map(|secret| secret.as_str());
    let after = Some(change.after.as_str());
    let (expected, desired) = if undo {
        (after, before)
    } else {
        (before, after)
    };
    let current = read(&change.access_key_id)?.map(Zeroizing::new);
    anyhow::ensure!(
        current.as_ref().map(|secret| secret.as_str()) == expected,
        "the stored credential changed outside this edit; undo cannot overwrite it"
    );
    write(&change.access_key_id, desired)
}

/// Make a folder destination's directory, then save `after` in place of
/// `before` (and the selection, as `replace_with_selection` does) while
/// storing `write`'s secret, which is restored if the list cannot be written.
/// The secret change, if one was stored.
pub(crate) fn save_with_secret(
    before: Option<&Destination>,
    after: &Destination,
    selection: Option<(Option<&str>, bool)>,
    write: Option<SecretWrite>,
) -> Result<Option<SecretChange>> {
    if let Target::Folder { path } = &after.target {
        volumes::ensure_present(path)?;
        std::fs::create_dir_all(path)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", path.display()))?;
    }
    super::replace_with_selection(
        before,
        Some(after),
        selection,
        || store_secret(write),
        |change| match change {
            Some(change) => restore_secret(change, true),
            None => Ok(()),
        },
    )
}

#[cfg(feature = "s3")]
use crate::s3::store_secret as keychain_store;
#[cfg(feature = "s3")]
use crate::s3::{delete_secret as keychain_delete, read_secret as keychain_read};

#[cfg(not(feature = "s3"))]
fn keychain_store(_access_key_id: &str, _secret: &str) -> Result<()> {
    Ok(())
}
#[cfg(not(feature = "s3"))]
fn keychain_read(_access_key_id: &str) -> Result<Option<String>> {
    Ok(None)
}
#[cfg(not(feature = "s3"))]
fn keychain_delete(_access_key_id: &str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_superseded_secret_store_never_lands_over_a_newer_one() {
        let queue = |id: &str, secret: &str| {
            SecretWrite::queue(Some((id.into(), Zeroizing::new(secret.into())))).unwrap()
        };
        let older = queue("AKIA", "old");
        let newer = queue("AKIA", "new");
        let other = queue("AKIB", "b");
        let written = Mutex::new(BTreeMap::new());
        let stored = std::cell::RefCell::new(Vec::new());
        let store = |id: &str, secret: &str| {
            stored.borrow_mut().push(format!("{id}={secret}"));
            Ok(())
        };
        // The newer store's thread runs first; the older one then finds it
        // superseded, while an unrelated key still stores.
        newer.store_in_order(&written, store).unwrap();
        assert!(older.store_in_order(&written, store).is_err());
        other.store_in_order(&written, store).unwrap();
        assert_eq!(*stored.borrow(), ["AKIA=new", "AKIB=b"]);
        // In order, both store and the newer one is last.
        let (first, second) = (queue("AKIC", "1"), queue("AKIC", "2"));
        first.store_in_order(&written, store).unwrap();
        second.store_in_order(&written, store).unwrap();
        assert_eq!(stored.borrow()[2..], ["AKIC=1", "AKIC=2"]);
    }

    #[test]
    fn credential_undo_redo_restore_old_values_without_overwriting_external_changes() {
        let change = SecretChange {
            access_key_id: "AKIA".into(),
            before: Some(Zeroizing::new("old".into())),
            after: Zeroizing::new("new".into()),
        };
        let current = std::cell::RefCell::new(Some("new".to_string()));
        let read = |_: &str| Ok(current.borrow().clone());
        let write = |_: &str, value: Option<&str>| {
            *current.borrow_mut() = value.map(str::to_owned);
            Ok(())
        };
        restore_secret_using(&change, true, read, write).unwrap();
        assert_eq!(current.borrow().as_deref(), Some("old"));
        restore_secret_using(&change, false, read, write).unwrap();
        assert_eq!(current.borrow().as_deref(), Some("new"));
        *current.borrow_mut() = Some("external".into());
        assert!(restore_secret_using(&change, true, read, write).is_err());
        assert_eq!(current.borrow().as_deref(), Some("external"));
        assert!(
            restore_secret_using(&change, true, |_| Err(anyhow::anyhow!("locked")), write).is_err()
        );
        assert_eq!(current.borrow().as_deref(), Some("external"));
        let new_key = SecretChange {
            before: None,
            ..change
        };
        *current.borrow_mut() = Some("new".into());
        restore_secret_using(&new_key, true, read, write).unwrap();
        assert!(
            current.borrow().is_none(),
            "undoing a new key removes only that key"
        );
        restore_secret_using(&new_key, false, read, write).unwrap();
        assert_eq!(current.borrow().as_deref(), Some("new"));
    }

    #[test]
    fn secret_changes_leave_secrets_out_of_debug() {
        let change = SecretChange {
            access_key_id: "AKIA".into(),
            before: Some(Zeroizing::new("private-old".into())),
            after: Zeroizing::new("private-new".into()),
        };
        let text = format!("{change:?}");
        assert!(text.contains("AKIA") && !text.contains("private"), "{text}");
    }
}

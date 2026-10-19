//! Session-owned, keyed commitments. Receipts from another coordinator fail.
use super::{AuthBinding, BindingReceipt, ProfileRef, Secret, Store, random_id, validate_username};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock, PoisonError},
};
use zeroize::Zeroizing;

/// Helper-stdin credentials only. Never put these in a SessionCommand.
#[derive(Clone)]
pub struct SignalCredentials {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}
impl std::fmt::Debug for SignalCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignalCredentials(<redacted>)")
    }
}
impl SignalCredentials {
    fn new(username: &str, password: &Secret) -> Result<Self> {
        validate_username(username)?;
        Ok(Self {
            username: username.to_owned().into(),
            password: password.expose().to_owned().into(),
        })
    }
    pub(crate) fn apply(&self, uri: &mut crate::signals::SignalUri) -> Result<()> {
        ensure!(
            matches!(
                uri.protocol,
                crate::signals::Protocol::Mqtt | crate::signals::Protocol::Opcua
            ) && uri.user.is_none()
                && uri.password.is_none(),
            "helper credentials require a credential-free MQTT or OPC UA endpoint"
        );
        uri.user = Some(self.username.to_string());
        uri.password = Some(self.password.to_string());
        Ok(())
    }
    pub(crate) fn redact(&self, text: &str) -> String {
        // Scrub against the original text, so replacement markers cannot
        // accidentally hide a second, overlapping credential.
        let mut ranges: Vec<_> = self.username.match_indices_in(text).collect();
        ranges.extend(self.password.match_indices_in(text));
        ranges.sort_unstable();
        let mut result = String::new();
        let mut copied = 0;
        let mut end = 0;
        for (start, length) in ranges {
            if start < end {
                end = end.max(start + length);
                continue;
            }
            if end > copied {
                result.push_str("<redacted>");
                copied = end;
            }
            result.push_str(&text[copied..start]);
            copied = start;
            end = start + length;
        }
        if end > copied {
            result.push_str("<redacted>");
            copied = end;
        }
        result.push_str(&text[copied..]);
        result
    }
    pub(crate) fn scrub_line(&self, line: &mut crate::signals::Line) {
        use crate::signals::{Line, Value};
        let text = |value: &mut String| *value = self.redact(value);
        let option = |value: &mut Option<String>| {
            if let Some(value) = value {
                text(value)
            }
        };
        let value = |value: &mut Value| {
            if let Value::Text(value) = value {
                text(value)
            }
        };
        match line {
            Line::Ready { tags } => {
                for tag in tags {
                    text(&mut tag.tag);
                    option(&mut tag.unit);
                    option(&mut tag.kind);
                }
            }
            Line::Samples { samples } => {
                for sample in samples {
                    text(&mut sample.tag);
                    value(&mut sample.value);
                }
            }
            Line::State { error, .. } => {
                option(error);
            }
            Line::Error { message } => text(message),
            Line::Browse { items } => {
                for item in items {
                    text(&mut item.id);
                    text(&mut item.name);
                    text(&mut item.kind);
                    option(&mut item.data_type);
                    option(&mut item.unit);
                    if let Some(item) = &mut item.value {
                        value(item)
                    }
                }
            }
            Line::Done { .. } | Line::Dropped { .. } => {}
        }
    }
}
trait Matches {
    fn match_indices_in<'a>(
        &'a self,
        text: &'a str,
    ) -> Box<dyn Iterator<Item = (usize, usize)> + 'a>;
}
impl Matches for str {
    fn match_indices_in<'a>(
        &'a self,
        text: &'a str,
    ) -> Box<dyn Iterator<Item = (usize, usize)> + 'a> {
        Box::new(
            text.match_indices(self)
                .map(|(start, found)| (start, found.len())),
        )
    }
}
impl Serialize for SignalCredentials {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("SignalCredentials", 2)?;
        value.serialize_field("username", self.username.as_str())?;
        value.serialize_field("password", self.password.as_str())?;
        value.end()
    }
}
impl<'de> Deserialize<'de> for SignalCredentials {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            #[serde(deserialize_with = "protected_text")]
            username: Zeroizing<String>,
            #[serde(deserialize_with = "protected_text")]
            password: Zeroizing<String>,
        }
        let input = Input::deserialize(deserializer)?;
        let secret = Secret::new(input.password).map_err(serde::de::Error::custom)?;
        Self::new(&input.username, &secret).map_err(serde::de::Error::custom)
    }
}

fn protected_text<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Zeroizing<String>, D::Error> {
    String::deserialize(deserializer).map(Zeroizing::new)
}

struct Bound {
    receipt: BindingReceipt,
    commitment: [u8; 32],
}
pub struct Session {
    store: Store,
    key: OnceLock<Zeroizing<[u8; 32]>>,
    bindings: Mutex<BTreeMap<String, Bound>>,
}
impl Session {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            key: OnceLock::new(),
            bindings: Mutex::default(),
        }
    }
    fn key(&self) -> Result<&[u8; 32]> {
        if self.key.get().is_none() {
            let mut key = Zeroizing::new([0u8; 32]);
            getrandom::fill(key.as_mut())
                .map_err(|_| anyhow!("cannot create a signal credential generation"))?;
            let _ = self.key.set(key);
        }
        Ok(self.key.get().expect("initialized credential generation"))
    }
    /// Explicit local consent action, off the UI thread. Always creates a
    /// new epoch; one active credential choice is allowed per endpoint.
    pub fn bind(&self, profile: &ProfileRef) -> Result<BindingReceipt> {
        self.store.with_current(profile, |record| {
            let password = self.store.resolve(record)?;
            let commitment = commitment(self.key()?, record.username.as_deref(), password.as_ref());
            let receipt = BindingReceipt {
                endpoint: record.endpoint.clone(),
                reference: crate::signals::source_id(&record.endpoint),
                auth: AuthBinding {
                    profile: record.profile.clone(),
                    epoch: random_id()?,
                },
            };
            let mut bindings = self.bindings.lock().unwrap_or_else(PoisonError::into_inner);
            bindings.retain(|id, bound| {
                id != &profile.id && bound.receipt.reference != receipt.reference
            });
            bindings.insert(
                profile.id.clone(),
                Bound {
                    receipt: receipt.clone(),
                    commitment,
                },
            );
            Ok(receipt)
        })
    }
    /// This is cached-only and performs no file, environment or OS-store IO.
    pub fn cached_bindings(&self) -> Vec<BindingReceipt> {
        self.bindings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|bound| bound.receipt.clone())
            .collect()
    }
    pub fn validate_cached(&self, binding: &AuthBinding, endpoint: &str) -> Result<()> {
        binding.validate()?;
        let bindings = self.bindings.lock().unwrap_or_else(PoisonError::into_inner);
        ensure!(
            bindings.get(&binding.profile.id).is_some_and(
                |bound| bound.receipt.auth == *binding && bound.receipt.endpoint == endpoint
            ),
            "the signal credential binding is stale or belongs to another coordinator; bind it locally and approve it again"
        );
        Ok(())
    }
    /// Hold the profile lifecycle through helper stdin. No changed metadata
    /// or secret can be adopted by an old approved epoch or helper restart.
    pub(crate) fn with_credentials<T>(
        &self,
        binding: &AuthBinding,
        endpoint: &str,
        run: impl FnOnce(Option<SignalCredentials>) -> Result<T>,
    ) -> Result<T> {
        self.validate_cached(binding, endpoint)?;
        let mut credentials_valid = false;
        let result = self.store.with_current(&binding.profile, |record| {
            self.validate_cached(binding, endpoint)?;
            ensure!(
                record.endpoint == endpoint,
                "the signal profile endpoint changed; bind it locally again"
            );
            let password = self.store.resolve(record)?;
            let current = commitment(self.key()?, record.username.as_deref(), password.as_ref());
            let mut bindings = self.bindings.lock().unwrap_or_else(PoisonError::into_inner);
            let bound = bindings
                .get(&binding.profile.id)
                .filter(|bound| bound.receipt.auth == *binding)
                .ok_or_else(|| {
                    anyhow!("the signal credential binding changed; bind it locally again")
                })?;
            if current != bound.commitment {
                bindings.remove(&binding.profile.id);
                anyhow::bail!(
                    "the signal credentials changed; bind the profile locally and approve it again"
                );
            }
            // Keep the binding lock through stdin as well, so a simultaneous
            // local rebind cannot issue its replacement before this launch.
            let credentials = match (record.username.as_deref(), password.as_ref()) {
                (Some(user), Some(password)) => Some(SignalCredentials::new(user, password)?),
                (None, None) => None,
                _ => anyhow::bail!("the signal profile has incomplete credentials"),
            };
            credentials_valid = true;
            run(credentials)
        });
        if result.is_err() && !credentials_valid {
            let mut bindings = self.bindings.lock().unwrap_or_else(PoisonError::into_inner);
            if bindings
                .get(&binding.profile.id)
                .is_some_and(|bound| bound.receipt.auth == *binding)
            {
                bindings.remove(&binding.profile.id);
            }
        }
        result
    }
}

// HMAC-SHA256 over length-delimited credential fields; neither this private
// commitment nor its random coordinator key appears in metadata or receipts.
fn commitment(key: &[u8; 32], username: Option<&str>, password: Option<&Secret>) -> [u8; 32] {
    let mut inner_key = Zeroizing::new([0x36u8; 64]);
    let mut outer_key = Zeroizing::new([0x5cu8; 64]);
    for (index, byte) in key.iter().enumerate() {
        inner_key[index] ^= byte;
        outer_key[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_key.as_slice());
    for field in [username, password.map(Secret::expose)] {
        inner.update([u8::from(field.is_some())]);
        let field = field.unwrap_or_default();
        inner.update((field.len() as u64).to_le_bytes());
        inner.update(field.as_bytes());
    }
    let inner = Zeroizing::new(<[u8; 32]>::from(inner.finalize()));
    let mut outer = Sha256::new();
    outer.update(outer_key.as_slice());
    outer.update(inner.as_slice());
    outer.finalize().into()
}

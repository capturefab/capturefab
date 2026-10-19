//! Short-lived GUI credential drafts. Values never participate in Debug or
//! persisted form state, and their allocation is zeroized when dropped.
use std::fmt;
use zeroize::Zeroizing;

#[derive(Clone, Default)]
pub(super) struct SecretDraft(pub(super) Zeroizing<String>);
impl SecretDraft {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<String> for SecretDraft {
    fn from(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
}
impl fmt::Debug for SecretDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretDraft([redacted])")
    }
}

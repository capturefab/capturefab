//! Optional AI assistant infrastructure. Camera actions remain in `tools`;
//! provider settings and credentials never enter a camera command or model body.
pub mod credentials;
pub mod google;
pub mod http;
pub mod limits_defaults;
pub mod provider;
pub mod settings;
pub mod usage;
pub mod vision;

pub use credentials::{Auth, CredentialStatus, ResolvedCredentials};
pub use http::{Http, ProviderError, ProviderErrorKind};
pub use limits_defaults::Limits;
pub use provider::{Effort, Protocol, ProviderId, ProviderSettings};
pub use settings::Settings;
pub use usage::{Prices, Usage};
pub use vision::{VisionDecision, VisionMode};

use serde_json::{Value, json};

/// Environment presence only: this never reads a keychain, ADC file or cloud
/// credential chain, or makes a request.
pub fn doctor() -> Value {
    json!({"providers":provider::ProviderId::ALL.iter().map(|id| {
        let info=id.info();
        let configured=info.key_envs.iter().any(|name|std::env::var_os(name).is_some_and(|value|!value.is_empty()));
        json!({"id":id.id(),"env_configured":configured})
    }).collect::<Vec<_>>()})
}

pub fn schema_contract() -> Value {
    json!({"providers":provider::ProviderId::ALL.iter().map(|id| {
        let info=id.info();
        json!({"id":id.id(),"name":info.name,"protocol":info.protocol,"env_vars":info.key_envs,"settings":info.settings,"default_model":info.default_model})
    }).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn doctor_is_presence_only_and_schema_names_all_providers() {
        let doctor = doctor();
        let entries = doctor["providers"].as_array().unwrap();
        assert_eq!(entries.len(), 22);
        for (entry, id) in entries.iter().zip(provider::ProviderId::ALL) {
            assert_eq!(entry["id"], id.id());
            assert!(entry["env_configured"].is_boolean());
            assert_eq!(entry.as_object().unwrap().len(), 2);
        }
        let schema = schema_contract();
        let providers = schema["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 22);
        for (entry, id) in providers.iter().zip(provider::ProviderId::ALL) {
            assert_eq!(entry["id"], id.id());
            assert!(entry["env_vars"].is_array());
            assert!(entry["settings"].is_array());
        }
    }
}

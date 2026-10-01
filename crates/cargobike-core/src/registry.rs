//! Provider registry and credential store (F-39, F-146).
//!
//! Both live here so steps can receive them without pulling an I/O
//! crate into `cargobike-core` (A.14). The server constructs them; steps
//! consume them.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use secrecy::SecretString;

use crate::error::LibraryError;
use crate::model::RepoRef;

pub type RegistryResult<T> = Result<T, LibraryError>;

/// Resolve providers by `RepoRef.provider` name (F-39).
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn crate::provider::Provider>>,
}

impl ProviderRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a provider under its config name; a later insert replaces
    /// an earlier one with the same name (config warn: duplicate names error).
    pub fn insert(&mut self, name: &str, provider: Arc<dyn crate::provider::Provider>) {
        self.providers.insert(name.to_owned(), provider);
    }

    /// Resolves the provider named by `repo.provider` (F-39).
    pub fn resolve(&self, repo: &RepoRef) -> RegistryResult<Arc<dyn crate::provider::Provider>> {
        self.providers
            .get(&repo.provider)
            .cloned()
            .ok_or(LibraryError::UnknownProvider(repo.provider.clone()))
    }

    /// The number of registered providers; a test convenience.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether no provider is registered.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// Resolves named secrets (`{ secret: <name> }`, F-146) to their values.
///
/// The value is returned as [`SecretString`] so a step can use it without
/// producing it in logs or step outputs (F-87, F-120).
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Resolves a named secret, or errors when the name is unknown.
    fn resolve(&self, name: &str) -> Result<SecretString, LibraryError>;
}

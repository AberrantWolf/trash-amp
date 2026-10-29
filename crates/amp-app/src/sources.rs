//! Provider-neutral source lifecycle contracts. Catalog owns jobs and selection.
use crate::catalog::ExternalCatalog;
use anyhow::Result;
use std::sync::Arc;

/// Stable opaque identity; never a picker position or a media path.
pub type SourceId = String;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceDescriptor {
    pub id: SourceId,
    pub name: String,
    pub provider: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessStatus {
    Authorized,
    NotDetermined,
    Denied,
    Restricted,
}

/// Cached projection changes (downloads/credentials) do not require network work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefreshKind {
    Cached,
    Live,
}
#[derive(Clone, Debug)]
pub struct SourceChange {
    pub id: SourceId,
    pub refresh: RefreshKind,
}
pub type SourceObserver = Arc<dyn Fn(SourceChange) + Send + Sync>;

/// Load/cache/artwork methods run on job threads. Descriptors and access status
/// must be quick, must not request permission, and may run on the catalog owner.
pub trait SourceProvider: Send + Sync {
    fn descriptors(&self) -> Result<Vec<SourceDescriptor>>;
    fn legacy_selection(&self) -> Result<Option<String>> {
        Ok(None)
    }
    fn access_status(&self) -> AccessStatus {
        AccessStatus::Authorized
    }
    fn catalog_cache(&self) -> bool {
        true
    }
    fn cached(&self, _id: &str) -> Result<Option<ExternalCatalog>> {
        Ok(None)
    }
    fn project_cached(&self, id: &str, _online: Option<bool>) -> Result<Option<ExternalCatalog>> {
        self.cached(id)
    }
    fn load(
        &self,
        id: &str,
        interactive: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ExternalCatalog>;
    fn has_artwork(&self) -> bool {
        false
    }
    fn artwork(
        &self,
        _catalog: &ExternalCatalog,
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<std::collections::BTreeMap<String, String>>> {
        Ok(None)
    }
    /// False prevents cached metadata from being exposed after permission revocation.
    fn cache_allowed(&self) -> bool {
        self.access_status() == AccessStatus::Authorized
    }
    fn revoke_cached_data(&self) -> Result<()> {
        Ok(())
    }
    fn observe(&self, _changed: SourceObserver) {}
}

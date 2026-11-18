//! Collector view data read on a worker and handed to the UI by stable source ID.
use super::*;

pub struct CatalogView {
    pub library: Arc<Library>,
    pub artwork: Arc<BTreeMap<String, String>>,
    pub collections: Vec<api::ClientCollection>,
    pub lists: Vec<api::CollectionListView>,
    pub album_collections: BTreeMap<String, ids::CollectionId>,
    pub updated: u64,
    scoped_library: Option<(ids::CollectionId, Arc<Library>)>,
}

impl CatalogView {
    pub fn library_for(&self, collection: Option<ids::CollectionId>) -> Arc<Library> {
        let Some(collection) = collection else {
            return self.library.clone();
        };
        if let Some((cached, library)) = &self.scoped_library
            && *cached == collection
        {
            return library.clone();
        }
        Arc::new(Library {
            albums: self
                .library
                .albums
                .iter()
                .filter(|album| self.album_collections.get(&album.id.item) == Some(&collection))
                .cloned()
                .collect(),
            scan: self.library.scan.clone(),
            id: self.library.id.clone(),
            root: self.library.root.clone(),
            warnings: self.library.warnings.clone(),
        })
    }
}

impl From<Snapshot> for CatalogView {
    fn from(snapshot: Snapshot) -> Self {
        Self {
            library: Arc::new(snapshot.library),
            artwork: Arc::new(snapshot.artwork),
            collections: snapshot.collections,
            lists: snapshot.lists,
            album_collections: snapshot.album_collections,
            updated: snapshot.updated,
            scoped_library: None,
        }
    }
}

pub struct SourceView {
    pub connections: Vec<SavedConnection>,
    pub selected: Option<ConnectionKey>,
    pub catalog: Option<CatalogView>,
}

impl Adapter {
    pub fn source_view(
        &self,
        snapshot: &amp_app::catalog::Snapshot,
        collection: Option<ids::CollectionId>,
    ) -> Result<SourceView> {
        let connections = self.connections()?;
        let connection = match &snapshot.source {
            amp_app::catalog::Source::External(id) => connections
                .iter()
                .find(|connection| connection_id(connection.key) == *id),
            _ => None,
        };
        let selected = connection.map(|connection| connection.key);
        let catalog = if let Some(connection) = connection {
            let mut library = snapshot.library.clone();
            if library.root != connection.name {
                Arc::make_mut(&mut library).root = connection.name.clone();
            }
            let artwork = snapshot.artwork.clone();
            let staged = {
                let staged = self.staged.lock().unwrap();
                staged.get(&connection.key).map(|(_, source)| CatalogView {
                    library: library.clone(),
                    artwork: artwork.clone(),
                    collections: source.collections.clone(),
                    lists: source.lists.clone(),
                    album_collections: source.album_collections.clone(),
                    updated: source.updated,
                    scoped_library: None,
                })
            };
            if let Some(staged) = staged {
                Some(staged)
            } else {
                self.snapshot(connection.key)?.map(|source| CatalogView {
                    library,
                    artwork,
                    collections: source.collections,
                    lists: source.lists,
                    album_collections: source.album_collections,
                    updated: source.updated,
                    scoped_library: None,
                })
            }
        } else {
            None
        };
        let catalog = catalog.map(|mut view| {
            if let Some(collection) = collection {
                view.scoped_library = Some((collection, view.library_for(Some(collection))));
            }
            view
        });
        Ok(SourceView {
            connections,
            selected,
            catalog,
        })
    }
}

use std::{cell::RefCell, collections::BTreeMap, u64};

use itertools::Itertools;
use log::debug;
use movy_types::error::MovyError;
use serde::{Deserialize, Serialize};
use sui_types::{
    base_types::ObjectID,
    effects::{TransactionEffects, TransactionEffectsAPI},
    inner_temporary_store::InnerTemporaryStore,
    messages_checkpoint::{CheckpointContents, CheckpointSummary},
    object::Object,
    storage::{BackingPackageStore, ChildObjectResolver, ObjectStore, PackageObject, ParentSync},
};

use crate::database::{DexForkedReplayStore, ForkedCheckpoint};

#[auto_impl::auto_impl(&, Arc, Box)]
pub trait ObjectSuiStoreCommit {
    // We are not using &mut self here to keep consistent to sui design (sigh...)
    fn commit_single_object(&self, object: Object) -> Result<(), MovyError>;
    fn commit_store(
        &self,
        store: InnerTemporaryStore,
        effects: &TransactionEffects,
    ) -> Result<(), MovyError>;
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CachedSnapshot {
    pub objects: BTreeMap<ObjectID, BTreeMap<u64, Option<Object>>>,
}

enum ResolvedResult {
    Unbound,
    KnownNotExisting,
    Existing(Object),
}

impl ResolvedResult {
    pub fn into_object(self) -> Option<Object> {
        match self {
            Self::Unbound => None,
            Self::KnownNotExisting => None,
            Self::Existing(obj) => Some(obj),
        }
    }
}

impl CachedSnapshot {
    pub fn object_exact_version(&self, object: &ObjectID, version: u64) -> Option<Object> {
        self.objects
            .get(object)
            .and_then(|t| t.get(&version).cloned())
            .flatten()
    }

    pub fn object_id_cached(&self, object: &ObjectID) -> Option<Object> {
        self.object_resolved_upperbound(object, u64::MAX)
    }

    pub fn object_resolved_upperbound(&self, object: &ObjectID, upbound: u64) -> Option<Object> {
        self.object_resolve_upperbound_details(object, upbound)
            .into_object()
    }

    fn object_resolve_upperbound_details(&self, object: &ObjectID, upbound: u64) -> ResolvedResult {
        self.objects
            .get(object)
            .map(|vmap| {
                let resolved = vmap.range(0..=upbound).max_by_key(|v| v.0);
                if let Some(resolved) = resolved {
                    if let Some(object) = resolved.1.as_ref() {
                        ResolvedResult::Existing(object.clone())
                    } else {
                        ResolvedResult::KnownNotExisting
                    }
                } else {
                    ResolvedResult::Unbound
                }
            })
            .unwrap_or(ResolvedResult::Unbound)
    }

    pub fn cache_object_only(&mut self, object: Object) {
        // Shall we kept the old versions? Seems it does not violate any invariants
        self.cache_query(object.id(), object.version().into(), Some(object));
    }
    pub fn cache_query(&mut self, object_id: ObjectID, version: u64, object: Option<Object>) {
        self.objects
            .entry(object_id)
            .or_default()
            .insert(version, object);
    }
}

#[derive(Debug)]
pub struct CachedStore<T> {
    pub inner: RefCell<CachedSnapshot>,
    pub store: T,
}

impl<T> CachedStore<T> {
    pub fn dump_snapshot(&self) -> CachedSnapshot {
        self.inner.borrow().clone()
    }
    pub fn restore_snapshot(&self, dump: CachedSnapshot) {
        let mut inner = self.inner.borrow_mut();
        for (obj_id, mp) in dump.objects.into_iter() {
            inner
                .objects
                .entry(obj_id)
                .or_default()
                .extend(mp.into_iter());
        }
    }
    pub fn new(db: T) -> Self {
        Self {
            inner: RefCell::new(CachedSnapshot::default()),
            store: db,
        }
    }
}

impl<T: BackingPackageStore> BackingPackageStore for CachedStore<T> {
    fn get_package_object(
        &self,
        package_id: &ObjectID,
    ) -> sui_types::error::SuiResult<Option<sui_types::storage::PackageObject>> {
        let hit = {
            let guard = self.inner.borrow_mut();
            guard.object_id_cached(package_id)
        };

        if let Some(hit) = hit {
            debug!(
                "[CachedStore] get_package_object hit for {}:{}",
                package_id,
                hit.version()
            );
            Ok(Some(PackageObject::new(hit)))
        } else {
            debug!("[CachedStore] get_package_object miss for {}", package_id);
            let hit = self.store.get_package_object(package_id)?;
            if let Some(hit) = &hit {
                let mut guard = self.inner.borrow_mut();
                guard.cache_object_only(hit.object().clone());
            }
            Ok(hit)
        }
    }
}

impl<T: ObjectStore> ObjectStore for CachedStore<T> {
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        let hit = {
            let guard = self.inner.borrow_mut();
            guard.object_id_cached(object_id)
        };

        if let Some(hit) = hit {
            debug!(
                "[CachedStore] get_object hit for {}:{}",
                object_id,
                hit.version()
            );
            Some(hit)
        } else {
            debug!("[CachedStore] get_object miss for {}", object_id);
            let hit = self.store.get_object(object_id)?;
            self.inner.borrow_mut().cache_object_only(hit.clone());
            Some(hit)
        }
    }
    fn get_object_by_key(
        &self,
        object_id: &ObjectID,
        version: sui_types::base_types::VersionNumber,
    ) -> Option<Object> {
        let hit = {
            let guard = self.inner.borrow_mut();
            guard.object_exact_version(object_id, version.into())
        };

        if let Some(hit) = hit {
            debug!(
                "[CachedStore] get_object_by_key hit for {}:{}",
                object_id, version
            );
            Some(hit)
        } else {
            debug!(
                "[CachedStore] get_object_by_key miss for {}:{}",
                object_id, version
            );
            let hit = self.store.get_object_by_key(object_id, version)?;
            self.inner.borrow_mut().cache_object_only(hit.clone());
            Some(hit)
        }
    }
}

impl<T: ParentSync> ParentSync for CachedStore<T> {
    fn get_latest_parent_entry_ref_deprecated(
        &self,
        object_id: ObjectID,
    ) -> Option<sui_types::base_types::ObjectRef> {
        self.store.get_latest_parent_entry_ref_deprecated(object_id)
    }
}

impl<T: ChildObjectResolver> ChildObjectResolver for CachedStore<T> {
    fn get_object_received_at_version(
        &self,
        owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: sui_types::base_types::SequenceNumber,
        epoch_id: sui_types::committee::EpochId,
    ) -> sui_types::error::SuiResult<Option<Object>> {
        let hit = {
            let guard = self.inner.borrow_mut();
            guard.object_exact_version(receiving_object_id, receive_object_at_version.into())
        };

        if let Some(hit) = hit {
            debug!(
                "[CachedStore] get_object_received_at_version hit for {}:{}",
                receiving_object_id, receive_object_at_version
            );
            Ok(Some(hit))
        } else {
            debug!(
                "[CachedStore] get_object_received_at_version miss for {}:{}",
                receiving_object_id, receive_object_at_version
            );
            let hit = self.store.get_object_received_at_version(
                owner,
                receiving_object_id,
                receive_object_at_version,
                epoch_id,
            )?;
            if let Some(hit) = hit {
                self.inner.borrow_mut().cache_object_only(hit.clone());
                Ok(Some(hit))
            } else {
                Ok(None)
            }
        }
    }

    fn read_child_object(
        &self,
        parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: sui_types::base_types::SequenceNumber,
    ) -> sui_types::error::SuiResult<Option<Object>> {
        let hit = {
            let guard = self.inner.borrow_mut();

            guard.object_resolve_upperbound_details(child, child_version_upper_bound.into())
        };

        match hit {
            ResolvedResult::KnownNotExisting => {
                debug!(
                    "[CachedStore] read_child_object not existing for {}:{}",
                    child, child_version_upper_bound,
                );
                return Ok(None);
            }
            ResolvedResult::Existing(hit) => {
                if hit.version() == child_version_upper_bound {
                    debug!(
                        "[CachedStore] read_child_object perfect hit for {}:{}, digest {}",
                        child,
                        child_version_upper_bound,
                        hit.digest()
                    );

                    return Ok(Some(hit));
                } else {
                    debug!(
                        "[CachedStore] read_child_object hit version {} but not the ideal version for {}:{}, digest {}",
                        hit.version(),
                        child,
                        child_version_upper_bound,
                        hit.digest()
                    );
                }
            }
            _ => {
                debug!(
                    "[CachedStore] read_child_object unbound miss for {}:{}",
                    child, child_version_upper_bound,
                );
            }
        }

        debug!(
            "[CachedStore] read_child_object miss for {}:{}",
            child, child_version_upper_bound,
        );
        let hit = self
            .store
            .read_child_object(parent, child, child_version_upper_bound)?;
        if let Some(hit) = hit {
            self.inner.borrow_mut().cache_object_only(hit.clone());
            Ok(Some(hit))
        } else {
            self.inner
                .borrow_mut()
                .cache_query(*child, child_version_upper_bound.into(), None);
            Ok(None)
        }
    }
}

impl<T> ObjectSuiStoreCommit for CachedStore<T> {
    fn commit_single_object(&self, object: Object) -> Result<(), MovyError> {
        let mut guard = self.inner.borrow_mut();
        let id = object.id();
        let version = object.version();
        debug!("[CachedStore] Commit a single object {}:{}", id, version);
        guard.cache_object_only(object);
        Ok(())
    }
    fn commit_store(
        &self,
        mut store: sui_types::inner_temporary_store::InnerTemporaryStore,
        effects: &TransactionEffects,
    ) -> Result<(), MovyError> {
        let mut guard = self.inner.borrow_mut();

        for (id, object) in store.written {
            debug!("[CachedStore] Committing {}:{}", id, object.version());
            guard.cache_object_only(object);
        }

        for (id, version) in effects
            .deleted()
            .into_iter()
            .chain(effects.transferred_from_consensus())
            .chain(effects.consensus_owner_changed())
            .map(|oref| (oref.0, oref.1))
            .filter_map(|(id, version)| store.input_objects.remove(&id).map(|_| (id, version)))
        {
            debug!(
                "[CachedStore] Removing deleted/transferred consensus objects {}:{}",
                id, version
            );
            guard
                .objects
                .entry(id)
                .or_default()
                .insert(version.into(), None);
        }

        let smeared_version = store.lamport_version;
        let deleted_accessed_objects = effects.stream_ended_mutably_accessed_consensus_objects();
        for object_id in deleted_accessed_objects.into_iter() {
            let (id, _) = store
                .input_objects
                .get(&object_id)
                .map(|obj| (obj.id(), obj.version()))
                .unwrap_or_else(|| {
                    let start_version = store.stream_ended_consensus_objects.get(&object_id)
                        .expect("stream-ended object must be in either input_objects or stream_ended_consensus_objects");
                    ( (*object_id).into(), *start_version)
                });
            debug!(
                "[CachedStore] Removing accessed consensus objects {}:{}",
                id, smeared_version
            );
            guard
                .objects
                .entry(id)
                .or_default()
                .insert(smeared_version.into(), None);
        }

        // Optionally prune history objects?
        Ok(())
    }
}

impl<T: ForkedCheckpoint> ForkedCheckpoint for CachedStore<T> {
    fn forked_at(&self) -> u64 {
        self.store.forked_at()
    }
}

impl<T: DexForkedReplayStore> DexForkedReplayStore for CachedStore<T> {
    fn checkpoint(
        &self,
        ckpt: Option<u64>,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Option<(CheckpointContents, CheckpointSummary)>, MovyError>>
                + Send
                + '_,
        >,
    > {
        self.store.checkpoint(ckpt)
    }
    fn dynamic_fields(
        &self,
        table: ObjectID,
        ty: Option<String>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Object>, MovyError>> + Send + '_>> {
        self.store.dynamic_fields(table, ty)
    }

    fn owned_objects(
        &self,
        owner: ObjectID,
        ty: Option<String>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Object>, MovyError>> + Send + '_>> {
        self.store.owned_objects(owner, ty)
    }
}

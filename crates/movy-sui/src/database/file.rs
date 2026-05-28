use std::sync::Arc;

use color_eyre::eyre::eyre;
use mdbx_derive::{
    KeyObjectDecode, KeyObjectEncode, TableObjectDecode, TableObjectEncode, ZstdBcsObject,
    ZstdJSONObject,
};
use movy_types::error::MovyError;
use rocksdb::{
    ColumnFamily, ColumnFamilyDescriptor, DB, Direction, IteratorMode, Options, WriteBatch,
};
use serde::{Deserialize, Serialize};
use sui_types::{
    base_types::{ObjectID, ObjectRef, SequenceNumber, VersionNumber},
    committee::EpochId,
    effects::TransactionEffectsAPI,
    error::{SuiError, SuiErrorKind, SuiResult},
    object::Object,
    storage::{BackingPackageStore, ChildObjectResolver, ObjectStore, PackageObject, ParentSync},
};
use tracing::{debug, warn};

use crate::{
    database::cache::{CachedSnapshot, ObjectSuiStoreCommit},
    schema::{ObjectIDKey, ObjectIDVersionedKey},
};

const CF_OBJECTS: &str = "objects";
const META_KEY: &[u8] = b"__metadata__";

fn rocks_err(e: rocksdb::Error) -> MovyError {
    MovyError::Other(eyre!("rocksdb: {}", e))
}

#[derive(Debug, Serialize, Deserialize, ZstdJSONObject)]
pub struct DatabaseMetadata {
    pub checkpoint: u64,
}

#[derive(Debug, Serialize, Deserialize, ZstdBcsObject)]
pub struct PlainObjectValue {
    pub object: Object,
}

pub struct RocksCachedStore<T> {
    db: Arc<DB>,
    pub ro: bool,
    pub store: T,
}

impl<T: std::fmt::Debug> std::fmt::Debug for RocksCachedStore<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksCachedStore")
            .field("ro", &self.ro)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl<T: Clone> Clone for RocksCachedStore<T> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            ro: self.ro,
            store: self.store.clone(),
        }
    }
}

impl<T> RocksCachedStore<T> {
    pub fn new(db_path: &str, store: T, fork_checkpoint: u64, ro: bool) -> Result<Self, MovyError> {
        let db = if ro {
            let opts = Options::default();
            let cf_names = DB::list_cf(&opts, db_path).map_err(rocks_err)?;
            DB::open_cf_for_read_only(&opts, db_path, &cf_names, false).map_err(rocks_err)?
        } else {
            let mut opts = Options::default();
            opts.create_if_missing(true);
            opts.create_missing_column_families(true);
            let cf_descriptors = vec![
                ColumnFamilyDescriptor::new("default", Options::default()),
                ColumnFamilyDescriptor::new(CF_OBJECTS, Options::default()),
            ];
            DB::open_cf_descriptors(&opts, db_path, cf_descriptors).map_err(rocks_err)?
        };

        if let Some(meta_bytes) = db.get(META_KEY).map_err(rocks_err)? {
            let meta = DatabaseMetadata::table_decode(&meta_bytes)?;
            if meta.checkpoint != fork_checkpoint {
                return Err(eyre!(
                    "cache is intended for {:?} but you want to fork {}",
                    meta.checkpoint,
                    fork_checkpoint
                )
                .into());
            }
        } else if !ro {
            let meta = DatabaseMetadata {
                checkpoint: fork_checkpoint,
            };
            let encoded = meta.table_encode()?;
            db.put(META_KEY, encoded).map_err(rocks_err)?;
        }

        Ok(Self {
            db: Arc::new(db),
            ro,
            store,
        })
    }

    fn cf_objects(&self) -> Result<&ColumnFamily, MovyError> {
        self.db
            .cf_handle(CF_OBJECTS)
            .ok_or_else(|| eyre!("column family '{}' not found", CF_OBJECTS).into())
    }

    pub fn dump_snapshot(&self) -> Result<CachedSnapshot, MovyError> {
        let cf = self.cf_objects()?;
        let iter = self.db.iterator_cf(cf, IteratorMode::Start);
        let mut snap = CachedSnapshot::default();
        for item in iter {
            let (key_bytes, value_bytes) = item.map_err(rocks_err)?;
            let key = ObjectIDVersionedKey::key_decode(&key_bytes)?;
            let value = PlainObjectValue::table_decode(&value_bytes)?;
            snap.objects
                .entry(key.id.into())
                .or_default()
                .entry(key.version)
                .or_insert(Some(value.object));
        }
        Ok(snap)
    }

    pub fn restore_snapshot(&self, snap: CachedSnapshot) -> Result<(), MovyError> {
        if self.ro {
            return Err(eyre!("db in ro, can not restore").into());
        }
        let cf = self.cf_objects()?;
        let mut batch = WriteBatch::default();
        for (_obj, obj_map) in snap.objects {
            for (_version, object) in obj_map {
                if let Some(object) = object {
                    let key = ObjectIDVersionedKey {
                        id: object.id().into(),
                        version: object.version().into(),
                    };
                    let key_bytes = key.key_encode()?;
                    let value_bytes = PlainObjectValue { object }.table_encode()?;
                    batch.put_cf(cf, &key_bytes, &value_bytes);
                }
            }
        }
        self.db.write(batch).map_err(rocks_err)?;
        Ok(())
    }

    fn put_object(&self, object: &Object) -> Result<(), MovyError> {
        if self.ro {
            return Ok(());
        }
        let cf = self.cf_objects()?;
        let key = ObjectIDVersionedKey {
            id: object.id().into(),
            version: object.version().into(),
        };
        let key_bytes = key.key_encode()?;
        let value_bytes = PlainObjectValue {
            object: object.clone(),
        }
        .table_encode()?;
        self.db
            .put_cf(cf, &key_bytes, &value_bytes)
            .map_err(rocks_err)?;
        debug!("[RocksCachedStore] cache object {}", object.id());
        Ok(())
    }

    pub fn get_object_upperbound(
        &self,
        object_id: ObjectID,
        upperbound: u64,
    ) -> Result<Option<Object>, MovyError> {
        let cf = self.cf_objects()?;
        let target_id: ObjectIDKey = object_id.into();
        let id_prefix = target_id.key_encode()?;
        let start_key = ObjectIDVersionedKey {
            id: target_id,
            version: 0,
        };
        let start_bytes = start_key.key_encode()?;

        let iter = self
            .db
            .iterator_cf(cf, IteratorMode::From(&start_bytes, Direction::Forward));
        let mut result: Option<Object> = None;
        for item in iter {
            let (key_bytes, value_bytes) = item.map_err(rocks_err)?;
            if !key_bytes.starts_with(&id_prefix) {
                break;
            }
            let key = ObjectIDVersionedKey::key_decode(&key_bytes)?;
            if key.version > upperbound {
                break;
            }
            let value = PlainObjectValue::table_decode(&value_bytes)?;
            result = Some(value.object);
        }
        Ok(result)
    }

    pub fn get_object_exact(
        &self,
        object_id: ObjectID,
        exact: u64,
    ) -> Result<Option<Object>, MovyError> {
        let cf = self.cf_objects()?;
        let key = ObjectIDVersionedKey {
            id: object_id.into(),
            version: exact,
        };
        let key_bytes = key.key_encode()?;
        match self.db.get_cf(cf, &key_bytes).map_err(rocks_err)? {
            Some(data) => {
                let value = PlainObjectValue::table_decode(&data)?;
                Ok(Some(value.object))
            }
            None => Ok(None),
        }
    }

    pub fn get_object_by_id_db(&self, object_id: ObjectID) -> Result<Option<Object>, MovyError> {
        self.get_object_upperbound(object_id, u64::MAX)
    }

    fn remove_all_versions(&self, id: ObjectID) -> Result<(), MovyError> {
        if self.ro {
            return Ok(());
        }
        let cf = self.cf_objects()?;
        let target_id: ObjectIDKey = id.into();
        let id_prefix = target_id.key_encode()?;
        let start_key = ObjectIDVersionedKey {
            id: target_id,
            version: 0,
        };
        let start_bytes = start_key.key_encode()?;

        let iter = self
            .db
            .iterator_cf(cf, IteratorMode::From(&start_bytes, Direction::Forward));
        let mut batch = WriteBatch::default();
        for item in iter {
            let (key_bytes, _) = item.map_err(rocks_err)?;
            if !key_bytes.starts_with(&id_prefix) {
                break;
            }
            batch.delete_cf(cf, &key_bytes);
        }
        self.db.write(batch).map_err(rocks_err)?;
        Ok(())
    }

    pub fn list_object_ids(&self) -> Result<Vec<ObjectID>, MovyError> {
        let cf = self.cf_objects()?;
        let iter = self.db.iterator_cf(cf, IteratorMode::Start);
        let mut out = vec![];
        for item in iter {
            let (key_bytes, _) = item.map_err(rocks_err)?;
            let key = ObjectIDVersionedKey::key_decode(&key_bytes)?;
            out.push(key.id.into());
        }
        Ok(out)
    }
}

impl<T: BackingPackageStore> BackingPackageStore for RocksCachedStore<T> {
    fn get_package_object(&self, package_id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        if let Some(hit) = self
            .get_object_by_id_db(*package_id)
            .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?
        {
            debug!("[RocksCachedStore] package hit for {}", package_id);
            return Ok(Some(PackageObject::new(hit)));
        } else {
            debug!("[RocksCachedStore] package miss for {}", package_id);
        }
        let package = self.store.get_package_object(package_id)?;
        if let Some(pkg) = &package {
            self.put_object(pkg.object())
                .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;
        }
        Ok(package)
    }
}

impl<T: ObjectStore> ObjectStore for RocksCachedStore<T> {
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        let hit = match self.get_object_by_id_db(*object_id) {
            Ok(v) => v,
            Err(e) => {
                warn!("Fail to get_object due to {}", e);
                None
            }
        };
        if let Some(hit) = hit {
            debug!("[RocksCachedStore] get_object hit for {}", object_id);
            return Some(hit);
        } else {
            debug!("[RocksCachedStore] get_object miss for {}", object_id);
        }

        let object = self.store.get_object(object_id);
        if let Some(object) = &object
            && let Err(e) = self.put_object(object)
        {
            warn!("Fail to cache object due to {}", e);
        }

        object
    }

    fn get_object_by_key(&self, object_id: &ObjectID, version: VersionNumber) -> Option<Object> {
        let hit = match self.get_object_exact(*object_id, version.into()) {
            Ok(v) => v,
            Err(e) => {
                warn!("Fail to get_object_by_key due to {}", e);
                None
            }
        };
        if let Some(hit) = hit {
            debug!("[RocksCachedStore] get_object hit for {}", object_id);
            return Some(hit);
        } else {
            debug!("[RocksCachedStore] get_object miss for {}", object_id);
        }

        let object = self.store.get_object_by_key(object_id, version);
        if let Some(object) = &object
            && let Err(e) = self.put_object(object)
        {
            warn!("Fail to cache object due to {}", e);
        }

        object
    }
}

impl<T: ParentSync> ParentSync for RocksCachedStore<T> {
    fn get_latest_parent_entry_ref_deprecated(&self, object_id: ObjectID) -> Option<ObjectRef> {
        self.store.get_latest_parent_entry_ref_deprecated(object_id)
    }
}

impl<T: ChildObjectResolver> ChildObjectResolver for RocksCachedStore<T> {
    fn get_object_received_at_version(
        &self,
        owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        epoch_id: EpochId,
    ) -> SuiResult<Option<Object>> {
        let hit = self
            .get_object_exact(*receiving_object_id, receive_object_at_version.into())
            .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;

        if let Some(hit) = hit {
            debug!(
                "[RocksCachedStore] get_object_received_at_version hit for {}:{}",
                receiving_object_id, receive_object_at_version
            );
            Ok(Some(hit))
        } else {
            debug!(
                "[RocksCachedStore] get_object_received_at_version miss for {}:{}",
                receiving_object_id, receive_object_at_version
            );
            let hit = self.store.get_object_received_at_version(
                owner,
                receiving_object_id,
                receive_object_at_version,
                epoch_id,
            )?;
            if let Some(hit) = hit {
                self.put_object(&hit)
                    .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;
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
        child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object>> {
        let hit = self
            .get_object_upperbound(*child, child_version_upper_bound.into())
            .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;

        if let Some(hit) = hit {
            if hit.version() == child_version_upper_bound {
                debug!(
                    "[RocksCachedStore] read_child_object perfect hit for {}:{} -> {}, digest {}",
                    child,
                    child_version_upper_bound,
                    hit.version(),
                    hit.digest()
                );
                return Ok(Some(hit));
            } else {
                debug!(
                    "[RocksCachedStore] read_child_object hit {} but not ideal for {}:{}, digest {}",
                    hit.version(),
                    child,
                    child_version_upper_bound,
                    hit.digest()
                );
            }
        }
        debug!(
            "[RocksCachedStore] read_child_object miss for {}:{}",
            child, child_version_upper_bound
        );
        let hit = self
            .store
            .read_child_object(parent, child, child_version_upper_bound)?;
        if let Some(hit) = hit {
            self.put_object(&hit)
                .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;
            Ok(Some(hit))
        } else {
            Ok(None)
        }
    }
}

impl<T> ObjectSuiStoreCommit for RocksCachedStore<T> {
    fn commit_single_object(&self, object: Object) -> Result<(), MovyError> {
        self.put_object(&object)
    }

    fn commit_store(
        &self,
        mut store: sui_types::inner_temporary_store::InnerTemporaryStore,
        effects: &sui_types::effects::TransactionEffects,
    ) -> Result<(), MovyError> {
        if self.ro {
            return Ok(());
        }
        let cf = self.cf_objects()?;
        let mut batch = WriteBatch::default();

        for (id, object) in store.written {
            debug!("[RocksCachedStore] Committing {}:{}", id, object.version());
            let key = ObjectIDVersionedKey {
                id: object.id().into(),
                version: object.version().into(),
            };
            let key_bytes = key.key_encode()?;
            let value_bytes = PlainObjectValue { object }.table_encode()?;
            batch.put_cf(cf, &key_bytes, &value_bytes);
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
                "[RocksCachedStore] Removing deleted/transferred consensus objects {}:{}",
                id, version
            );
            let target_id: ObjectIDKey = id.into();
            let id_prefix = target_id.key_encode()?;
            let start = ObjectIDVersionedKey {
                id: target_id,
                version: 0,
            }
            .key_encode()?;
            let iter = self
                .db
                .iterator_cf(cf, IteratorMode::From(&start, Direction::Forward));
            for item in iter {
                let (key_bytes, _) = item.map_err(rocks_err)?;
                if !key_bytes.starts_with(&id_prefix) {
                    break;
                }
                batch.delete_cf(cf, &key_bytes);
            }
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
                "[RocksCachedStore] Removing accessed consensus objects {}:{}",
                id, smeared_version
            );
            let target_id: ObjectIDKey = id.into();
            let id_prefix = target_id.key_encode()?;
            let start = ObjectIDVersionedKey {
                id: target_id,
                version: 0,
            }
            .key_encode()?;
            let iter = self
                .db
                .iterator_cf(cf, IteratorMode::From(&start, Direction::Forward));
            for item in iter {
                let (key_bytes, _) = item.map_err(rocks_err)?;
                if !key_bytes.starts_with(&id_prefix) {
                    break;
                }
                batch.delete_cf(cf, &key_bytes);
            }
        }

        self.db.write(batch).map_err(rocks_err)?;
        Ok(())
    }
}

use std::u64;

use color_eyre::eyre::eyre;
use log::{debug, warn};
use mdbx_derive::{
    HasMDBXEnvironment, KeyObjectEncode, MDBXDatabase, MDBXTable, ZstdBcsObject, ZstdJSONObject,
    mdbx::{
        BufferConfiguration, Environment, EnvironmentFlags, Geometry, Mode, PageSize, RW, SyncMode,
        TransactionAny, TransactionKind, WriteFlags,
    },
};
use movy_types::error::MovyError;
use serde::{Deserialize, Serialize};
use sui_types::{
    base_types::{ObjectID, ObjectRef, SequenceNumber, VersionNumber},
    committee::EpochId,
    effects::TransactionEffectsAPI,
    error::{SuiError, SuiErrorKind, SuiResult},
    object::Object,
    storage::{BackingPackageStore, ChildObjectResolver, ObjectStore, PackageObject, ParentSync},
};
use tokio_stream::StreamExt;

use crate::{
    database::cache::{CachedSnapshot, ObjectSuiStoreCommit},
    schema::{ObjectIDKey, ObjectIDVersionedKey},
};

pub struct ObjectTable;

#[derive(Debug, Serialize, Deserialize, ZstdJSONObject)]
pub struct DatabaseMetadata {
    pub checkpoint: u64, // inclusive, same from fuzz cli
}

#[derive(Debug, Serialize, Deserialize, ZstdBcsObject)]
pub struct PlainObjectValue {
    pub object: Object,
}

mdbx_derive::mdbx_table!(
    ObjectTable,
    ObjectIDVersionedKey,
    PlainObjectValue,
    MovyError
);

mdbx_derive::mdbx_database!(
    ObjectCacheDatabase,
    MovyError,
    DatabaseMetadata,
    ObjectTable
);

#[derive(Debug, Clone)]
pub struct MDBXCachedStore<T> {
    pub env: ObjectCacheDatabase,
    pub ro: bool,
    pub store: T,
}

impl<T> MDBXCachedStore<T> {
    pub async fn new(
        db: &str,
        store: T,
        fork_checkpoint: u64,
        ro: bool,
    ) -> Result<Self, MovyError> {
        let mut defaults = Environment::builder();
        defaults
            .set_flags(EnvironmentFlags {
                mode: if ro {
                    Mode::ReadOnly
                } else {
                    Mode::ReadWrite {
                        sync_mode: SyncMode::default(),
                    }
                },
                ..Default::default()
            })
            .set_geometry(Geometry {
                size: Some(0usize..1024 * 1024 * 1024 * 128), // max 128G
                growth_step: Some(64 * 1024 * 1024),          // 64 MB
                shrink_threshold: None,
                page_size: Some(PageSize::Set(16384)),
            })
            .set_max_dbs(256)
            .set_max_readers(256);
        let env = if ro {
            ObjectCacheDatabase::open_tables_with_defaults(db, defaults).await?
        } else {
            ObjectCacheDatabase::open_create_tables_with_defaults(db, defaults).await?
        };

        if let Some(meta) = env.metadata().await? {
            if meta.checkpoint != fork_checkpoint {
                return Err(eyre!(
                    "cache is intended for {:?} but you want to fork {}",
                    &meta,
                    fork_checkpoint
                )
                .into());
            }
        } else {
            env.write_metadata(&DatabaseMetadata {
                checkpoint: fork_checkpoint,
            })
            .await?;
        }

        Ok(Self { env, ro, store })
    }

    pub async fn dump_snapshot(&self) -> Result<CachedSnapshot, MovyError> {
        let tx = self.env.begin_ro_txn().await?;
        let cur = self.env.dbis.object_table_cursor(&tx).await?;
        let mut st = cur.into_iter_buffered::<ObjectIDVersionedKey, PlainObjectValue>(
            BufferConfiguration::default(),
        );
        let mut snap = CachedSnapshot::default();
        while let Some(it) = st.next().await {
            let (key, value) = it?;
            snap.objects
                .entry(key.id.into())
                .or_default()
                .entry(key.version)
                .or_insert(Some(value.object));
        }
        Ok(snap)
    }

    pub async fn restore_snapshot(&self, snap: CachedSnapshot) -> Result<(), MovyError> {
        if self.ro {
            return Err(eyre!("db in ro, can not restore").into());
        }

        let tx = self.env.begin_rw_txn().await?;
        for (_obj, obj_map) in snap.objects {
            for (_version, object) in obj_map {
                if let Some(object) = object {
                    self.may_cache_object_only(&tx, object).await?;
                } else {
                    // TODO: !
                }
            }
        }
        Ok(())
    }

    pub async fn may_cache_object_only(
        &self,
        tx: &TransactionAny<RW>,
        object: Object,
    ) -> Result<(), MovyError> {
        if !self.ro {
            debug!("[MDBXCachedStore] cache object {}", object.id());
            ObjectTable::put_item_tx(
                tx,
                Some(self.env.dbis.object_table),
                &ObjectIDVersionedKey {
                    id: object.id().into(),
                    version: object.version().into(),
                },
                &PlainObjectValue { object },
                WriteFlags::default(),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn cache_object_and_version(
        &self,
        tx: &TransactionAny<RW>,
        object: Object,
    ) -> Result<(), MovyError> {
        if !self.ro {
            debug!(
                "[MDBXCachedStore] cache object with version mapping {}",
                object.id()
            );
            self.may_cache_object_only(tx, object).await?;
        }
        Ok(())
    }
    pub async fn get_object_upperbound<K: TransactionKind>(
        &self,
        tx: &TransactionAny<K>,
        object_id: ObjectID,
        upperbound: u64,
    ) -> Result<Option<Object>, MovyError> {
        let start_key = ObjectIDVersionedKey {
            id: object_id.into(),
            version: 0,
        };
        let cur = tx.cursor_with_dbi(self.env.dbis.object_table).await?;

        let start_bs = start_key.key_encode()?;
        let mut st = cur
            .into_iter_from_buffered::<ObjectIDVersionedKey, PlainObjectValue>(
                &start_bs,
                BufferConfiguration::default(),
            )
            .await?;

        let mut object: Option<PlainObjectValue> = None;
        while let Some(it) = st.next().await {
            let (key, value) = it?;
            let key_id: ObjectID = key.id.into();

            if key_id == object_id {
                if key.version > upperbound {
                    break;
                }
                object = Some(value);
            }
        }

        Ok(object.map(|t| t.object))
    }

    pub async fn get_object_exact<K: TransactionKind>(
        &self,
        tx: &TransactionAny<K>,
        object_id: ObjectID,
        exact: u64,
    ) -> Result<Option<Object>, MovyError> {
        let object = self
            .env
            .dbis
            .read_object_table_tx(
                tx,
                &ObjectIDVersionedKey {
                    id: object_id.into(),
                    version: exact,
                },
            )
            .await?;

        Ok(object.map(|t| t.object))
    }

    pub async fn get_object_by_id<K: TransactionKind>(
        &self,
        tx: &TransactionAny<K>,
        object_id: ObjectID,
    ) -> Result<Option<Object>, MovyError> {
        self.get_object_upperbound(tx, object_id, u64::MAX).await
    }

    fn get_object_by_id_sync(&self, object_id: ObjectID) -> Result<Option<Object>, MovyError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let tx = self.env.begin_ro_txn().await?;
                self.get_object_by_id(&tx, object_id).await
            })
        })
    }

    fn get_object_by_id_exact_version_sync(
        &self,
        object_id: ObjectID,
        version: u64,
    ) -> Result<Option<Object>, MovyError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let tx = self.env.begin_ro_txn().await?;
                self.get_object_exact(&tx, object_id, version).await
            })
        })
    }

    fn cache_object_sync(&self, object: Object) -> Result<(), MovyError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let tx = self.env.begin_rw_txn().await?;
                self.cache_object_and_version(&tx, object).await?;
                tx.commit().await?;
                Ok(())
            })
        })
    }

    fn remove_object_sync(&self, id: ObjectID) -> Result<(), MovyError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let target_key: ObjectIDKey = id.into();
                let prefix = ObjectIDVersionedKey {
                    id: target_key,
                    version: 0,
                }
                .key_encode()?;
                let tx = self.env.begin_rw_txn().await?;

                let mut cur = tx.cursor_with_dbi(self.env.dbis.object_table).await?;
                let mut st = cur
                    .iter_from::<ObjectIDVersionedKey, Vec<u8>>(&prefix)
                    .await?;

                let mut to_delete = vec![];
                // TODO: Fix mutable borrow in mdbx-remote...
                while let Some(item) = st.next().await {
                    let (key, _) = item?;
                    if key.id == target_key {
                        to_delete.push(key);
                    } else {
                        break;
                    }
                }
                for key in to_delete {
                    self.env.dbis.del_object_table_tx(&tx, &key, None).await?;
                }

                tx.commit().await?;
                Ok(())
            })
        })
    }

    fn cache_resolver_sync(&self, object: Object, _upper: u64) -> Result<(), MovyError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let tx = self.env.begin_rw_txn().await?;
                self.may_cache_object_only(&tx, object).await?;
                tx.commit().await?;
                Ok(())
            })
        })
    }

    fn get_object_version_upperbound_sync(
        &self,
        object_id: ObjectID,
        upper: u64,
    ) -> Result<Option<Object>, MovyError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let tx = self.env.begin_ro_txn().await?;
                self.get_object_upperbound(&tx, object_id, upper).await
            })
        })
    }

    fn cache_object_only_sync(&self, object: Object) -> Result<(), MovyError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let tx = self.env.begin_rw_txn().await?;
                self.may_cache_object_only(&tx, object).await?;
                tx.commit().await?;
                Ok(())
            })
        })
    }
}

impl<T: BackingPackageStore> BackingPackageStore for MDBXCachedStore<T> {
    fn get_package_object(&self, package_id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        if let Some(hit) = self
            .get_object_by_id_sync(*package_id)
            .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?
        {
            debug!("[MDBXCachedStore] package hit for {}", package_id);
            return Ok(Some(PackageObject::new(hit)));
        } else {
            debug!("[MDBXCachedStore] package miss for {}", package_id);
        }
        let package = self.store.get_package_object(package_id)?;
        if let Some(pkg) = &package {
            self.cache_object_sync(pkg.object().clone())
                .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;
        }
        Ok(package)
    }
}

impl<T: ObjectStore> ObjectStore for MDBXCachedStore<T> {
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        let hit = match self.get_object_by_id_sync(*object_id) {
            Ok(v) => v,
            Err(e) => {
                warn!("Fail to get_object due to {}", e);
                None
            }
        };
        if let Some(hit) = hit {
            debug!("[MDBXCachedStore] get_object hit for {}", object_id);
            return Some(hit);
        } else {
            debug!("[MDBXCachedStore] get_object miss for {}", object_id);
        }

        let object = self.store.get_object(object_id);
        if let Some(object) = &object
            && let Err(e) = self.cache_object_sync(object.clone())
        {
            warn!("Fail to cache object due to {}", e);
        }

        object
    }

    fn get_object_by_key(&self, object_id: &ObjectID, version: VersionNumber) -> Option<Object> {
        let hit = match self.get_object_by_id_exact_version_sync(*object_id, version.into()) {
            Ok(v) => v,
            Err(e) => {
                warn!("Fail to get_object_by_key due to {}", e);
                None
            }
        };
        if let Some(hit) = hit {
            debug!("[MDBXCachedStore] get_object hit for {}", object_id);
            return Some(hit);
        } else {
            debug!("[MDBXCachedStore] get_object miss for {}", object_id);
        }

        let object = self.store.get_object_by_key(object_id, version);
        if let Some(object) = &object
            && let Err(e) = self.cache_object_only_sync(object.clone())
        {
            warn!("Fail to cache object due to {}", e);
        }

        object
    }
}

impl<T: ParentSync> ParentSync for MDBXCachedStore<T> {
    fn get_latest_parent_entry_ref_deprecated(&self, object_id: ObjectID) -> Option<ObjectRef> {
        self.store.get_latest_parent_entry_ref_deprecated(object_id)
    }
}

impl<T: ChildObjectResolver> ChildObjectResolver for MDBXCachedStore<T> {
    fn get_object_received_at_version(
        &self,
        owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        epoch_id: EpochId,
    ) -> SuiResult<Option<Object>> {
        let hit = self
            .get_object_by_id_exact_version_sync(
                *receiving_object_id,
                receive_object_at_version.into(),
            )
            .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;

        if let Some(hit) = hit {
            debug!(
                "[MDBXCachedStore] get_object_received_at_version hit for {}:{}",
                receiving_object_id, receive_object_at_version
            );
            Ok(Some(hit))
        } else {
            debug!(
                "[MDBXCachedStore] get_object_received_at_version miss for {}:{}",
                receiving_object_id, receive_object_at_version
            );
            let hit = self.store.get_object_received_at_version(
                owner,
                receiving_object_id,
                receive_object_at_version,
                epoch_id,
            )?;
            if let Some(hit) = hit {
                self.cache_object_only_sync(hit.clone())
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
            .get_object_version_upperbound_sync(*child, child_version_upper_bound.into())
            .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;

        if let Some(hit) = hit {
            if hit.version() == child_version_upper_bound {
                debug!(
                    "[MDBXCachedStore] read_child_object perfect hit for {}:{} -> {}, digest {}",
                    child,
                    child_version_upper_bound,
                    hit.version(),
                    hit.digest()
                );
                return Ok(Some(hit));
            } else {
                debug!(
                    "[MDBXCachedStore] read_child_object hit {} but not ideal for {}:{}, digest {}",
                    hit.version(),
                    child,
                    child_version_upper_bound,
                    hit.digest()
                );
            }
        }
        debug!(
            "[MDBXCachedStore] read_child_object miss for {}:{}",
            child, child_version_upper_bound
        );
        let hit = self
            .store
            .read_child_object(parent, child, child_version_upper_bound)?;
        if let Some(hit) = hit {
            self.cache_resolver_sync(hit.clone(), child_version_upper_bound.into())
                .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;
            Ok(Some(hit))
        } else {
            Ok(None)
        }
    }
}

impl<T> ObjectSuiStoreCommit for MDBXCachedStore<T> {
    fn commit_single_object(&self, object: Object) -> Result<(), MovyError> {
        self.cache_object_sync(object)
    }

    fn commit_store(
        &self,
        mut store: sui_types::inner_temporary_store::InnerTemporaryStore,
        effects: &sui_types::effects::TransactionEffects,
    ) -> Result<(), MovyError> {
        for (id, object) in store.written {
            debug!("[MDBXCachedStore] Committing {}:{}", id, object.version());
            self.cache_object_sync(object)?;
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
                "[MDBXCachedStore] Removing deleted/transferred consensus objects {}:{}",
                id, version
            );
            self.remove_object_sync(id)?;
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
                "[MDBXCachedStore] Removing accessed consensus objects {}:{}",
                id, smeared_version
            );
            self.remove_object_sync(id)?;
        }

        // Optionally prune history objects?
        Ok(())
    }
}

use movy_types::error::MovyError;
use sui_types::{
    base_types::{ObjectID, ObjectRef, SequenceNumber, VersionNumber},
    committee::EpochId,
    effects::TransactionEffects,
    error::SuiResult,
    inner_temporary_store::InnerTemporaryStore,
    object::Object,
    storage::{
        BackingPackageStore, ChildObjectResolver, ObjectKey, ObjectStore, PackageObject, ParentSync,
    },
};

use crate::database::cache::ObjectSuiStoreCommit;

// Small utils to workaround traits bound
#[derive(Clone, Debug)]
pub enum TrivialBackStore<T1, T2> {
    T1(T1),
    T2(T2),
}

impl<T1, T2> BackingPackageStore for TrivialBackStore<T1, T2>
where
    T1: BackingPackageStore,
    T2: BackingPackageStore,
{
    fn get_package_object(&self, package_id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        match self {
            Self::T1(t1) => t1.get_package_object(package_id),
            Self::T2(t2) => t2.get_package_object(package_id),
        }
    }
}

impl<T1, T2> ChildObjectResolver for TrivialBackStore<T1, T2>
where
    T1: ChildObjectResolver,
    T2: ChildObjectResolver,
{
    fn get_object_received_at_version(
        &self,
        owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        epoch_id: EpochId,
    ) -> SuiResult<Option<Object>> {
        match self {
            Self::T1(t1) => t1.get_object_received_at_version(
                owner,
                receiving_object_id,
                receive_object_at_version,
                epoch_id,
            ),
            Self::T2(t2) => t2.get_object_received_at_version(
                owner,
                receiving_object_id,
                receive_object_at_version,
                epoch_id,
            ),
        }
    }
    fn read_child_object(
        &self,
        parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object>> {
        match self {
            Self::T1(t1) => t1.read_child_object(parent, child, child_version_upper_bound),
            Self::T2(t2) => t2.read_child_object(parent, child, child_version_upper_bound),
        }
    }
}

impl<T1, T2> ParentSync for TrivialBackStore<T1, T2>
where
    T1: ParentSync,
    T2: ParentSync,
{
    fn get_latest_parent_entry_ref_deprecated(&self, object_id: ObjectID) -> Option<ObjectRef> {
        match self {
            Self::T1(t) => t.get_latest_parent_entry_ref_deprecated(object_id),
            Self::T2(t) => t.get_latest_parent_entry_ref_deprecated(object_id),
        }
    }
}

impl<T1, T2> ObjectStore for TrivialBackStore<T1, T2>
where
    T1: ObjectStore,
    T2: ObjectStore,
{
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        match self {
            Self::T1(t1) => t1.get_object(object_id),
            Self::T2(t2) => t2.get_object(object_id),
        }
    }

    fn get_object_by_key(&self, object_id: &ObjectID, version: VersionNumber) -> Option<Object> {
        match self {
            Self::T1(t1) => t1.get_object_by_key(object_id, version),
            Self::T2(t2) => t2.get_object_by_key(object_id, version),
        }
    }

    fn multi_get_objects(&self, object_ids: &[ObjectID]) -> Vec<Option<Object>> {
        match self {
            Self::T1(t1) => t1.multi_get_objects(object_ids),
            Self::T2(t2) => t2.multi_get_objects(object_ids),
        }
    }

    fn multi_get_objects_by_key(&self, object_keys: &[ObjectKey]) -> Vec<Option<Object>> {
        match self {
            Self::T1(t1) => t1.multi_get_objects_by_key(object_keys),
            Self::T2(t2) => t2.multi_get_objects_by_key(object_keys),
        }
    }
}

impl<T1, T2> ObjectSuiStoreCommit for TrivialBackStore<T1, T2>
where
    T1: ObjectSuiStoreCommit,
    T2: ObjectSuiStoreCommit,
{
    fn commit_single_object(&self, object: Object) -> Result<(), MovyError> {
        match self {
            Self::T1(t1) => t1.commit_single_object(object),
            Self::T2(t2) => t2.commit_single_object(object),
        }
    }

    fn commit_store(
        &self,
        store: InnerTemporaryStore,
        effects: &TransactionEffects,
    ) -> Result<(), MovyError> {
        match self {
            Self::T1(t1) => t1.commit_store(store, effects),
            Self::T2(t2) => t2.commit_store(store, effects),
        }
    }
}

use sui_types::{
    base_types::{ObjectID, ObjectRef, SequenceNumber, VersionNumber},
    committee::EpochId,
    error::SuiResult,
    object::Object,
    storage::{BackingPackageStore, ChildObjectResolver, ObjectStore, PackageObject, ParentSync},
};

#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyStore;

impl ObjectStore for EmptyStore {
    fn get_object(&self, _object_id: &ObjectID) -> Option<Object> {
        None
    }

    fn get_object_by_key(&self, _object_id: &ObjectID, _version: VersionNumber) -> Option<Object> {
        None
    }
}

impl ParentSync for EmptyStore {
    fn get_latest_parent_entry_ref_deprecated(&self, _object_id: ObjectID) -> Option<ObjectRef> {
        None
    }
}

impl BackingPackageStore for EmptyStore {
    fn get_package_object(&self, _package_id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        Ok(None)
    }
}

impl ChildObjectResolver for EmptyStore {
    fn get_object_received_at_version(
        &self,
        _owner: &ObjectID,
        _receiving_object_id: &ObjectID,
        _receive_object_at_version: SequenceNumber,
        _epoch_id: EpochId,
    ) -> SuiResult<Option<Object>> {
        Ok(None)
    }
    fn read_child_object(
        &self,
        _parent: &ObjectID,
        _child: &ObjectID,
        _child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object>> {
        Ok(None)
    }
}

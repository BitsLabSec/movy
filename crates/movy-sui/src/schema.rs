use std::string::FromUtf8Error;

use mdbx_derive::{
    KeyAsTableObject, KeyObject, KeyObjectDecode, KeyObjectEncode, ZstdBcsObject, ZstdJSONObject,
    mdbx_database, mdbx_dupsort_table, mdbx_table,
};
use serde::{Deserialize, Serialize};
use sui_types::{
    base_types::{ObjectID, SequenceNumber},
    object::Object,
};

#[derive(Debug, Clone, PartialEq, Eq, Copy, KeyObject, KeyAsTableObject)]
pub struct ObjectIDKey {
    pub id: [u8; 32],
}

impl From<ObjectID> for ObjectIDKey {
    fn from(value: ObjectID) -> Self {
        Self {
            id: value.into_bytes(),
        }
    }
}

impl From<ObjectIDKey> for ObjectID {
    fn from(value: ObjectIDKey) -> Self {
        ObjectID::from_address(value.id.into())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Copy, KeyObject, KeyAsTableObject,
)]
pub struct ObjectVersionKey {
    pub version: u64,
}

impl From<u64> for ObjectVersionKey {
    fn from(version: u64) -> Self {
        Self { version }
    }
}

impl From<SequenceNumber> for ObjectVersionKey {
    fn from(value: SequenceNumber) -> Self {
        Self {
            version: value.value(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Copy, KeyObject)]
pub struct ObjectIDVersionedKey {
    pub id: ObjectIDKey,
    pub version: u64,
}

// Tables

pub struct ObjectTable;

#[derive(Debug, Clone, Serialize, Deserialize, ZstdBcsObject)]
pub enum ObjectTableValue {
    Object(Object),
    Wrapped,
    Deleted,
}

impl From<Object> for ObjectTableValue {
    fn from(object: Object) -> Self {
        Self::Object(object)
    }
}

impl ObjectTableValue {
    pub fn into_object(self) -> Option<Object> {
        match self {
            Self::Object(obj) => Some(obj),
            _ => None,
        }
    }
}

mdbx_table!(ObjectTable, ObjectIDKey, ObjectTableValue);

pub struct ObjectTypeTable;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectTypeKey {
    pub key: Vec<u8>,
}

impl ObjectTypeKey {
    pub fn into_string(self) -> Result<String, FromUtf8Error> {
        String::from_utf8(self.key)
    }
}

impl From<&[u8]> for ObjectTypeKey {
    fn from(key: &[u8]) -> Self {
        Self { key: key.to_vec() }
    }
}

impl KeyObjectEncode for ObjectTypeKey {
    fn key_encode(&self) -> Result<Vec<u8>, mdbx_derive::Error> {
        Ok(self.key.to_vec())
    }
}

impl KeyObjectDecode for ObjectTypeKey {
    const KEYSIZE: usize = 0;
    fn key_decode(val: &[u8]) -> Result<Self, mdbx_derive::Error> {
        Ok(Self { key: val.to_vec() })
    }
}

impl mdbx_derive::mdbx::TableObject for ObjectTypeKey {
    fn decode(data_val: &[u8]) -> Result<Self, mdbx_derive::mdbx::Error> {
        Self::key_decode(data_val).map_err(|_e| mdbx_derive::mdbx::Error::Corrupted)
    }
}

mdbx_dupsort_table!(ObjectTypeTable, ObjectTypeKey, ObjectIDKey);

// Onwer could change but TransactionEffectsV1 can not track that efficiently unfortunately (no input owners)
// Maybe build owner tables afterwards (?)

// pub struct ObjectOwnerTable;
// mdbx_dupsort_table!(ObjectOwnerTable, ObjectIDKey, ObjectIDKey);

#[derive(Debug, Clone, Serialize, Deserialize, ZstdJSONObject, Default)]
pub struct ObjectTypeDatabaseMetadata {
    pub checkpoint: u64, // exclusive!
}

mdbx_database!(
    ObjectTypeIndexedDatabase,
    mdbx_derive::Error,
    ObjectTypeDatabaseMetadata,
    ObjectTable,
    ObjectTypeTable
);

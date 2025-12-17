use alloy_primitives::B256;
use color_eyre::eyre::eyre;
use serde::{Deserialize, Serialize};
use sui_types::{
    TypeTag,
    base_types::ObjectRef,
    digests::{Digest, ObjectDigest, TransactionDigest},
    object::{Object, Owner},
};

use crate::{
    error::MovyError,
    input::{MoveAddress, MoveTypeTag},
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MoveOwner {
    AddressOwner(MoveAddress),
    ObjectOwner(MoveAddress), // Aptos only has this!
    Immutable,
    Shared {
        initial_shared_version: u64,
    },
    ConsensusAddressOwner {
        start_version: u64,
        owner: MoveAddress,
    },
}

impl From<MoveOwner> for Owner {
    fn from(value: MoveOwner) -> Self {
        match value {
            MoveOwner::Immutable => Self::Immutable,
            MoveOwner::Shared {
                initial_shared_version,
            } => Self::Shared {
                initial_shared_version: initial_shared_version.into(),
            },
            MoveOwner::ObjectOwner(v) => Self::ObjectOwner(v.into()),
            MoveOwner::AddressOwner(v) => Self::AddressOwner(v.into()),
            MoveOwner::ConsensusAddressOwner {
                start_version,
                owner,
            } => Self::ConsensusAddressOwner {
                start_version: start_version.into(),
                owner: owner.into(),
            },
        }
    }
}

impl From<Owner> for MoveOwner {
    fn from(value: Owner) -> Self {
        match value {
            Owner::Immutable => Self::Immutable,
            Owner::Shared {
                initial_shared_version,
            } => Self::Shared {
                initial_shared_version: initial_shared_version.into(),
            },
            Owner::ObjectOwner(v) => Self::ObjectOwner(v.into()),
            Owner::AddressOwner(v) => Self::AddressOwner(v.into()),
            Owner::ConsensusAddressOwner {
                start_version,
                owner,
            } => Self::ConsensusAddressOwner {
                start_version: start_version.into(),
                owner: owner.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Copy)]
pub struct MoveDigest(B256);

impl From<TransactionDigest> for MoveDigest {
    fn from(value: TransactionDigest) -> Self {
        Self(B256::new(value.into_inner()))
    }
}

impl From<MoveDigest> for TransactionDigest {
    fn from(value: MoveDigest) -> Self {
        Self::new(value.0.0)
    }
}

impl From<ObjectDigest> for MoveDigest {
    fn from(value: ObjectDigest) -> Self {
        Self(B256::new(value.into_inner()))
    }
}

impl From<MoveDigest> for ObjectDigest {
    fn from(value: MoveDigest) -> Self {
        Self::new(value.0.0)
    }
}

impl From<Digest> for MoveDigest {
    fn from(value: Digest) -> Self {
        Self(B256::new(value.into_inner()))
    }
}

impl From<MoveDigest> for Digest {
    fn from(value: MoveDigest) -> Self {
        Self::new(value.0.0)
    }
}

// Aptos: ObjectID + Type = Resource
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MoveObjectInfo {
    pub id: MoveAddress,
    pub ty: MoveTypeTag,
    pub owner: MoveOwner,
    pub version: u64,
    pub digest: MoveDigest,
}

impl MoveObjectInfo {
    pub fn sui_reference(&self) -> ObjectRef {
        (self.id.into(), self.version.into(), self.digest.into())
    }
}

impl TryFrom<&Object> for MoveObjectInfo {
    type Error = MovyError;

    fn try_from(value: &Object) -> Result<Self, Self::Error> {
        match value.data.try_as_move() {
            Some(v) => Ok(Self {
                id: value.id().into(),
                ty: TypeTag::from(v.type_().clone()).into(),
                owner: value.owner().clone().into(),
                version: v.version().into(),
                digest: value.digest().into(),
            }),
            None => Err(eyre!("not a move object").into()),
        }
    }
}

use anyhow::anyhow;
use color_eyre::eyre::eyre;
use mdbx_derive::mdbx::BufferConfiguration;
use movy_sui::{
    database::{
        cache::{CachedStore, ObjectSuiStoreCommit},
        file::MDBXCachedStore,
    },
    schema::ObjectIDVersionedKey,
};
use movy_types::{
    abi::MovePackageAbi,
    error::MovyError,
    input::{MoveAddress, MoveTypeTag},
    object::{MoveObjectInfo, MoveOwner},
};
use sui_types::{
    base_types::ObjectID,
    digests::TransactionDigest,
    object::{MoveObject, OBJECT_START_VERSION, Object},
    storage::ObjectStore,
};
use tokio_stream::StreamExt;

/// Abstract Sui/Aptos DB operations

pub trait ObjectStoreDumpRestore: Sized {}

#[auto_impl::auto_impl(&, Arc, Box)]
pub trait ObjectStoreCachedStore {
    fn load_object(&self, address: MoveAddress) -> impl Future<Output = Result<(), MovyError>>;
    fn list_objects(&self) -> impl Future<Output = Result<Vec<MoveAddress>, MovyError>>;
    fn dump(&self) -> impl Future<Output = Result<Vec<u8>, MovyError>>;
    fn restore(&self, bs: Vec<u8>) -> impl Future<Output = Result<(), MovyError>>;
}

impl<T: ObjectStore> ObjectStoreCachedStore for CachedStore<T> {
    async fn load_object(&self, address: MoveAddress) -> Result<(), MovyError> {
        self.get_object(&address.into())
            .ok_or_else(|| eyre!("can not load {}", address))?;
        Ok(())
    }
    async fn list_objects(&self) -> Result<Vec<MoveAddress>, MovyError> {
        Ok(self
            .inner
            .borrow()
            .objects
            .keys()
            .map(|v| MoveAddress::from(*v))
            .collect())
    }
    async fn dump(&self) -> Result<Vec<u8>, MovyError> {
        Ok(bcs::to_bytes(&*self.inner.borrow())?)
    }

    async fn restore(&self, bs: Vec<u8>) -> Result<(), MovyError> {
        self.restore_snapshot(bcs::from_bytes(&bs)?);
        Ok(())
    }
}

impl<T: ObjectStore> ObjectStoreCachedStore for MDBXCachedStore<T> {
    async fn load_object(&self, address: MoveAddress) -> Result<(), MovyError> {
        self.get_object(&address.into())
            .ok_or_else(|| eyre!("can not load {}", address))?;
        Ok(())
    }
    async fn list_objects(&self) -> Result<Vec<MoveAddress>, MovyError> {
        let tx = self.env.env.begin_ro_txn().await?;
        let cur = tx.cursor_with_dbi(self.env.dbis.object_table).await?;
        let mut st =
            cur.into_iter_buffered::<ObjectIDVersionedKey, Vec<_>>(BufferConfiguration::default());
        let mut out = vec![];
        while let Some(it) = st.next().await {
            let it = it?;
            let it: ObjectID = it.0.id.into();
            out.push(MoveAddress::from(it));
        }
        Ok(out)
    }
    async fn dump(&self) -> Result<Vec<u8>, MovyError> {
        let snap = self.dump_snapshot().await?;
        Ok(bcs::to_bytes(&snap)?)
    }

    async fn restore(&self, bs: Vec<u8>) -> Result<(), MovyError> {
        self.restore_snapshot(bcs::from_bytes(&bs)?).await?;
        Ok(())
    }
}

pub trait ObjectStoreInfo {
    fn get_version(&self, object_id: MoveAddress) -> Result<u64, MovyError>;
    fn get_move_object_info(&self, object_id: MoveAddress) -> Result<MoveObjectInfo, MovyError>;
    fn get_package_info(&self, object_id: MoveAddress)
    -> Result<Option<MovePackageAbi>, MovyError>;
}

impl<T: ObjectStore> ObjectStoreInfo for T {
    fn get_version(&self, object_id: MoveAddress) -> Result<u64, MovyError> {
        let object = self
            .get_object(&object_id.into())
            .ok_or_else(|| MovyError::Any(anyhow!("object {} not found", object_id)))?;
        Ok(object.version().into())
    }

    fn get_move_object_info(&self, object_id: MoveAddress) -> Result<MoveObjectInfo, MovyError> {
        let object = self
            .get_object(&object_id.into())
            .ok_or_else(|| MovyError::Any(anyhow!("object {} not found", object_id)))?;
        MoveObjectInfo::try_from(&object)
    }
    fn get_package_info(
        &self,
        object_id: MoveAddress,
    ) -> Result<Option<MovePackageAbi>, MovyError> {
        match self.get_object(&object_id.into()) {
            Some(object) => MovePackageAbi::from_sui_object(&object).map(Some),
            None => Ok(None),
        }
    }
}

pub trait ObjectStoreMintObject {
    fn mint_coin(
        &self,
        coin: MoveTypeTag,
        owner: MoveOwner,
        value: u64,
    ) -> Result<MoveAddress, MovyError> {
        let address = MoveAddress::random();
        self.mint_coin_id(coin, owner, address, value)?;
        Ok(address)
    }

    fn mint_coin_id(
        &self,
        coin: MoveTypeTag,
        owner: MoveOwner,
        id: MoveAddress,
        value: u64,
    ) -> Result<(), MovyError>;
}

impl<T: ObjectSuiStoreCommit> ObjectStoreMintObject for T {
    fn mint_coin_id(
        &self,
        coin: MoveTypeTag,
        owner: MoveOwner,
        id: MoveAddress,
        value: u64,
    ) -> Result<(), MovyError> {
        log::debug!("Minted coin ty {} value {} owner {}", coin, value, id);
        let move_object =
            MoveObject::new_coin(coin.try_into()?, OBJECT_START_VERSION, id.into(), value);
        let coin = Object::new_move(
            move_object,
            owner.into(),
            TransactionDigest::genesis_marker(),
        );
        self.commit_single_object(coin.clone())?;
        Ok(())
    }
}

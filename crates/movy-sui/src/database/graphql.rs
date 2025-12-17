use color_eyre::eyre::eyre;
use log::{debug, trace, warn};
use movy_types::error::MovyError;
use sui_types::{
    base_types::ObjectID,
    error::{SuiError, SuiErrorKind},
    messages_checkpoint::{CheckpointContents, CheckpointSummary},
    object::Object,
    storage::{BackingPackageStore, PackageObject, ParentSync},
};

use crate::{
    database::{DexForkedReplayStore, ForkedCheckpoint},
    rpc::graphql::{EpochData, GraphQlClient, TransactionGraphQlResponse, objects_query},
};

#[derive(Debug, Clone)]
pub struct GraphQlDatabase {
    pub graphql: GraphQlClient,
    pub fork: u64, // inclusive
}

impl GraphQlDatabase {
    pub fn new(client: reqwest::Client, url: reqwest::Url, fork: u64, concurrent: usize) -> Self {
        Self {
            graphql: GraphQlClient::new(client, url, concurrent),
            fork,
        }
    }

    pub fn new_client(client: GraphQlClient, fork: u64) -> Self {
        Self {
            graphql: client,
            fork,
        }
    }

    pub fn new_mystens(fork: u64) -> Self {
        Self {
            graphql: GraphQlClient::new_mystens(),
            fork,
        }
    }

    pub async fn wait_until_fork(&self) -> Result<(), MovyError> {
        loop {
            let (_, current_summary) = self
                .graphql
                .query_checkpoint(None)
                .await?
                .ok_or_else(|| eyre!("no latest ckpt"))?;

            if current_summary.sequence_number >= self.fork {
                return Ok(());
            }
            log::debug!(
                "GraphQl is at {} but we need to fork {}",
                current_summary.sequence_number,
                self.fork
            );
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    pub async fn transaction(
        &self,
        digest: &str,
    ) -> Result<Option<TransactionGraphQlResponse>, MovyError> {
        debug!("Query transaction {}", digest);
        let mut data = self
            .graphql
            .query_transactions(vec![digest.to_string()])
            .await?;
        debug!("Query transaction succeeds");
        Ok(data.pop())
    }

    pub async fn epoch(&self, epoch_id: u64) -> Result<Option<EpochData>, MovyError> {
        debug!("Start epoch query for {}", epoch_id);
        let mut data = self.graphql.query_epoches(vec![epoch_id]).await?;
        debug!("Epoch query succeeds");
        Ok(data.pop())
    }

    pub async fn objects(
        &self,
        keys: Vec<objects_query::ObjectKey>,
    ) -> Result<Vec<Object>, MovyError> {
        debug!("Object query: {:?}", keys);
        let data = self.graphql.query_objects(keys).await?;
        for t in &data {
            trace!(
                "Object result is: {}",
                format!("{}:{} -> {}", t.id(), t.version(), t.digest())
            )
        }
        debug!("Object query succeeds");
        Ok(data)
    }

    pub async fn sinlge_object(
        &self,
        k: objects_query::ObjectKey,
    ) -> Result<Option<Object>, MovyError> {
        let mut v = self.objects(vec![k.clone()]).await?;

        if v.is_empty() {
            debug!("Get {:?} but no results", &k);
            Ok(None)
        } else {
            let object = v.remove(0);
            debug!("Get {:?} -> {}", &k, object.version());
            Ok(Some(object))
        }
    }

    pub async fn get_object_at_checkpoint(
        &self,
        object: ObjectID,
        checkpoint: u64,
    ) -> Result<Option<Object>, MovyError> {
        self.sinlge_object(objects_query::ObjectKey::at_checkpoint(object, checkpoint))
            .await
    }
}

impl sui_types::storage::ChildObjectResolver for GraphQlDatabase {
    fn read_child_object(
        &self,
        parent: &sui_types::base_types::ObjectID,
        child: &sui_types::base_types::ObjectID,
        child_version_upper_bound: sui_types::base_types::SequenceNumber,
    ) -> sui_types::error::SuiResult<Option<Object>> {
        debug!(
            "[ChildObjectResolver] read_child_object({}, {}, {})",
            parent, child, child_version_upper_bound,
        );
        let object_key =
            objects_query::ObjectKey::at_root_version(*child, child_version_upper_bound.into());
        let object = tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current()
                .block_on(async move { self.sinlge_object(object_key).await })
        })
        .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;
        debug!(
            "[ChildObjectResolver]> read_child_object({}, {}, {}) -> {:?}",
            parent,
            child,
            child_version_upper_bound,
            object
                .as_ref()
                .map(|t| format!("{}:{} -> {}", t.id(), t.version(), t.digest()))
        );
        Ok(object)
    }

    fn get_object_received_at_version(
        &self,
        owner: &sui_types::base_types::ObjectID,
        receiving_object_id: &sui_types::base_types::ObjectID,
        receive_object_at_version: sui_types::base_types::SequenceNumber,
        epoch_id: sui_types::committee::EpochId,
    ) -> sui_types::error::SuiResult<Option<Object>> {
        debug!(
            "[ChildObjectResolver] get_object_received_at_version({}, {}, {}, {})",
            owner, receiving_object_id, receive_object_at_version, epoch_id
        );
        let object_key = objects_query::ObjectKey::at_version(
            *receiving_object_id,
            receive_object_at_version.into(),
        );
        let object = tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current()
                .block_on(async move { self.sinlge_object(object_key).await })
        })
        .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;
        debug!(
            "[ChildObjectResolver]> get_object_received_at_version({}:{}) {:?}",
            receiving_object_id,
            receive_object_at_version,
            object
                .as_ref()
                .map(|t| format!("{}:{} -> {}", t.id(), t.version(), t.digest()))
        );
        Ok(object)
    }
}

impl sui_types::storage::ObjectStore for GraphQlDatabase {
    fn get_object(&self, object_id: &sui_types::base_types::ObjectID) -> Option<Object> {
        debug!("[ObjectStore] get_object id={}", object_id);
        match tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current()
                .block_on(async move { self.get_object_at_checkpoint(*object_id, self.fork).await })
        }) {
            Ok(object) => {
                debug!(
                    "[ObjectStore] get_object {} -> {:?}",
                    object_id,
                    object.as_ref().map(|t| t.version())
                );
                object
            }
            Err(e) => {
                warn!("Fail to get object {} due to {}", object_id, e);
                None
            }
        }
    }

    fn get_object_by_key(
        &self,
        object_id: &sui_types::base_types::ObjectID,
        version: sui_types::base_types::VersionNumber,
    ) -> Option<Object> {
        debug!(
            "[ObjectStore] get_object_by_key id={} version={}",
            object_id, version
        );
        let object_key = objects_query::ObjectKey::at_version(*object_id, version.into());
        match tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current()
                .block_on(async move { self.sinlge_object(object_key).await })
        }) {
            Ok(object) => {
                debug!(
                    "[ObjectStore] get_object_by_key {}:{} -> {:?}",
                    object_id,
                    version,
                    object.as_ref().map(|t| t.version())
                );
                object
            }
            Err(e) => {
                warn!("Fail to get object {}:{} due to {}", object_id, version, e);
                None
            }
        }
    }
}

impl ParentSync for GraphQlDatabase {
    fn get_latest_parent_entry_ref_deprecated(
        &self,
        object_id: sui_types::base_types::ObjectID,
    ) -> Option<sui_types::base_types::ObjectRef> {
        unreachable!(
            "unexpected ParentSync::get_latest_parent_entry_ref_deprecated({})",
            object_id,
        )
    }
}

impl BackingPackageStore for GraphQlDatabase {
    fn get_package_object(
        &self,
        package_id: &ObjectID,
    ) -> sui_types::error::SuiResult<Option<sui_types::storage::PackageObject>> {
        debug!("[BackingPackageStore] get_package_object({})", package_id);
        let package = tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current().block_on(async move {
                self.get_object_at_checkpoint(*package_id, self.fork).await
            })
        })
        .map_err(|e| SuiError(Box::new(SuiErrorKind::Storage(e.to_string()))))?;
        if let Some(package) = package {
            debug!(
                "[BackingPackageStore]> get_package_object {}:{} -> {}",
                package.id(),
                package.version(),
                package.digest()
            );
            Ok(Some(PackageObject::new(package)))
        } else {
            debug!(
                "[BackingPackageStore]> get_package_object {} -> None",
                package_id
            );
            Ok(None)
        }
    }
}

impl ForkedCheckpoint for GraphQlDatabase {
    fn forked_at(&self) -> u64 {
        self.fork
    }
}

impl DexForkedReplayStore for GraphQlDatabase {
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
        Box::pin(async move {
            self.graphql
                .query_checkpoint(Some(ckpt.unwrap_or(self.fork)))
                .await
        })
    }
    fn dynamic_fields(
        &self,
        table: ObjectID,
        ty: Option<String>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Object>, MovyError>> + Send + '_>> {
        let fork = self.fork;
        Box::pin(async move {
            self.graphql
                .dynamic_fields_at_checkpoint(fork, table.to_canonical_string(true), ty)
                .await
        })
    }
    fn owned_objects(
        &self,
        owner: ObjectID,
        ty: Option<String>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Object>, MovyError>> + Send + '_>> {
        let fork = self.fork;
        Box::pin(async move {
            self.graphql
                .owned_objects_at_checkpoint(fork, owner.to_canonical_string(true), ty)
                .await
        })
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use sui_types::base_types::ObjectID;

    use crate::database::graphql::{GraphQlClient, GraphQlDatabase};

    #[tokio::test]
    async fn test_list_fields() {
        let gql = GraphQlClient::new_mystens();
        let target = ObjectID::from_str(
            "0x8b0a90c71b7993522e609c40df29bc5bf476609c026b74b2ae4572d05e4416a2",
        )
        .unwrap();
        let fields = gql
            .dynamic_fields_at_checkpoint(204031988, target.to_canonical_string(true), None)
            .await
            .unwrap();
        dbg!(fields.len());
        let fields = gql
            .dynamic_fields_at_checkpoint(154031988, target.to_canonical_string(true), None)
            .await
            .unwrap();
        dbg!(fields.len());
    }

    #[tokio::test]
    async fn test_checkpont() {
        let gql = GraphQlClient::new_mystens();
        let checkpoint = gql.query_checkpoint(None).await.unwrap();
        let (_, summary) = checkpoint.unwrap();
        let non_exist = gql
            .query_checkpoint(Some(summary.sequence_number * 2))
            .await
            .unwrap();
        assert!(non_exist.is_none());
    }
}

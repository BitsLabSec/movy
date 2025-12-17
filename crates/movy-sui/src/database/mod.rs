use std::pin::Pin;

use movy_types::error::MovyError;
use sui_types::{
    base_types::ObjectID,
    messages_checkpoint::{CheckpointContents, CheckpointSummary},
    object::Object,
    storage::BackingStore,
};

pub mod cache;
pub mod empty;
pub mod file;
pub mod graphql;

pub trait ForkedCheckpoint {
    /// Inclusive checkpoint number
    fn forked_at(&self) -> u64;
}

pub trait DexForkedReplayStore: BackingStore + ForkedCheckpoint {
    fn checkpoint(
        &self,
        ckpt: Option<u64>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<(CheckpointContents, CheckpointSummary)>, MovyError>>
                + Send
                + '_,
        >,
    >;
    fn owned_objects(
        &self,
        owner: ObjectID,
        ty: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Object>, MovyError>> + Send + '_>>;
    fn dynamic_fields(
        &self,
        table: ObjectID,
        ty: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Object>, MovyError>> + Send + '_>>;
}

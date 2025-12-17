use std::{
    io::Write,
    path::{Path, PathBuf},
};

use clap::Args;
use color_eyre::eyre::eyre;
use movy_sui::rpc::graphql::GraphQlClient;
use movy_types::error::MovyError;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub fn read_value<T: DeserializeOwned>(path: &Path) -> Result<T, MovyError> {
    let fp = std::fs::File::open(path)?;
    Ok(serde_json::from_reader(fp)?)
}

pub fn read_bcs_value<T: DeserializeOwned>(path: &Path) -> Result<T, MovyError> {
    let fp = std::fs::File::open(path)?;
    Ok(bcs::from_reader(fp)?)
}

pub fn may_save_bytes(
    output: &Option<PathBuf>,
    fname: &str,
    bytes: &[u8],
) -> Result<(), MovyError> {
    if let Some(output) = output.as_ref() {
        let fpath = output.join(fname);
        let mut fp = std::fs::File::create(&fpath)?;
        fp.write_all(bytes)?;
    }
    Ok(())
}

pub fn may_save_json_value<V: Serialize>(
    output: &Option<PathBuf>,
    fname: &str,
    val: &V,
) -> Result<(), MovyError> {
    if output.is_some() {
        may_save_bytes(output, fname, &serde_json::to_vec_pretty(&val)?)?;
    }
    Ok(())
}

#[derive(Args, Clone, Debug, Serialize, Deserialize)]
pub struct SuiOnchainArguments {
    #[arg(
        long,
        help = "Checkpoint to fork, if not specified, use the latest checkpoint"
    )]
    pub checkpoint: Option<u64>,
    #[arg(long, help = "The epoch of the checkpoint")]
    pub epoch: Option<u64>,
    #[arg(long, help = "The timestamp of the epoch")]
    pub epoch_ms: Option<u64>,
}

#[derive(Copy, Debug, Clone, Serialize, Deserialize)]
pub struct SuiOnchainPrimitives {
    pub epoch: u64,
    pub epoch_ms: u64,
    pub checkpoint: u64,
}

impl SuiOnchainArguments {
    pub async fn resolve_onchain_primitives(
        &self,
        gql: Option<&GraphQlClient>,
    ) -> Result<SuiOnchainPrimitives, MovyError> {
        if self.checkpoint.is_some() && self.epoch.is_some() && self.epoch_ms.is_some() {
            return Ok(SuiOnchainPrimitives {
                checkpoint: self.checkpoint.unwrap(),
                epoch: self.epoch.unwrap(),
                epoch_ms: self.epoch_ms.unwrap(),
            });
        }

        if self.epoch.is_some() != self.epoch_ms.is_some() {
            return Err(eyre!("epoch and epoch_ms should be either all given or all empty").into());
        }

        if self.checkpoint.is_none() && self.epoch.is_some() {
            return Err(eyre!("can not specify epoch without specifying checkpoint").into());
        }

        // Now we have to infer values anyway
        let gql = gql.ok_or_else(|| {
            eyre!("no rpc given while checkpoint is not given or epoch/epoch_ms is not given")
        })?;
        let ckpt = if let Some(ckpt) = self.checkpoint {
            gql.query_checkpoint(Some(ckpt)).await?
        } else {
            gql.query_checkpoint(None).await?
        };
        let (_, summary) = ckpt.ok_or_else(|| eyre!("{:?} not present", self.checkpoint))?;
        let epoch = gql.query_epoches(vec![summary.epoch]).await?.pop().unwrap();

        Ok(SuiOnchainPrimitives {
            checkpoint: summary.sequence_number,
            epoch: summary.epoch,
            epoch_ms: epoch.start_timestamp.timestamp().try_into().unwrap(),
        })
    }
}

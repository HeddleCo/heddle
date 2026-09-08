// SPDX-License-Identifier: Apache-2.0
//! Build and retain an exact selected-source transfer for explicit publication
//! and retries. The application chooses the scratch directory and disclosure
//! policy; preparation never reads historical States or adjacent checkouts.
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

use api::v2::client::RpcTransport;
use objects::{
    object::{State, StateId},
    store::{
        ObjectSource,
        pack::{StreamingPackBuilder, build_source_pack},
    },
};

use super::{Error, typed_digest};
use crate::{Thread, contract::*, transport};

pub struct SourceBudget {
    pub max_objects: usize,
    pub max_decoded_bytes: u64,
}

/// The caller retains the same options for retry. The source endpoint is the
/// local Iroh identity; credential signing may use a distinct owner device key.
pub struct PublicationOptions {
    pub client_operation_id: String,
    pub source: EndpointRef,
    pub sharing_policy_version: Vec<u8>,
    pub checkpoint: Option<TransferCheckpoint>,
}

pub struct SourcePack {
    directory: tempfile::TempDir,
    revision: StateId,
    artifacts: [PackExtent; 2],
}

impl SourcePack {
    /// Performs disk I/O and compression. Async applications run preparation on
    /// their blocking-work executor. The output owns and removes its scratch.
    pub fn prepare(
        source: &impl ObjectSource,
        selected: &State,
        scratch_root: &Path,
        budget: SourceBudget,
    ) -> Result<Self, Error> {
        let directory = tempfile::Builder::new()
            .prefix("thread-source-")
            .tempdir_in(scratch_root)?;
        let pack_path = directory.path().join("source.pack");
        let index_path = directory.path().join("source.idx");
        let pack = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&pack_path)?;
        let builder = StreamingPackBuilder::new(
            pack,
            index_path.clone(),
            Default::default(),
            directory.path().join("buckets"),
        )
        .map_err(store_error)?;
        let (pack, _) = build_source_pack(
            builder,
            source,
            selected,
            budget.max_objects,
            budget.max_decoded_bytes,
        )
        .map_err(store_error)?;
        drop(pack);
        let artifacts = [
            artifact(&pack_path, pack_extent::Kind::NativePack)?,
            artifact(&index_path, pack_extent::Kind::NativeIndex)?,
        ];
        Ok(Self {
            directory,
            revision: selected.id(),
            artifacts,
        })
    }

    /// Whole, independently hashed artifacts in transmission order. Source
    /// preparation bounds the decoded closure before producing this inventory.
    pub fn artifacts(&self) -> &[PackExtent; 2] {
        &self.artifacts
    }

    /// Open the exact prepared artifacts without buffering their bodies. Keep
    /// this SourcePack alive until the readers finish; dropping it removes its
    /// temporary files, including after a cancelled transfer.
    pub async fn open_artifacts(&self) -> Result<[tokio::fs::File; 2], Error> {
        Ok([
            tokio::fs::File::open(self.directory.path().join("source.pack")).await?,
            tokio::fs::File::open(self.directory.path().join("source.idx")).await?,
        ])
    }

    pub fn revision(&self) -> StateId {
        self.revision
    }

    /// The same inventory digest verified by the publication receipt.
    pub fn inventory_digest(&self) -> Result<[u8; 32], Error> {
        use prost::Message;
        let mut inventory = Vec::new();
        for extent in &self.artifacts {
            extent
                .encode_length_delimited(&mut inventory)
                .map_err(|_| Error::Invalid("inventory encoding failed"))?;
        }
        Ok(typed_digest("thread-source-inventory-v1", &inventory))
    }
}

impl<T: RpcTransport<Error = transport::Error>> Thread<'_, T> {
    /// One exchange, directly to this Thread's endpoint. The source capture must
    /// already be causally admitted there. Preparation and binding are local;
    /// no head lookup, proxy call or implicit capture happens here.
    pub async fn publish_source(
        &self,
        source: &SourcePack,
        options: PublicationOptions,
    ) -> Result<PublicationReceipt, Error> {
        if self
            .reference
            .spool
            .as_ref()
            .is_none_or(|spool| spool.id.is_empty())
            || self
                .reference
                .id
                .as_ref()
                .is_none_or(|id| id.value.len() != 32)
            || options.source.public_key.len() != 32
            || options.sharing_policy_version.len() != 32
        {
            return Err(Error::Invalid(
                "Thread, source endpoint and sharing policy version required",
            ));
        }
        let destination = self
            .remote
            .description
            .endpoint
            .clone()
            .ok_or(Error::Invalid("remote endpoint identity missing"))?;
        let opening = PublishContentClientFrame {
            client_operation_id: options.client_operation_id,
            body: Some(publish_content_client_frame::Body::Open(
                PublishContentOpen {
                    thread: Some(self.reference.clone()),
                    revision: Some(RevisionRef {
                        spool: self.reference.spool.clone(),
                        revision: Some(revision_ref::Revision::State(
                            api::heddle::api::v1alpha1::StateId {
                                value: source.revision.as_bytes().to_vec(),
                            },
                        )),
                    }),
                    sharing_policy_version: options.sharing_policy_version,
                    packs: source.artifacts.to_vec(),
                    checkpoint: options.checkpoint,
                    source: Some(options.source),
                    destination: Some(destination),
                },
            )),
        };
        let [pack, index] = source.open_artifacts().await?;
        self.remote.publish_content(&opening, [pack, index]).await
    }
}

fn artifact(path: &Path, kind: pack_extent::Kind) -> Result<PackExtent, Error> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut hash = blake3::Hasher::new();
    hash.update_reader(&mut file)?;
    let address = ObjectAddress {
        algorithm: "blake3".into(),
        digest: hash.finalize().as_bytes().to_vec(),
    };
    Ok(PackExtent {
        pack: Some(address.clone()),
        kind: kind as i32,
        offset: 0,
        length,
        extent_digest: Some(address),
    })
}
fn store_error(error: impl std::fmt::Display) -> Error {
    transport::Error::Io(error.to_string()).into()
}

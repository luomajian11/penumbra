use std::sync::{Arc, Mutex};

use crate::{sync::scan_block, Storage};
use penumbra_crypto::FullViewingKey;
use penumbra_proto::client::oblivious::{
    oblivious_query_client::ObliviousQueryClient, AssetListRequest, CompactBlockRangeRequest,
};
use tokio::sync::{
    mpsc::{error::TryRecvError, Receiver},
    RwLock,
};
use tonic::transport::Channel;
pub struct Worker {
    storage: Storage,
    client: ObliviousQueryClient<Channel>,
    nct: Arc<RwLock<penumbra_tct::Tree>>,
    fvk: FullViewingKey, // TODO: notifications (see TODOs on ViewService)
    error_slot: Arc<Mutex<Option<anyhow::Error>>>,
    shutdown_rx: Receiver<()>,
}

impl Worker {
    pub async fn new(
        storage: Storage,
        client: ObliviousQueryClient<Channel>,
        error_slot: Arc<Mutex<Option<anyhow::Error>>>,
        rx: Receiver<()>,
    ) -> Result<(Self, Arc<RwLock<penumbra_tct::Tree>>), anyhow::Error> {
        let nct = Arc::new(RwLock::new(storage.note_commitment_tree().await?));
        let fvk = storage.full_viewing_key().await?;
        Ok((
            Self {
                storage,
                client,
                nct: nct.clone(),
                fvk,
                error_slot,
                shutdown_rx: rx,
            },
            nct,
        ))
    }

    pub async fn fetch_assets(&mut self) -> Result<(), anyhow::Error> {
        tracing::info!("fetching assets");

        let chain_id = self.storage.chain_params().await?.chain_id;

        let known_assets = self
            .client
            .asset_list(tonic::Request::new(AssetListRequest { chain_id }))
            .await?
            .into_inner();

        for known_asset in known_assets.assets {
            self.storage.record_asset(known_asset.try_into()?).await?;
        }

        tracing::info!("updated asset cache");

        Ok(())
    }

    pub async fn sync_to_latest(&mut self) -> Result<u64, anyhow::Error> {
        // Do a single sync run, up to whatever the latest block height is
        tracing::info!("starting client sync");

        // Lock the NCT during sync
        let mut nct = self.nct.write().await;

        let start_height = self
            .storage
            .last_sync_height()
            .await?
            .map(|h| h + 1)
            .unwrap_or(0);

        let epoch_duration = self.storage.chain_params().await?.epoch_duration;

        let mut stream = self
            .client
            .compact_block_range(tonic::Request::new(CompactBlockRangeRequest {
                start_height,
                end_height: 0,
                chain_id: self.storage.chain_params().await?.chain_id,
            }))
            .await?
            .into_inner();

        while let Some(block) = stream.message().await? {
            let scan_result = scan_block(&self.fvk, &mut nct, block.try_into()?, epoch_duration);

            self.storage.record_block(scan_result, &mut nct).await?;
        }

        let end_height = self.storage.last_sync_height().await?.unwrap();

        // Release the NCT RwLock
        drop(nct);

        tracing::info!(?end_height, "finished sync");

        Ok(end_height)
    }

    pub async fn run(mut self) -> Result<(), anyhow::Error> {
        loop {
            match self.run_inner().await {
                Ok(_) => {
                    // If the worker returns `Ok` then it means it's done, so we can
                    // stop looping.
                    break;
                }
                Err(e) => {
                    tracing::info!(?e, "view worker error");
                    self.error_slot.lock().unwrap().replace(e);
                }
            };
        }

        Ok(())
    }

    async fn run_inner(&mut self) -> Result<(), anyhow::Error> {
        loop {
            self.sync_to_latest().await?;
            self.fetch_assets().await?;

            if let Err(TryRecvError::Disconnected) = self.shutdown_rx.try_recv() {
                // All senders have been dropped, so we can shut down.
                tracing::info!("All senders dropped, wallet worker shutting down.");
                break;
            }

            // TODO 1: randomize sleep interval within some range?
            // TODO 2: use websockets to be notified on new block
            tokio::time::sleep(std::time::Duration::from_millis(1729)).await;
        }

        // If this is returned, it means the loop was broken by a shutdown signal.
        Ok(())
    }
}
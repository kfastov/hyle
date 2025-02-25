//! Index system for historical data.

mod api;
pub mod contract_handlers;
pub mod contract_state_indexer;
pub mod da_listener;

use crate::model::*;
use crate::utils::logger::LogMe;
use crate::{
    module_handle_messages,
    node_state::module::NodeStateEvent,
    utils::modules::{module_bus_client, Module},
};
use anyhow::{bail, Context, Error, Result};
use api::IndexerAPI;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use chrono::DateTime;
use hyle_contract_sdk::TxHash;
use hyle_model::api::{BlobWithStatus, TransactionStatus, TransactionType, TransactionWithBlobs};
use sqlx::Row;
use sqlx::{postgres::PgPoolOptions, PgPool, Pool, Postgres};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{broadcast, mpsc};
use tracing::trace;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

module_bus_client! {
#[derive(Debug)]
struct IndexerBusClient {
    receiver(NodeStateEvent),
}
}

// TODO: generalize for all tx types
type Subscribers = HashMap<ContractName, Vec<broadcast::Sender<TransactionWithBlobs>>>;

#[derive(Debug, Clone)]
pub struct IndexerApiState {
    db: PgPool,
    new_sub_sender: mpsc::Sender<(ContractName, WebSocket)>,
}

#[derive(Debug)]
pub struct Indexer {
    bus: IndexerBusClient,
    state: IndexerApiState,
    new_sub_receiver: tokio::sync::mpsc::Receiver<(ContractName, WebSocket)>,
    subscribers: Subscribers,
}

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./src/indexer/migrations");

impl Module for Indexer {
    type Context = Arc<CommonRunContext>;

    async fn build(ctx: Self::Context) -> Result<Self> {
        let bus = IndexerBusClient::new_from_bus(ctx.bus.new_handle()).await;

        let pool = PgPoolOptions::new()
            .max_connections(20)
            .acquire_timeout(std::time::Duration::from_secs(1))
            .connect(&ctx.config.database_url)
            .await
            .context("Failed to connect to the database")?;

        let _ =
            tokio::time::timeout(tokio::time::Duration::from_secs(60), MIGRATOR.run(&pool)).await?;

        let (new_sub_sender, new_sub_receiver) = tokio::sync::mpsc::channel(100);

        let subscribers = HashMap::new();

        let indexer = Indexer {
            bus,
            state: IndexerApiState {
                db: pool,
                new_sub_sender,
            },
            new_sub_receiver,
            subscribers,
        };

        if let Ok(mut guard) = ctx.router.lock() {
            if let Some(router) = guard.take() {
                guard.replace(router.nest("/v1/indexer", indexer.api(Some(&ctx))));
                return Ok(indexer);
            }
        }

        if let Ok(mut guard) = ctx.openapi.lock() {
            tracing::info!("Adding OpenAPI for Indexer");
            let openapi = guard.clone().nest("/v1/indexer", IndexerAPI::openapi());
            *guard = openapi;
        } else {
            tracing::error!("Failed to add OpenAPI for Indexer");
        }

        anyhow::bail!("context router should be available");
    }

    fn run(&mut self) -> impl futures::Future<Output = Result<()>> + Send {
        self.start()
    }
}

impl Indexer {
    pub async fn start(&mut self) -> Result<()> {
        module_handle_messages! {
            on_bus self.bus,
            listen<NodeStateEvent> event => {
                _ = self.handle_node_state_event(event)
                    .await
                    .log_error("Handling node state event");
            }

            Some((contract_name, mut socket)) = self.new_sub_receiver.recv() => {

                let (tx, mut rx) = broadcast::channel(100);
                // Append tx to the list of subscribers for contract_name
                self.subscribers.entry(contract_name)
                    .or_default()
                    .push(tx);

                tokio::task::Builder::new()
                    .name("indexer-recv")
                    .spawn(async move {
                        while let Ok(transaction) = rx.recv().await {
                            if let Ok(json) = serde_json::to_vec(&transaction)
                                    .log_error("Serialize transaction to JSON") {
                                if socket.send(Message::Binary(json.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    })?;
            }
        };
        Ok(())
    }

    pub async fn get_last_block(&self) -> Result<Option<BlockHeight>> {
        let rows = sqlx::query("SELECT max(height) as max FROM blocks")
            .fetch_one(&self.state.db)
            .await?;
        Ok(rows
            .try_get("max")
            .map(|m: i64| Some(BlockHeight(m as u64)))
            .unwrap_or(None))
    }

    pub fn api(&self, ctx: Option<&CommonRunContext>) -> Router<()> {
        #[derive(OpenApi)]
        struct IndexerAPI;

        let (router, api) = OpenApiRouter::with_openapi(IndexerAPI::openapi())
            // block
            .routes(routes!(api::get_blocks))
            .routes(routes!(api::get_last_block))
            .routes(routes!(api::get_block))
            .routes(routes!(api::get_block_by_hash))
            // transaction
            .routes(routes!(api::get_transactions))
            .routes(routes!(api::get_transactions_by_height))
            .routes(routes!(api::get_transactions_by_contract))
            .routes(routes!(api::get_transaction_with_hash))
            .routes(routes!(api::get_blob_transactions_by_contract))
            .route(
                "/blob_transactions/contract/{contract_name}/ws",
                get(Self::get_blob_transactions_by_contract_ws_handler),
            )
            // blob
            .routes(routes!(api::get_blobs_by_tx_hash))
            .routes(routes!(api::get_blob))
            // contract
            .routes(routes!(api::list_contracts))
            .routes(routes!(api::get_contract))
            .routes(routes!(api::get_contract_state_by_height))
            .split_for_parts();

        if let Some(ctx) = ctx {
            if let Ok(mut o) = ctx.openapi.lock() {
                *o = o.clone().nest("/v1/indexer", api);
            }
        }

        router.with_state(self.state.clone())
    }

    async fn get_blob_transactions_by_contract_ws_handler(
        ws: WebSocketUpgrade,
        Path(contract_name): Path<String>,
        State(state): State<IndexerApiState>,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |socket| {
            Self::get_blob_transactions_by_contract_ws(socket, contract_name, state.new_sub_sender)
        })
    }

    async fn get_blob_transactions_by_contract_ws(
        socket: WebSocket,
        contract_name: String,
        new_sub_sender: mpsc::Sender<(ContractName, WebSocket)>,
    ) {
        // TODO: properly handle errors and ws messages
        _ = new_sub_sender
            .send((ContractName(contract_name), socket))
            .await;
    }

    async fn handle_node_state_event(&mut self, event: NodeStateEvent) -> Result<(), Error> {
        match event {
            NodeStateEvent::NewBlock(block) => self.handle_processed_block(*block).await,
        }
    }

    async fn handle_processed_block(&mut self, block: Block) -> Result<(), Error> {
        trace!("Indexing block at height {:?}", block.block_height);
        let mut transaction = self.state.db.begin().await?;

        // Insert the block into the blocks table
        let block_hash = &block.hash;
        let block_height = i64::try_from(block.block_height.0)
            .map_err(|_| anyhow::anyhow!("Block height is too large to fit into an i64"))?;

        let block_timestamp = match DateTime::from_timestamp(
            i64::try_from(block.block_timestamp)
                .map_err(|_| anyhow::anyhow!("Timestamp too large for i64"))?,
            0,
        ) {
            Some(date) => date,
            None => bail!("Block's timestamp is incorrect"),
        };

        sqlx::query(
            "INSERT INTO blocks (hash, parent_hash, height, timestamp) VALUES ($1, $2, $3, $4)",
        )
        .bind(block_hash)
        .bind(block.parent_hash)
        .bind(block_height)
        .bind(block_timestamp)
        .execute(&mut *transaction)
        .await?;

        let mut i: i32 = 0;
        #[allow(clippy::explicit_counter_loop)]
        for tx in block.txs {
            let tx_hash: TxHash = tx.hash();
            let version = i32::try_from(tx.version)
                .map_err(|_| anyhow::anyhow!("Tx version is too large to fit into an i32"))?;

            // Insert the transaction into the transactions table
            let tx_type = TransactionType::get_type_from_transaction(&tx);
            let tx_status = match tx.transaction_data {
                TransactionData::Blob(_) => TransactionStatus::Sequenced,
                TransactionData::Proof(_) => TransactionStatus::Success,
                TransactionData::VerifiedProof(_) => TransactionStatus::Success,
            };

            let tx_hash: &TxHashDb = &tx_hash.into();

            sqlx::query(
                "INSERT INTO transactions (tx_hash, block_hash, index, version, transaction_type, transaction_status)
                VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(tx_hash)
            .bind(block_hash)
            .bind(i)
            .bind(version)
            .bind(tx_type)
            .bind(tx_status)
            .execute(&mut *transaction)
            .await?;

            i += 1;

            match tx.transaction_data {
                TransactionData::Blob(blob_tx) => {
                    for (blob_index, blob) in blob_tx.blobs.iter().enumerate() {
                        let blob_index = i32::try_from(blob_index).map_err(|_| {
                            anyhow::anyhow!("Blob index is too large to fit into an i32")
                        })?;
                        // Send the transaction to all websocket subscribers
                        self.send_blob_transaction_to_websocket_subscribers(
                            &blob_tx,
                            tx_hash,
                            block_hash,
                            i as u32,
                            version as u32,
                        );

                        let identity = &blob_tx.identity.0;
                        let contract_name = &blob.contract_name.0;
                        let blob_data = &blob.data.0;
                        sqlx::query(
                            "INSERT INTO blobs (tx_hash, blob_index, identity, contract_name, data, verified)
                             VALUES ($1, $2, $3, $4, $5, $6)",
                        )
                        .bind(tx_hash)
                        .bind(blob_index)
                        .bind(identity)
                        .bind(contract_name)
                        .bind(blob_data)
                        .bind(false)
                        .execute(&mut *transaction)
                        .await?;
                    }
                }
                TransactionData::VerifiedProof(tx_data) => {
                    // Then insert the proof in to the proof table.
                    let proof = match tx_data.proof {
                        Some(proof_data) => proof_data.0,
                        None => {
                            tracing::trace!(
                                "Verified proof TX {:?} does not contain a proof",
                                &tx_hash
                            );
                            continue;
                        }
                    };

                    sqlx::query("INSERT INTO proofs (tx_hash, proof) VALUES ($1, $2)")
                        .bind(tx_hash)
                        .bind(proof)
                        .execute(&mut *transaction)
                        .await?;
                }
                _ => {
                    bail!("Unsupported transaction type");
                }
            }
        }

        // Handling new stakers
        for _staker in block.staking_actions {
            // TODO: add new table with stakers at a given height
        }

        // Handling settled blob transactions
        for settled_blob_tx_hash in block.successful_txs {
            let tx_hash: &TxHashDb = &settled_blob_tx_hash.into();
            sqlx::query("UPDATE transactions SET transaction_status = $1 WHERE tx_hash = $2")
                .bind(TransactionStatus::Success)
                .bind(tx_hash)
                .execute(&mut *transaction)
                .await?;
        }

        for failed_blob_tx_hash in block.failed_txs {
            let tx_hash: &TxHashDb = &failed_blob_tx_hash.into();
            sqlx::query("UPDATE transactions SET transaction_status = $1 WHERE tx_hash = $2")
                .bind(TransactionStatus::Failure)
                .bind(tx_hash)
                .execute(&mut *transaction)
                .await?;
        }

        // Handling timed out blob transactions
        for timed_out_tx_hash in block.timed_out_txs {
            let tx_hash: &TxHashDb = &timed_out_tx_hash.into();
            sqlx::query("UPDATE transactions SET transaction_status = $1 WHERE tx_hash = $2")
                .bind(TransactionStatus::TimedOut)
                .bind(tx_hash)
                .execute(&mut *transaction)
                .await?;
        }

        for handled_blob_proof_output in block.blob_proof_outputs {
            let proof_tx_hash: &TxHashDb = &handled_blob_proof_output.proof_tx_hash.into();
            let blob_tx_hash: &TxHashDb = &handled_blob_proof_output.blob_tx_hash.into();
            let blob_index = i32::try_from(handled_blob_proof_output.blob_index.0)
                .map_err(|_| anyhow::anyhow!("Blob index is too large to fit into an i32"))?;
            let blob_proof_output_index =
                i32::try_from(handled_blob_proof_output.blob_proof_output_index).map_err(|_| {
                    anyhow::anyhow!("Blob proof output index is too large to fit into an i32")
                })?;
            let serialized_hyle_output =
                serde_json::to_string(&handled_blob_proof_output.hyle_output)?;
            sqlx::query(
                "INSERT INTO blob_proof_outputs (proof_tx_hash, blob_tx_hash, blob_index, blob_proof_output_index, contract_name, hyle_output, settled)
                    VALUES ($1, $2, $3, $4, $5, $6::jsonb, false)",
            )
            .bind(proof_tx_hash)
            .bind(blob_tx_hash)
            .bind(blob_index)
            .bind(blob_proof_output_index)
            .bind(handled_blob_proof_output.contract_name.0)
            .bind(serialized_hyle_output)
            .execute(&mut *transaction)
            .await?;
        }

        // Handling verified blob (! must come after blob proof output, as it updates that)
        for (blob_tx_hash, blob_index, blob_proof_output_index) in block.verified_blobs {
            let blob_tx_hash: &TxHashDb = &blob_tx_hash.into();
            let blob_index = i32::try_from(blob_index.0)
                .map_err(|_| anyhow::anyhow!("Blob index is too large to fit into an i32"))?;

            sqlx::query("UPDATE blobs SET verified = true WHERE tx_hash = $1 AND blob_index = $2")
                .bind(blob_tx_hash)
                .bind(blob_index)
                .execute(&mut *transaction)
                .await?;

            if let Some(blob_proof_output_index) = blob_proof_output_index {
                let blob_proof_output_index =
                    i32::try_from(blob_proof_output_index).map_err(|_| {
                        anyhow::anyhow!("Blob proof output index is too large to fit into an i32")
                    })?;

                sqlx::query("UPDATE blob_proof_outputs SET settled = true WHERE blob_tx_hash = $1 AND blob_index = $2 AND blob_proof_output_index = $3")
                    .bind(blob_tx_hash)
                    .bind(blob_index)
                    .bind(blob_proof_output_index)
                    .execute(&mut *transaction)
                    .await?;
            }
        }

        // After TXes as it refers to those (for now)
        for (tx_hash, contract) in block.registered_contracts {
            let verifier = &contract.verifier.0;
            let program_id = &contract.program_id.0;
            let state_digest = &contract.state_digest.0;
            let contract_name = &contract.contract_name.0;
            let tx_hash: &TxHashDb = &tx_hash.into();

            // Adding to Contract table
            sqlx::query(
                "INSERT INTO contracts (tx_hash, verifier, program_id, state_digest, contract_name)
                VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(tx_hash)
            .bind(verifier)
            .bind(program_id)
            .bind(state_digest)
            .bind(contract_name)
            .execute(&mut *transaction)
            .await?;

            // Adding to ContractState table
            sqlx::query(
                "INSERT INTO contract_state (contract_name, block_hash, state_digest)
                VALUES ($1, $2, $3)",
            )
            .bind(contract_name)
            .bind(block_hash)
            .bind(state_digest)
            .execute(&mut *transaction)
            .await?;
        }

        // Handling updated contract state
        for (contract_name, state_digest) in block.updated_states {
            let contract_name = &contract_name.0;
            let state_digest = &state_digest.0;
            sqlx::query(
                "UPDATE contract_state SET state_digest = $1 WHERE contract_name = $2 AND block_hash = $3",
            )
            .bind(state_digest.clone())
            .bind(contract_name.clone())
            .bind(block_hash)
            .execute(&mut *transaction)
            .await?;

            sqlx::query("UPDATE contracts SET state_digest = $1 WHERE contract_name = $2")
                .bind(state_digest)
                .bind(contract_name)
                .execute(&mut *transaction)
                .await?;
        }

        // Commit the transaction
        transaction.commit().await?;

        tracing::debug!("Indexed block at height {:?}", block.block_height);

        Ok(())
    }

    fn send_blob_transaction_to_websocket_subscribers(
        &self,
        tx: &BlobTransaction,
        tx_hash: &TxHashDb,
        block_hash: &ConsensusProposalHash,
        index: u32,
        version: u32,
    ) {
        for (contrat_name, senders) in self.subscribers.iter() {
            if tx
                .blobs
                .iter()
                .any(|blob| &blob.contract_name == contrat_name)
            {
                let enriched_tx = TransactionWithBlobs {
                    tx_hash: tx_hash.0.clone(),
                    block_hash: block_hash.clone(),
                    index,
                    version,
                    transaction_type: TransactionType::BlobTransaction,
                    transaction_status: TransactionStatus::Sequenced,
                    identity: tx.identity.0.clone(),
                    blobs: tx
                        .blobs
                        .iter()
                        .map(|blob| BlobWithStatus {
                            contract_name: blob.contract_name.0.clone(),
                            data: blob.data.0.clone(),
                            proof_outputs: vec![],
                        })
                        .collect(),
                };
                senders.iter().for_each(|sender| {
                    let _ = sender.send(enriched_tx.clone());
                });
            }
        }
    }
}

impl std::ops::Deref for Indexer {
    type Target = Pool<Postgres>;

    fn deref(&self) -> &Self::Target {
        &self.state.db
    }
}

#[cfg(test)]
mod test {
    use assert_json_diff::assert_json_include;
    use axum_test::TestServer;
    use hyle_contract_sdk::{BlobIndex, HyleOutput, Identity, ProgramId, StateDigest, TxHash};
    use hyle_model::api::{APIBlock, APIContract};
    use serde_json::json;
    use std::{
        future::IntoFuture,
        net::{Ipv4Addr, SocketAddr},
    };

    use crate::{
        bus::SharedMessageBus,
        model::{
            Blob, BlobData, BlobProofOutput, ProofData, SignedBlock, Transaction, TransactionData,
            VerifiedProofTransaction,
        },
        node_state::NodeState,
    };

    use super::*;

    use sqlx::postgres::PgPoolOptions;
    use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

    async fn setup_test_server(indexer: &Indexer) -> Result<TestServer> {
        let router = indexer.api(None);
        TestServer::new(router)
    }

    async fn new_indexer(pool: PgPool) -> Indexer {
        let (new_sub_sender, new_sub_receiver) = tokio::sync::mpsc::channel(100);

        Indexer {
            bus: IndexerBusClient::new_from_bus(SharedMessageBus::default()).await,
            state: IndexerApiState {
                db: pool,
                new_sub_sender,
            },
            new_sub_receiver,
            subscribers: HashMap::new(),
        }
    }

    fn new_register_tx(contract_name: ContractName, state_digest: StateDigest) -> BlobTransaction {
        BlobTransaction {
            identity: "hyle.hyle".into(),
            blobs: vec![RegisterContractAction {
                verifier: "test".into(),
                program_id: ProgramId(vec![3, 2, 1]),
                state_digest,
                contract_name,
            }
            .as_blob("hyle".into(), None, None)],
        }
    }

    fn new_blob_tx(
        first_contract_name: ContractName,
        second_contract_name: ContractName,
    ) -> Transaction {
        Transaction {
            version: 1,
            transaction_data: TransactionData::Blob(BlobTransaction {
                identity: Identity::new("test.c1"),
                blobs: vec![
                    Blob {
                        contract_name: first_contract_name,
                        data: BlobData(vec![1, 2, 3]),
                    },
                    Blob {
                        contract_name: second_contract_name,
                        data: BlobData(vec![1, 2, 3]),
                    },
                ],
            }),
        }
    }

    fn new_proof_tx(
        contract_name: ContractName,
        blob_index: BlobIndex,
        blob_tx_hash: TxHash,
        initial_state: StateDigest,
        next_state: StateDigest,
        blobs: Vec<u8>,
    ) -> Transaction {
        let proof = ProofData(initial_state.0.clone());
        Transaction {
            version: 1,
            transaction_data: TransactionData::VerifiedProof(VerifiedProofTransaction {
                contract_name: contract_name.clone(),
                proof_hash: proof.hash(),
                proven_blobs: vec![BlobProofOutput {
                    original_proof_hash: proof.hash(),
                    program_id: ProgramId(vec![3, 2, 1]),
                    blob_tx_hash: blob_tx_hash.clone(),
                    hyle_output: HyleOutput {
                        version: 1,
                        initial_state,
                        next_state,
                        identity: Identity::new("test.c1"),
                        tx_hash: blob_tx_hash,
                        tx_ctx: None,
                        index: blob_index,
                        blobs,
                        success: true,
                        registered_contracts: vec![],
                        program_outputs: vec![],
                    },
                }],
                is_recursive: false,
                proof: Some(proof),
            }),
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_indexer_handle_block_flow() -> Result<()> {
        let container = Postgres::default().start().await.unwrap();
        let db = PgPoolOptions::new()
            .max_connections(5)
            .connect(&format!(
                "postgresql://postgres:postgres@localhost:{}/postgres",
                container.get_host_port_ipv4(5432).await.unwrap()
            ))
            .await
            .unwrap();
        MIGRATOR.run(&db).await.unwrap();

        let mut indexer = new_indexer(db).await;
        let server = setup_test_server(&indexer).await?;

        let initial_state = StateDigest(vec![1, 2, 3]);
        let next_state = StateDigest(vec![4, 5, 6]);
        let first_contract_name = ContractName::new("c1");
        let second_contract_name = ContractName::new("c2");

        let register_tx_1 = new_register_tx(first_contract_name.clone(), initial_state.clone());
        let register_tx_2 = new_register_tx(second_contract_name.clone(), initial_state.clone());

        let blob_transaction =
            new_blob_tx(first_contract_name.clone(), second_contract_name.clone());
        let blob_transaction_hash = blob_transaction.hash();

        let proof_tx_1 = new_proof_tx(
            first_contract_name.clone(),
            BlobIndex(0),
            blob_transaction_hash.clone(),
            initial_state.clone(),
            next_state.clone(),
            vec![99, 49, 1, 2, 3, 99, 50, 1, 2, 3],
        );

        let proof_tx_2 = new_proof_tx(
            second_contract_name.clone(),
            BlobIndex(1),
            blob_transaction_hash.clone(),
            initial_state.clone(),
            next_state.clone(),
            vec![99, 49, 1, 2, 3, 99, 50, 1, 2, 3],
        );

        let other_blob_transaction =
            new_blob_tx(second_contract_name.clone(), first_contract_name.clone());
        let other_blob_transaction_hash = other_blob_transaction.hash();
        // Send two proofs for the same blob
        let proof_tx_3 = new_proof_tx(
            first_contract_name.clone(),
            BlobIndex(1),
            other_blob_transaction_hash.clone(),
            StateDigest(vec![7, 7, 7]),
            StateDigest(vec![9, 9, 9]),
            vec![99, 50, 1, 2, 3, 99, 49, 1, 2, 3],
        );
        let proof_tx_4 = new_proof_tx(
            first_contract_name.clone(),
            BlobIndex(1),
            other_blob_transaction_hash.clone(),
            StateDigest(vec![8, 8]),
            StateDigest(vec![9, 9]),
            vec![99, 50, 1, 2, 3, 99, 49, 1, 2, 3],
        );

        let txs = vec![
            register_tx_1.into(),
            register_tx_2.into(),
            blob_transaction,
            proof_tx_1,
            proof_tx_2,
            other_blob_transaction,
            proof_tx_3,
            proof_tx_4,
        ];

        let mut node_state = NodeState::default();

        let mut signed_block = SignedBlock::default();
        signed_block.data_proposals.push((
            ValidatorPublicKey("ttt".into()),
            vec![DataProposal {
                id: 1,
                parent_data_proposal_hash: None,
                txs,
            }],
        ));
        let block = node_state.handle_signed_block(&signed_block);

        indexer
            .handle_processed_block(block)
            .await
            .expect("Failed to handle block");

        let transactions_response = server.get("/contract/c1").await;
        transactions_response.assert_status_ok();
        let json_response = transactions_response.json::<APIContract>();
        assert_eq!(json_response.state_digest, next_state.0);

        let transactions_response = server.get("/contract/c2").await;
        transactions_response.assert_status_ok();
        let json_response = transactions_response.json::<APIContract>();
        assert_eq!(json_response.state_digest, next_state.0);

        let blob_transactions_response = server.get("/blob_transactions/contract/c1").await;
        blob_transactions_response.assert_status_ok();
        assert_json_include!(
            actual: blob_transactions_response.json::<serde_json::Value>(),
            expected: json!([
                {
                    "blobs": [{
                        "contract_name": "c1",
                        "data": hex::encode([1,2,3]),
                        "proof_outputs": [{}]
                    }],
                    "tx_hash": blob_transaction_hash.to_string(),
                    "index": 2,
                },
                {
                    "blobs": [{
                        "contract_name": "c1",
                        "data": hex::encode([1,2,3]),
                        "proof_outputs": [
                            {
                                "initial_state": [7,7,7],
                            },
                            {
                                "initial_state": [8,8],
                            }
                        ]
                    }],
                    "transaction_status": "Sequenced",
                    "tx_hash": other_blob_transaction_hash.to_string(),
                    "index": 5,
                }
            ])
        );

        let all_txs = server.get("/transactions/block/0").await;
        all_txs.assert_status_ok();
        assert_json_include!(
            actual: all_txs.json::<serde_json::Value>(),
            expected: json!([
                { "index": 0, "transaction_type": "BlobTransaction", "transaction_status": "Success" },
                { "index": 1, "transaction_type": "BlobTransaction", "transaction_status": "Success" },
                { "index": 2, "transaction_type": "BlobTransaction", "transaction_status": "Success" },
                { "index": 3, "transaction_type": "ProofTransaction", "transaction_status": "Success" },
                { "index": 4, "transaction_type": "ProofTransaction", "transaction_status": "Success" },
                { "index": 5, "transaction_type": "BlobTransaction", "transaction_status": "Sequenced" },
                { "index": 6, "transaction_type": "ProofTransaction", "transaction_status": "Success" },
                { "index": 7, "transaction_type": "ProofTransaction", "transaction_status": "Success" },
            ])
        );

        let blob_transactions_response = server.get("/blob_transactions/contract/c2").await;
        blob_transactions_response.assert_status_ok();
        assert_json_include!(
            actual: blob_transactions_response.json::<serde_json::Value>(),
            expected: json!([
                {
                    "blobs": [{
                        "contract_name": "c2",
                        "data": hex::encode([1,2,3]),
                        "proof_outputs": [{}]
                    }],
                    "tx_hash": blob_transaction_hash.to_string(),
                },
                {
                    "blobs": [{
                        "contract_name": "c2",
                        "data": hex::encode([1,2,3]),
                        "proof_outputs": []
                    }],
                    "tx_hash": other_blob_transaction_hash.to_string(),
                }
            ])
        );

        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn test_indexer_api() -> Result<()> {
        let container = Postgres::default().start().await.unwrap();
        let db = PgPoolOptions::new()
            .max_connections(5)
            .connect(&format!(
                "postgresql://postgres:postgres@localhost:{}/postgres",
                container.get_host_port_ipv4(5432).await.unwrap()
            ))
            .await
            .unwrap();
        MIGRATOR.run(&db).await.unwrap();
        sqlx::raw_sql(include_str!("../tests/fixtures/test_data.sql"))
            .execute(&db)
            .await?;

        let mut indexer = new_indexer(db).await;
        let server = setup_test_server(&indexer).await?;

        // Blocks
        // Get all blocks
        let transactions_response = server.get("/blocks").await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Test pagination
        let transactions_response = server.get("/blocks?nb_results=1").await;
        transactions_response.assert_status_ok();
        assert_eq!(transactions_response.json::<Vec<APIBlock>>().len(), 1);
        assert_eq!(
            transactions_response
                .json::<Vec<APIBlock>>()
                .first()
                .unwrap()
                .height,
            2
        );
        let transactions_response = server.get("/blocks?nb_results=1&start_block=1").await;
        transactions_response.assert_status_ok();
        assert_eq!(transactions_response.json::<Vec<APIBlock>>().len(), 1);
        assert_eq!(
            transactions_response
                .json::<Vec<APIBlock>>()
                .first()
                .unwrap()
                .height,
            1
        );
        // Test negative end of blocks
        let transactions_response = server.get("/blocks?nb_results=10&start_block=4").await;
        transactions_response.assert_status_ok();

        // Get the last block
        let transactions_response = server.get("/block/last").await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get block by height
        let transactions_response = server.get("/block/height/1").await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get block by hash
        let transactions_response = server
            .get("/block/hash/block1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Transactions
        // Get all transactions
        let transactions_response = server.get("/transactions").await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get all transactions by height
        let transactions_response = server.get("/transactions/block/2").await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get an existing transaction by name
        let transactions_response = server.get("/transactions/contract/contract_1").await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get an unknown transaction by name
        let transactions_response = server.get("/transactions/contract/unknown_contract").await;
        transactions_response.assert_status_ok();
        assert_eq!(transactions_response.text(), "[]");

        // Get an existing transaction by hash
        let transactions_response = server
            .get("/transaction/hash/test_tx_hash_1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get an unknown transaction by hash
        let unknown_tx = server.get("/transaction/hash/1111111111111111111111111111111111111111111111111111111111111111").await;
        unknown_tx.assert_status_not_found();

        // Blobs
        // Get all transactions for a specific contract name
        let transactions_response = server.get("/blob_transactions/contract/contract_1").await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get blobs by tx_hash
        let transactions_response = server
            .get("/blobs/hash/test_tx_hash_2aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get unknown blobs by tx_hash
        let transactions_response = server
            .get("/blobs/hash/test_tx_hash_1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .await;
        transactions_response.assert_status_ok();
        assert_eq!(transactions_response.text(), "[]");

        // Get blob by tx_hash and index
        let transactions_response = server
            .get("/blob/hash/test_tx_hash_2aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/index/0")
            .await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get blob by tx_hash and unknown index
        let transactions_response = server
            .get("/blob/hash/test_tx_hash_2aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/index/1000")
            .await;
        transactions_response.assert_status_not_found();

        // Contracts
        // Get contract by name
        let transactions_response = server.get("/contract/contract_1").await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Get contract state by name and height
        let transactions_response = server.get("/state/contract/contract_1/block/1").await;
        transactions_response.assert_status_ok();
        assert!(!transactions_response.text().is_empty());

        // Websocket
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(axum::serve(listener, indexer.api(None)).into_future());

        let _ = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/blob_transactions/contract/contract_1/ws"
        ))
        .await
        .unwrap();

        if let Some(tx) = indexer.new_sub_receiver.recv().await {
            let (contract_name, _) = tx;
            assert_eq!(contract_name, ContractName::new("contract_1"));
        }

        Ok(())
    }
}

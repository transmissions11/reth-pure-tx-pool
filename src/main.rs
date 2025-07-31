use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::{sync::Arc, time::Instant};

use jemallocator::Jemalloc;
use jsonrpsee::server::ServerConfigBuilder;

use dashmap::DashMap;
use hashbrown::HashSet;

use reth_ethereum::evm::revm::primitives::{Address, U256};
use reth_ethereum::pool::{PoolTransaction, TransactionListenerKind};
use reth_ethereum::provider::ChangedAccount;
use reth_ethereum::{
    BlockBody,
    chainspec::ChainSpecBuilder,
    consensus::EthBeaconConsensus,
    evm::{EthEvmConfig, revm::primitives::alloy_primitives::BlockHash},
    network::{
        EthNetworkPrimitives, NetworkConfig, NetworkManager, api::noop::NoopNetwork,
        config::rng_secret_key,
    },
    pool::{
        CanonicalStateUpdate, CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolConfig,
        PoolUpdateKind, SubPoolLimit, TransactionPool, TransactionPoolExt,
        blobstore::InMemoryBlobStore, test_utils::OkValidator,
    },
    primitives::{Header, SealedBlock},
    provider::test_utils::NoopProvider,
    rpc::{
        EthApiBuilder,
        builder::{RethRpcModule, RpcModuleBuilder, RpcServerConfig, TransportRpcModuleConfig},
    },
    tasks::TokioTaskExecutor,
};
use thousands::Separable;

mod utils;

static TOTAL_TRANSACTIONS: AtomicU64 = AtomicU64::new(0);
static SENDER_NONCES: LazyLock<DashMap<Address, u64>> = LazyLock::new(|| DashMap::new());

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // increase the file descriptor limit so we can handle a lot of connections
    utils::increase_nofile_limit(1_000_000)?;

    // This block provider implementation is used for testing purposes.
    // NOTE: This also means that we don't have access to the blockchain and are not able to serve
    // any requests for headers or bodies which can result in dropped connections initiated by
    // remote or able to validate transaction against the latest state.
    let client = NoopProvider::default();

    let pool: Pool<
        OkValidator<EthPooledTransaction>,
        CoinbaseTipOrdering<EthPooledTransaction>,
        InMemoryBlobStore,
    > = reth_ethereum::pool::Pool::new(
        OkValidator::default(),
        CoinbaseTipOrdering::default(),
        InMemoryBlobStore::default(),
        PoolConfig {
            pending_limit: SubPoolLimit {
                max_txs: 10_000_000_000_000,
                max_size: 2 * 1024 * 1024 * 1024,
            },
            basefee_limit: SubPoolLimit {
                max_txs: 10_000_000_000_000,
                max_size: 2 * 1024 * 1024 * 1024,
            },
            queued_limit: SubPoolLimit {
                max_txs: 10_000_000_000_000,
                max_size: 2 * 1024 * 1024 * 1024,
            },
            max_new_pending_txs_notifications: 10_000_000,
            max_account_slots: 500_000,
            pending_tx_listener_buffer_size: 10_000_000_000_000,
            new_tx_listener_buffer_size: 10_000_000_000_000,
            max_queued_lifetime: Duration::from_secs(360000), // 100 hours
            minimal_protocol_basefee: 0,
            minimum_priority_fee: Some(0),
            ..Default::default()
        },
    );

    let spec = Arc::new(ChainSpecBuilder::mainnet().build());
    let rpc_builder = RpcModuleBuilder::default()
        .with_provider(client.clone())
        // Rest is just noops that do nothing
        .with_pool(pool.clone())
        .with_noop_network()
        .with_executor(Box::new(TokioTaskExecutor::default()))
        .with_evm_config(EthEvmConfig::new(spec.clone()))
        .with_consensus(EthBeaconConsensus::new(spec.clone()));

    let eth_api = EthApiBuilder::new(
        client.clone(),
        pool.clone(),
        NoopNetwork::default(),
        EthEvmConfig::mainnet(),
    )
    .build();
    let config = TransportRpcModuleConfig::default().with_http([RethRpcModule::Eth]);
    let server = rpc_builder.build(config, eth_api);

    let config = NetworkConfig::<_, EthNetworkPrimitives>::builder(rng_secret_key())
        .disable_discovery()
        .build(client);
    let transactions_manager_config = config.transactions_manager_config.clone();
    let (_handle, network, txpool, _) = NetworkManager::builder(config)
        .await?
        .transactions(pool.clone(), transactions_manager_config)
        .split_with_handle();

    let _txs_handle = txpool.handle();

    // spawn the network task
    tokio::task::spawn(network);
    // spawn the pool task
    tokio::task::spawn(txpool);

    let server_args = RpcServerConfig::http(
        ServerConfigBuilder::default()
            .max_request_body_size(1024 * 1024 * 1024) // 1GB
            .max_response_body_size(1024 * 1024 * 1024) // 1GB
            .max_subscriptions_per_connection(429496729)
            .max_connections(429496729)
            .set_message_buffer_capacity(429496729),
    )
    .with_http_address("0.0.0.0:8545".parse()?);
    let _handle = server_args.start(&server).await?;

    std::thread::spawn({
        let pool = pool.clone();
        move || {
            let mut last_pending = 0usize;
            let mut last_queued = 0usize;
            std::thread::sleep(Duration::from_secs(1)); // Skip the first tick
            loop {
                std::thread::sleep(Duration::from_secs(1));

                let (current_pending, current_queued) = pool.pending_and_queued_txn_count();
                let pending_tps = current_pending as i64 - last_pending as i64;
                let queued_tps = current_queued as i64 - last_queued as i64;
                let total_tps = pending_tps + queued_tps;
                let total_txs = current_pending + current_queued;

                println!(
                    "[~] TPS: {} ({} Pending, {} Queued), Total transactions: {} ({} Pending, {} Queued)",
                    total_tps.separate_with_commas(),
                    pending_tps.separate_with_commas(),
                    queued_tps.separate_with_commas(),
                    total_txs.separate_with_commas(),
                    current_pending.separate_with_commas(),
                    current_queued.separate_with_commas()
                );

                {
                    let start = Instant::now();

                    let block = alloy_consensus::Block::new(
                        Header {
                            gas_limit: 1000_000_000_000_000_u64,
                            ..Default::default()
                        },
                        BlockBody::default(),
                    );
                    let sealed_block = SealedBlock::new_unchecked(block, BlockHash::ZERO);

                    let mut seen_senders = HashSet::new();
                    let mut tx_hashes = Vec::new();
                    for tx in pool.all_transactions().pending.into_iter() {
                        seen_senders.insert(tx.transaction.sender());
                        tx_hashes.push(tx.transaction.hash().clone());
                    }

                    let mut changed_accounts = Vec::with_capacity(seen_senders.len());
                    for sender in seen_senders {
                        let nonce = SENDER_NONCES.get(&sender).map(|v| *v).unwrap_or(0);
                        changed_accounts.push(ChangedAccount {
                            address: sender,
                            nonce,
                            balance: U256::from(nonce),
                        });
                    }

                    let duration = start.elapsed();
                    println!(
                        "[1] Total time setting up for on_canonical_state_change: {:.2?}",
                        duration
                    );

                    let start = Instant::now();
                    pool.on_canonical_state_change(CanonicalStateUpdate {
                        new_tip: &sealed_block,
                        pending_block_base_fee: 1_000_000_000, // 1 gwei
                        pending_block_blob_fee: Some(1_000_000), // 0.001 gwei
                        changed_accounts,
                        mined_transactions: tx_hashes,
                        update_kind: PoolUpdateKind::Commit,
                    });

                    let duration = start.elapsed();
                    println!(
                        "[2] Time spent running on_canonical_state_change: {:.2?}",
                        duration
                    );
                }

                (last_pending, last_queued) = pool.pending_and_queued_txn_count();
            }
        }
    });

    tokio::spawn(async move {
        let mut txs = pool.new_transactions_listener_for(TransactionListenerKind::All);
        while let Some(tx) = txs.recv().await {
            TOTAL_TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
            let sender = tx.transaction.sender();
            let nonce = tx.transaction.nonce();
            let prev_nonce = SENDER_NONCES.get(&sender).map(|v| *v).unwrap_or(0);
            if nonce > prev_nonce {
                SENDER_NONCES.insert(sender, nonce);
            }
        }
    });

    tokio::signal::ctrl_c().await?; // Wait for Ctrl+C to exit.

    Ok(())
}

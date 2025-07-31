use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::{Arc, atomic::AtomicU64};
use std::time::{Duration, Instant};

use jsonrpsee::server::ServerConfigBuilder;

use rand::Rng;
use reth_ethereum::evm::revm::primitives::alloy_primitives::BlockHash;
use reth_ethereum::{
    BlockBody,
    chainspec::ChainSpecBuilder,
    consensus::EthBeaconConsensus,
    evm::{
        EthEvmConfig,
        revm::primitives::{HashMap, U256},
    },
    network::{
        EthNetworkPrimitives, NetworkConfig, NetworkManager, api::noop::NoopNetwork,
        config::rng_secret_key,
    },
    pool::{
        CanonicalStateUpdate, CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolConfig,
        PoolUpdateKind, SubPoolLimit, TransactionListenerKind, TransactionPool, TransactionPoolExt,
        blobstore::InMemoryBlobStore, test_utils::OkValidator,
    },
    primitives::{Block, Header, SealedBlock},
    provider::{ChangedAccount, test_utils::NoopProvider},
    rpc::{
        EthApiBuilder,
        builder::{RethRpcModule, RpcModuleBuilder, RpcServerConfig, TransportRpcModuleConfig},
    },
    tasks::TokioTaskExecutor,
};
use thousands::Separable;
use tokio::time::interval;

mod utils;

static TOTAL_TRANSACTIONS: AtomicU64 = AtomicU64::new(0);

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

    let sender_nonces = Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn({
        let pool = pool.clone();
        let sender_nonces = sender_nonces.clone();
        async move {
            let mut interval = interval(Duration::from_secs(1));
            let mut last_pending = 0usize;
            let mut last_queued = 0usize;
            interval.tick().await; // Skip the first tick
            loop {
                interval.tick().await;

                let (current_pending, current_queued) = pool.pending_and_queued_txn_count();
                let pending_tps = current_pending as i64 - last_pending as i64;
                let queued_tps = current_queued as i64 - last_queued as i64;
                let total_tps = pending_tps + queued_tps;
                let total_txs = current_pending + current_queued;

                last_pending = current_pending;
                last_queued = current_queued;

                println!(
                    "TPS: {} ({} Pending, {} Queued), Total transactions: {} ({} Pending, {} Queued)",
                    total_tps.separate_with_commas(),
                    pending_tps.separate_with_commas(),
                    queued_tps.separate_with_commas(),
                    total_txs.separate_with_commas(),
                    current_pending.separate_with_commas(),
                    current_queued.separate_with_commas()
                );

                {
                    let start = Instant::now();

                    let mut rng = rand::thread_rng();
                    let block = alloy_consensus::Block::new(
                        Header {
                            gas_limit: 1000_000_000_000_000_u64,
                            ..Default::default()
                        },
                        BlockBody::default(),
                    );
                    let sealed_block = SealedBlock::new_unchecked(block, BlockHash::ZERO);
                    pool.on_canonical_state_change(CanonicalStateUpdate {
                        new_tip: &sealed_block,
                        pending_block_base_fee: 1_000_000_000, // 1 gwei
                        pending_block_blob_fee: Some(1_000_000), // 0.001 gwei
                        changed_accounts: sender_nonces
                            .lock()
                            .unwrap()
                            .iter()
                            .map(|(address, nonce)| ChangedAccount {
                                address: *address,
                                nonce: *nonce,
                                balance: U256::from(rng.gen_range(0..1000000000000000000_u64)),
                            })
                            .collect::<Vec<ChangedAccount>>(),
                        mined_transactions: pool.pooled_transaction_hashes(),
                        update_kind: PoolUpdateKind::Commit,
                    });

                    let duration = start.elapsed();
                    println!("Time emptying queue: {:?}", duration);
                }
            }
        }
    });

    let mut txs = pool.new_transactions_listener_for(TransactionListenerKind::All);
    while let Some(tx) = txs.recv().await {
        TOTAL_TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
        let sender = tx.transaction.sender();
        let nonce = tx.transaction.nonce();
        let mut sender_nonces = sender_nonces.lock().unwrap();
        let prev_nonce = sender_nonces.get(&sender).copied().unwrap_or(0);
        if nonce > prev_nonce {
            sender_nonces.insert(sender, nonce);
        }
    }

    Ok(())
}

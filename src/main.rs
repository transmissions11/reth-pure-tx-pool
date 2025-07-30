use std::sync::Arc;
use std::time::Duration;

use jsonrpsee::server::ServerConfigBuilder;
use reth_ethereum::{
    chainspec::ChainSpecBuilder,
    consensus::EthBeaconConsensus,
    evm::EthEvmConfig,
    network::{
        EthNetworkPrimitives, NetworkConfig, NetworkManager, api::noop::NoopNetwork,
        config::rng_secret_key,
    },
    pool::{
        CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolConfig, SubPoolLimit, TransactionPool,
        blobstore::InMemoryBlobStore, test_utils::OkValidator,
    },
    provider::test_utils::NoopProvider,
    rpc::{
        EthApiBuilder,
        builder::{RethRpcModule, RpcModuleBuilder, RpcServerConfig, TransportRpcModuleConfig},
    },
    tasks::TokioTaskExecutor,
};
use tokio::time::interval;

#[tokio::main]
async fn main() -> eyre::Result<()> {
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

    // Spawn TPS monitoring task
    tokio::spawn({
        let pool = pool.clone();
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
                let total_txs = current_pending + current_queued;

                last_pending = current_pending;
                last_queued = current_queued;

                println!(
                    "Pending TPS: {}, Queued TPS: {}, Total pending: {}, Total queued: {}, Total transactions: {}",
                    pending_tps, queued_tps, current_pending, current_queued, total_txs
                );
            }
        }
    });

    // Keep the main task alive
    tokio::signal::ctrl_c().await?;

    Ok(())
}

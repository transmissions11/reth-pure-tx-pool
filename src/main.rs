use clap::Parser;
use hashbrown::HashMap;
use jsonrpsee::server::ServerConfigBuilder;
use reth_db_common::init::init_genesis;
use reth_ethereum::cli::chainspec::chain_value_parser;
use reth_ethereum::evm::revm::primitives::U256;
use reth_ethereum::node::EthereumNode;
use reth_ethereum::node::api::NodeTypesWithDBAdapter;
use reth_ethereum::pool::validate::EthTransactionValidatorBuilder;
use reth_ethereum::pool::{EthTransactionValidator, PoolTransaction};
use reth_ethereum::provider::db::mdbx::DatabaseArguments;
use reth_ethereum::provider::db::{DatabaseEnv, init_db};
use reth_ethereum::provider::providers::{BlockchainProvider, StaticFileProvider};
use reth_ethereum::provider::{ChangedAccount, ProviderFactory};
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
        blobstore::InMemoryBlobStore,
    },
    primitives::{Header, SealedBlock},
    rpc::{
        EthApiBuilder,
        builder::{RethRpcModule, RpcModuleBuilder, RpcServerConfig, TransportRpcModuleConfig},
    },
    tasks::TokioTaskExecutor,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{sync::Arc, time::Instant};
use thousands::Separable;
use tikv_jemallocator::Jemalloc;

mod utils;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(Parser)]
#[command(name = "reth-pure-tx-pool")]
struct Args {
    #[arg(long, help = "Path to the genesis JSON file")]
    chain: Option<PathBuf>,

    #[arg(long, help = "Max batch size for txpool insertion")]
    max_batch_size: Option<usize>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // increase the file descriptor limit so we can handle a lot of connections
    utils::increase_nofile_limit(1_000_000)?;

    let args = Args::parse();

    let chain_spec = if let Some(path) = args.chain {
        let genesis = fs::read_to_string(path)?;
        chain_value_parser(&genesis)?
    } else {
        Arc::new(ChainSpecBuilder::mainnet().build())
    };

    let db_path = Path::new("./data");
    if !db_path.exists() {
        fs::create_dir_all(db_path)?;
    }
    let db = Arc::new(init_db(db_path, DatabaseArguments::default())?);
    let factory = ProviderFactory::<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>::new(
        db.clone(),
        chain_spec.clone(),
        StaticFileProvider::read_write(db_path.join("static_files"))?,
    );
    init_genesis(&factory)?;

    let client = BlockchainProvider::new(factory)?;

    let blob_store = InMemoryBlobStore::default();
    let tx_validator =
        EthTransactionValidatorBuilder::new(client.clone()).build(blob_store.clone());
    let pool: Pool<
        EthTransactionValidator<_, EthPooledTransaction>,
        CoinbaseTipOrdering<EthPooledTransaction>,
        InMemoryBlobStore,
    > = reth_ethereum::pool::Pool::new(
        tx_validator,
        CoinbaseTipOrdering::default(),
        blob_store,
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

    let eth_evm_config = EthEvmConfig::new(chain_spec.clone());

    let rpc_builder = RpcModuleBuilder::default()
        .with_provider(client.clone())
        // Rest is just noops that do nothing
        .with_pool(pool.clone())
        .with_noop_network()
        .with_executor(Box::new(TokioTaskExecutor::default()))
        .with_evm_config(eth_evm_config.clone())
        .with_consensus(EthBeaconConsensus::new(chain_spec.clone()));

    let eth_api_builder = EthApiBuilder::new(
        client.clone(),
        pool.clone(),
        NoopNetwork::default(),
        eth_evm_config,
    );

    let eth_api = if let Some(batch_size) = args.max_batch_size {
        eth_api_builder.max_batch_size(batch_size).build()
    } else {
        eth_api_builder.build()
    };

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
                            gas_limit: 1_000_000_000_000_000_u64,
                            ..Default::default()
                        },
                        BlockBody::default(),
                    );
                    let sealed_block = SealedBlock::new_unchecked(block, BlockHash::ZERO);

                    let mut touched_senders_final_nonces = HashMap::new();
                    let mut tx_hashes = Vec::new();
                    for tx in pool.all_transactions().pending.into_iter() {
                        let sender = tx.transaction.sender();
                        let nonce = tx.nonce();
                        touched_senders_final_nonces
                            .entry(sender)
                            .and_modify(|existing_nonce| {
                                if nonce > *existing_nonce {
                                    *existing_nonce = nonce;
                                }
                            })
                            .or_insert(nonce);
                        tx_hashes.push(*tx.transaction.hash());
                    }

                    let mut changed_accounts =
                        Vec::with_capacity(touched_senders_final_nonces.len());
                    for (sender, nonce) in touched_senders_final_nonces {
                        changed_accounts.push(ChangedAccount {
                            address: sender,
                            nonce,
                            balance: U256::MAX - U256::from(nonce),
                        });
                    }

                    let duration = start.elapsed();
                    println!(
                        "[1] Total time setting up for on_canonical_state_change: {duration:.2?}"
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
                    println!("[2] Time spent running on_canonical_state_change: {duration:.2?}");
                }

                (last_pending, last_queued) = pool.pending_and_queued_txn_count();
            }
        }
    });

    tokio::signal::ctrl_c().await?; // Wait for Ctrl+C to exit.

    Ok(())
}

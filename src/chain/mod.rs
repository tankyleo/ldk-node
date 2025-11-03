// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod bitcoind;
mod electrum;
mod esplora;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bitcoin::{Script, Transaction, Txid};
use lightning::chain::Filter;
use lightning_block_sync::gossip::UtxoSource;

use crate::chain::bitcoind::BitcoindChainSource;
use crate::chain::electrum::ElectrumChainSource;
use crate::chain::esplora::EsploraChainSource;
use crate::config::{
	BackgroundSyncConfig, BitcoindRestClientConfig, Config, ElectrumSyncConfig, EsploraSyncConfig,
	RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL, WALLET_SYNC_INTERVAL_MINIMUM_SECS,
};
use crate::fee_estimator::OnchainFeeEstimator;
use crate::io::utils::write_node_metrics;
use crate::logger::{log_debug, log_info, log_trace, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{Broadcaster, ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

pub(crate) enum WalletSyncStatus {
	Completed,
	InProgress { subscribers: tokio::sync::broadcast::Sender<Result<(), Error>> },
}

impl WalletSyncStatus {
	fn register_or_subscribe_pending_sync(
		&mut self,
	) -> Option<tokio::sync::broadcast::Receiver<Result<(), Error>>> {
		match self {
			WalletSyncStatus::Completed => {
				// We're first to register for a sync.
				let (tx, _) = tokio::sync::broadcast::channel(1);
				*self = WalletSyncStatus::InProgress { subscribers: tx };
				None
			},
			WalletSyncStatus::InProgress { subscribers } => {
				// A sync is in-progress, we subscribe.
				let rx = subscribers.subscribe();
				Some(rx)
			},
		}
	}

	fn propagate_result_to_subscribers(&mut self, res: Result<(), Error>) {
		// Send the notification to any other tasks that might be waiting on it by now.
		{
			match self {
				WalletSyncStatus::Completed => {
					// No sync in-progress, do nothing.
					return;
				},
				WalletSyncStatus::InProgress { subscribers } => {
					// A sync is in-progress, we notify subscribers.
					if subscribers.receiver_count() > 0 {
						match subscribers.send(res) {
							Ok(_) => (),
							Err(e) => {
								debug_assert!(
									false,
									"Failed to send wallet sync result to subscribers: {:?}",
									e
								);
							},
						}
					}
					*self = WalletSyncStatus::Completed;
				},
			}
		}
	}
}

pub(crate) struct ChainSource {
	kind: ChainSourceKind,
	tx_broadcaster: Arc<Broadcaster>,
	logger: Arc<Logger>,
}

enum ChainSourceKind {
	Esplora(EsploraChainSource),
	Electrum(ElectrumChainSource),
	Bitcoind(BitcoindChainSource),
}

impl ChainSource {
	pub(crate) fn new_esplora(
		server_url: String, headers: HashMap<String, String>, sync_config: EsploraSyncConfig,
		onchain_wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let esplora_chain_source = EsploraChainSource::new(
			server_url,
			headers,
			sync_config,
			onchain_wallet,
			fee_estimator,
			kv_store,
			config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Esplora(esplora_chain_source);
		Self { kind, tx_broadcaster, logger }
	}

	pub(crate) fn new_electrum(
		server_url: String, sync_config: ElectrumSyncConfig, onchain_wallet: Arc<Wallet>,
		fee_estimator: Arc<OnchainFeeEstimator>, tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>, config: Arc<Config>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let electrum_chain_source = ElectrumChainSource::new(
			server_url,
			sync_config,
			onchain_wallet,
			fee_estimator,
			kv_store,
			config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Electrum(electrum_chain_source);
		Self { kind, tx_broadcaster, logger }
	}

	pub(crate) fn new_bitcoind_rpc(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		onchain_wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let bitcoind_chain_source = BitcoindChainSource::new_rpc(
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			onchain_wallet,
			fee_estimator,
			kv_store,
			config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Bitcoind(bitcoind_chain_source);
		Self { kind, tx_broadcaster, logger }
	}

	pub(crate) fn new_bitcoind_rest(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		onchain_wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>, kv_store: Arc<DynStore>, config: Arc<Config>,
		rest_client_config: BitcoindRestClientConfig, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let bitcoind_chain_source = BitcoindChainSource::new_rest(
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			onchain_wallet,
			fee_estimator,
			kv_store,
			config,
			rest_client_config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Bitcoind(bitcoind_chain_source);
		Self { kind, tx_broadcaster, logger }
	}

	pub(crate) fn start(&self, runtime: Arc<Runtime>) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.start(runtime)?
			},
			_ => {
				// Nothing to do for other chain sources.
			},
		}
		Ok(())
	}

	pub(crate) fn stop(&self) {
		match &self.kind {
			ChainSourceKind::Electrum(electrum_chain_source) => electrum_chain_source.stop(),
			_ => {
				// Nothing to do for other chain sources.
			},
		}
	}

	pub(crate) fn as_utxo_source(&self) -> Option<Arc<dyn UtxoSource>> {
		match &self.kind {
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
				Some(bitcoind_chain_source.as_utxo_source())
			},
			_ => None,
		}
	}

	pub(crate) fn is_transaction_based(&self) -> bool {
		match &self.kind {
			ChainSourceKind::Esplora(_) => true,
			ChainSourceKind::Electrum { .. } => true,
			ChainSourceKind::Bitcoind { .. } => false,
		}
	}

	pub(crate) async fn continuously_sync_wallets(
		&self, stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				if let Some(background_sync_config) =
					esplora_chain_source.sync_config.background_sync_config.as_ref()
				{
					self.start_tx_based_sync_loop(
						stop_sync_receiver,
						channel_manager,
						chain_monitor,
						output_sweeper,
						background_sync_config,
						Arc::clone(&self.logger),
					)
					.await
				} else {
					// Background syncing is disabled
					log_info!(
						self.logger,
						"Background syncing is disabled. Manual syncing required for onchain wallet, lightning wallet, and fee rate updates.",
					);
					return;
				}
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				if let Some(background_sync_config) =
					electrum_chain_source.sync_config.background_sync_config.as_ref()
				{
					self.start_tx_based_sync_loop(
						stop_sync_receiver,
						channel_manager,
						chain_monitor,
						output_sweeper,
						background_sync_config,
						Arc::clone(&self.logger),
					)
					.await
				} else {
					// Background syncing is disabled
					log_info!(
						self.logger,
						"Background syncing is disabled. Manual syncing required for onchain wallet, lightning wallet, and fee rate updates.",
					);
					return;
				}
			},
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
				bitcoind_chain_source
					.continuously_sync_wallets(
						stop_sync_receiver,
						channel_manager,
						chain_monitor,
						output_sweeper,
					)
					.await
			},
		}
	}

	async fn start_tx_based_sync_loop(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>, background_sync_config: &BackgroundSyncConfig,
		logger: Arc<Logger>,
	) {
		// Setup syncing intervals
		let onchain_wallet_sync_interval_secs = background_sync_config
			.onchain_wallet_sync_interval_secs
			.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		let mut onchain_wallet_sync_interval =
			tokio::time::interval(Duration::from_secs(onchain_wallet_sync_interval_secs));
		onchain_wallet_sync_interval
			.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		let fee_rate_cache_update_interval_secs = background_sync_config
			.fee_rate_cache_update_interval_secs
			.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		let mut fee_rate_update_interval =
			tokio::time::interval(Duration::from_secs(fee_rate_cache_update_interval_secs));
		// When starting up, we just blocked on updating, so skip the first tick.
		fee_rate_update_interval.reset();
		fee_rate_update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		let lightning_wallet_sync_interval_secs = background_sync_config
			.lightning_wallet_sync_interval_secs
			.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		let mut lightning_wallet_sync_interval =
			tokio::time::interval(Duration::from_secs(lightning_wallet_sync_interval_secs));
		lightning_wallet_sync_interval
			.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		// Start the syncing loop.
		loop {
			tokio::select! {
				_ = stop_sync_receiver.changed() => {
					log_trace!(
						logger,
						"Stopping background syncing on-chain wallet.",
						);
					return;
				}
				_ = onchain_wallet_sync_interval.tick() => {
					let _ = self.sync_onchain_wallet().await;
				}
				_ = fee_rate_update_interval.tick() => {
					let _ = self.update_fee_rate_estimates().await;
				}
				_ = lightning_wallet_sync_interval.tick() => {
					let _ = self.sync_lightning_wallet(
						Arc::clone(&channel_manager),
						Arc::clone(&chain_monitor),
						Arc::clone(&output_sweeper),
						).await;
				}
			}
		}
	}

	// Synchronize the onchain wallet via transaction-based protocols (i.e., Esplora, Electrum,
	// etc.)
	pub(crate) async fn sync_onchain_wallet(&self) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.sync_onchain_wallet().await
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.sync_onchain_wallet().await
			},
			ChainSourceKind::Bitcoind { .. } => {
				// In BitcoindRpc mode we sync lightning and onchain wallet in one go via
				// `ChainPoller`. So nothing to do here.
				unreachable!("Onchain wallet will be synced via chain polling")
			},
		}
	}

	// Synchronize the Lightning wallet via transaction-based protocols (i.e., Esplora, Electrum,
	// etc.)
	pub(crate) async fn sync_lightning_wallet(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source
					.sync_lightning_wallet(channel_manager, chain_monitor, output_sweeper)
					.await
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source
					.sync_lightning_wallet(channel_manager, chain_monitor, output_sweeper)
					.await
			},
			ChainSourceKind::Bitcoind { .. } => {
				// In BitcoindRpc mode we sync lightning and onchain wallet in one go via
				// `ChainPoller`. So nothing to do here.
				unreachable!("Lightning wallet will be synced via chain polling")
			},
		}
	}

	pub(crate) async fn poll_and_update_listeners(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Esplora { .. } => {
				// In Esplora mode we sync lightning and onchain wallets via
				// `sync_onchain_wallet` and `sync_lightning_wallet`. So nothing to do here.
				unreachable!("Listeners will be synced via transction-based syncing")
			},
			ChainSourceKind::Electrum { .. } => {
				// In Electrum mode we sync lightning and onchain wallets via
				// `sync_onchain_wallet` and `sync_lightning_wallet`. So nothing to do here.
				unreachable!("Listeners will be synced via transction-based syncing")
			},
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
				bitcoind_chain_source
					.poll_and_update_listeners(channel_manager, chain_monitor, output_sweeper)
					.await
			},
		}
	}

	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.update_fee_rate_estimates().await
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.update_fee_rate_estimates().await
			},
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
				bitcoind_chain_source.update_fee_rate_estimates().await
			},
		}
	}

	pub(crate) async fn continuously_process_broadcast_queue(
		&self, mut stop_tx_bcast_receiver: tokio::sync::watch::Receiver<()>,
	) {
		let mut receiver = self.tx_broadcaster.get_broadcast_queue().await;
		loop {
			let tx_bcast_logger = Arc::clone(&self.logger);
			tokio::select! {
				_ = stop_tx_bcast_receiver.changed() => {
					log_debug!(
						tx_bcast_logger,
						"Stopping broadcasting transactions.",
					);
					return;
				}
				Some(next_package) = receiver.recv() => {
					match &self.kind {
						ChainSourceKind::Esplora(esplora_chain_source) => {
							esplora_chain_source.process_broadcast_package(next_package).await
						},
						ChainSourceKind::Electrum(electrum_chain_source) => {
							electrum_chain_source.process_broadcast_package(next_package).await
						},
						ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
							bitcoind_chain_source.process_broadcast_package(next_package).await
						},
					}
				}
			}
		}
	}
}

impl Filter for ChainSource {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.register_tx(txid, script_pubkey)
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.register_tx(txid, script_pubkey)
			},
			ChainSourceKind::Bitcoind { .. } => (),
		}
	}
	fn register_output(&self, output: lightning::chain::WatchedOutput) {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.register_output(output)
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.register_output(output)
			},
			ChainSourceKind::Bitcoind { .. } => (),
		}
	}
}

fn periodically_archive_fully_resolved_monitors(
	channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
	kv_store: Arc<DynStore>, logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
) -> Result<(), Error> {
	let mut locked_node_metrics = node_metrics.write().unwrap();
	let cur_height = channel_manager.current_best_block().height;
	let should_archive = locked_node_metrics
		.latest_channel_monitor_archival_height
		.as_ref()
		.map_or(true, |h| cur_height >= h + RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL);

	if should_archive {
		chain_monitor.archive_fully_resolved_channel_monitors();
		locked_node_metrics.latest_channel_monitor_archival_height = Some(cur_height);
		write_node_metrics(&*locked_node_metrics, kv_store, logger)?;
	}
	Ok(())
}

fn topologically_sort_single_child(txs: &mut [Transaction]) {
	if txs.len() <= 1 {
		panic!("Nothing to sort if the package is empty or has len 1")
	};
	let mut txids: Vec<_> = txs.iter().map(|tx| tx.compute_txid()).collect();
	txids.sort_unstable();
	let mut child_ptr = None;
	for (i, tx) in txs.iter().enumerate() {
		for input in tx.input.iter() {
			if let Ok(_) = txids.binary_search(&input.previous_output.txid) {
				child_ptr = Some(i);
				break;
			}
		}
	}
	txs.swap(txs.len() - 1, child_ptr.expect("child not found"));
}

#[cfg(test)]
mod tests {
	use super::*;

	use bitcoin::consensus::Decodable;
	use bitcoin::hex::FromHex;
	use bitcoin::Transaction;

	#[test]
	fn test_topologically_sort_child() {
		let parent_tx_hex = "030000000001014271edaf38b7fe0af0fa1d15d6ddf4784944f7168024ef5f765722a4f78d3d3c0100000000752c71800300000000000000000451024e731d25000000000000160014e16e0237f3ebd356e9afbcfe0cc54e4491bad90045370000000000002200200b05341083d40bc7c327b2d1e5b5a9a287d0735420890cdb2a4110a9b3d053e80400473044022060b4de1a7c6ed7c2af367e895e29e0d6815565b3356c5dcffdd65a5912c2403702207f518eecff0a4015bac6a2abf7c0c4249c21b0184232448366ca8c359042cdd601473044022018a29a31afebddbe77fa5474d441103ac116af18f91e7a9047cd228b34775dbf02205156da6637ca660ba202f7cd748bc26911e6159c99f7a4e050b51eb45c7c4e19014752210223519c7fe517d2b8f77687a9c74f3fa0e721f40aa9dc2073054ed23da9d90cf32102cf616337c3bc6d4831e47dbfb70e893747765c6d016b28730d05fb1e53e765de52ae5ea69120";
		let child_tx_hex = "030000000001026b52aeb30d110dae822a3e72fe29fef14bc306ad2f650ccd02a7fc2aa161c6a80000000000fdffffff4271edaf38b7fe0af0fa1d15d6ddf4784944f7168024ef5f765722a4f78d3d3c0000000000000000000131420000000000001600145b4db32622cf60c14ac2d6e70c913e15480527bf0002473044022062e31b6d586a84588eb938fa434eeaf06ab128262539a10b52b508b3689da1f302205f75cb334229817a880c696bc307ded6c4fc47bedbd795dbf03988e9a9bfc7e9012103bd291add58d63dccc9bf5fefdee51bbbdf5ce0e95e5e34fbab67b245c410b62100000000";
		let parent_tx_bytes = Vec::from_hex(parent_tx_hex).expect("valid hex digits");
		let child_tx_bytes = Vec::from_hex(child_tx_hex).expect("valid hex digits");
		let parent_tx = Transaction::consensus_decode(&mut &parent_tx_bytes[..]).unwrap();
		let child_tx = Transaction::consensus_decode(&mut &child_tx_bytes[..]).unwrap();

		let mut txs = [child_tx.clone(), parent_tx.clone()];
		topologically_sort_single_child(&mut txs);
		assert_eq!(txs, [parent_tx.clone(), child_tx.clone()]);

		let mut txs = [parent_tx.clone(), child_tx.clone()];
		topologically_sort_single_child(&mut txs);
		assert_eq!(txs, [parent_tx.clone(), child_tx.clone()]);
	}

	#[test]
	#[should_panic]
	fn test_topologically_sort_child_panics_on_empty_package() {
		topologically_sort_single_child(&mut []);
	}

	#[test]
	#[should_panic]
	fn test_topologically_sort_child_panics_on_single_tx() {
		let parent_tx_hex = "030000000001014271edaf38b7fe0af0fa1d15d6ddf4784944f7168024ef5f765722a4f78d3d3c0100000000752c71800300000000000000000451024e731d25000000000000160014e16e0237f3ebd356e9afbcfe0cc54e4491bad90045370000000000002200200b05341083d40bc7c327b2d1e5b5a9a287d0735420890cdb2a4110a9b3d053e80400473044022060b4de1a7c6ed7c2af367e895e29e0d6815565b3356c5dcffdd65a5912c2403702207f518eecff0a4015bac6a2abf7c0c4249c21b0184232448366ca8c359042cdd601473044022018a29a31afebddbe77fa5474d441103ac116af18f91e7a9047cd228b34775dbf02205156da6637ca660ba202f7cd748bc26911e6159c99f7a4e050b51eb45c7c4e19014752210223519c7fe517d2b8f77687a9c74f3fa0e721f40aa9dc2073054ed23da9d90cf32102cf616337c3bc6d4831e47dbfb70e893747765c6d016b28730d05fb1e53e765de52ae5ea69120";
		let parent_tx_bytes = Vec::from_hex(parent_tx_hex).expect("valid hex digits");
		let parent_tx = Transaction::consensus_decode(&mut &parent_tx_bytes[..]).unwrap();

		let mut txs = [parent_tx];
		topologically_sort_single_child(&mut txs);
	}

	#[test]
	#[should_panic]
	fn test_topologically_sort_child_panics_on_unrelated_txs() {
		let tx_a_hex = "0200000000010798a1624a1a04d81e42fb1529a27006ed0d81a90276255c847284eba5ac4d58c80000000017160014407077a4cd193ec40ff2d00e64c2b4354740a159fdffffffb67095f480e9b5501d17e7ef4e243886dd4e3f3b79bae4c7e55d0238af8d4a5a0100000017160014dc976898abbb811452c1c527e571418238314f0cfdffffffdb9e455b98fd92754433537611a5c06c088d9ce29d110ea702f306775f4abe100100000017160014b7291a41b5b68e2d78b2b95fdffd1c2afca84458fdffffffa5e39a8b9df85e6d788e8260a13f76898e3a94f3694ce011ad339fcaad1447c301000000171600147a987c2e6cf3da949f05c795197017c9c3e8f878fdffffff7e394cfb304a8730f787a4876ff3a9dce5cd23358faace280214d20cee8d3b960100000017160014407077a4cd193ec40ff2d00e64c2b4354740a159fdffffffc009d7c9ec66cdf2d0ceb9179fd9ad66a020cae56d56d000a7650f906413623401000000171600143dfffc9317843a08956a3f89f24605fbf005c6b4fdffffff2ba249675058b2fe3d94c80b0257ec5078a89863f976c806dc75b4bb79da54ce0100000017160014b4314b07feeb551bd528f5ccc92416af28957a7ffdffffff0280a8120100000000160014d2394251f7537348e83bd07d1653da6c407a4b736ace1f000000000017a9142d8e1075e00f2b1e2a3ae9a615ede6a193d3c35b8702483045022100f313be4ce546e04fd78a7f5addbd4a45a50e1733974b3e0b4278a3ad4c0a51de0220043cff7a025a153428f2253cc9685260ba5602567ae7c3007a24692cdab7beea0121028d42936ac36f1ff6cfbe4a98ba86c34afe4a7ec6ad6fb82eb7185b310c6aab0e02483045022100b2430b7cb59a2fc1bbfd14e3f91cae3190568c5a3316f2f22cc98a7b7575712f022073d577f034386f5396a1c7eb846ef57ea66d3a0fcddc9e5bd2ba8e93e9e976090121036e8de18a55283d9c85c51694f72e9bb334a6b8549e7e8f10ae9a2464a4ea5ddf0247304402207a522d12005baa72e9647973ee34befab47dc0d1d18caba5137697359d562414022070f131c60b53239f6be2bca5678355b7cf0401a931488b031cf740c71962d92901210309ebb9fb8427ad0abed9f833ea848933139a35e0a860a6ad54c35c80f1df688f02483045022100e18c007b981827c175a395bbf832dd97ba6e6774b782096e314e9e2d17e4c58b022033bf65f30eb2e2ae95214c9275289112d0bc0a0f58501964aa6cc9ec3bd7588a012103ce49c953ea6ee23ed1c7eb77bd74b646b7da61833a8c4b2ea4a4684b986ee2a4024830450221008385732183ebe4cf99c41c1253205191961794d2dda71552e0d43f410c86f0f902203f1b3cdffeaa051d51b435357409e1c5bf95fea5fccc8e3d63ee08ccbae816000121028d42936ac36f1ff6cfbe4a98ba86c34afe4a7ec6ad6fb82eb7185b310c6aab0e0247304402204b440e67723294f7a6e0d3d9d4020e4ec3639cdcd4c39505af75d0c454f80a3e022030e1c507a00b0af227a3fa934de804e6d0ee9b4ded84e866170db76de8eca13a012103b9f70126efa51654dd95074db8ff970f32c2ab276a7eaacabd2535a88afa5369024730440220396fd46880ead0fb66b228b1897721ddf69f670533260c97b70b11d2361b4d5f022004ce30520ea7fb0922bac564827bac08483c45fb7c693dc7575905b6f15eb316012103ebe30c9ee3d2c4aca3f65db12c81fc6e86f5c7a4d3f7bffb0e21400b1cfc38db00000000";
		let tx_b_hex = "020000000001025f8aaea97f684591c63723d9b2b4b35f0b88bf8cb5e98b6a37166d02ff3f207a0100000017160014f2bd0b7ff5b6816827b0adb04ed73f206264c596fdffffff4771b033f26c10880a5797d2076f6a46462446dcc027f7a3b9a8544c4dcfc0350100000017160014b433bbb4c98063c616de6d876275c01f7b09f300fdffffff01206c070000000000160014d2394251f7537348e83bd07d1653da6c407a4b7302483045022100b8f36e051cc738ac5a6efd1447720c325f4a5d006836614a3db17b65d55cada502205b27ce25ee5cfe352a3a4a40675203c0a9c849200f4a77bef0f84f81e1426ad1012102a4180e4e6cd55fa8de52d0419743bd6a78bb17d6651c9aff2cbe3ad0400d1b9e02483045022100e6309ebb0c69bd1530ce5dd8cf6f9888fde3bda514dffdc62efb791c187fb13302207c4ad706045148804134997c6407962f8897d140ebf5ce7073e8a948ee64831701210294de779b11c0b9c0ffacbe92f2fccf293655e1fdf5687b5a064827922eef334b00000000";
		let tx_a_bytes = Vec::from_hex(tx_a_hex).expect("valid hex digits");
		let tx_b_bytes = Vec::from_hex(tx_b_hex).expect("valid hex digits");
		let tx_a = Transaction::consensus_decode(&mut &tx_a_bytes[..]).unwrap();
		let tx_b = Transaction::consensus_decode(&mut &tx_b_bytes[..]).unwrap();

		let mut txs = [tx_b.clone(), tx_a.clone()];
		topologically_sort_single_child(&mut txs);
	}
}

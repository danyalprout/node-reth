use crate::cache::Cache;
use alloy_primitives::{map::foldhash::HashMap, Bytes, U256};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use futures_util::StreamExt;
use reth::core::primitives::SignedTransaction;
use reth_optimism_primitives::{OpBlock, OpReceipt, OpTransactionSigned};
use rollup_boost::primitives::{ExecutionPayloadBaseV1, FlashblocksPayloadV1};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::error;
use url::Url;

use crate::metrics::Metrics;
use std::time::Instant;

#[derive(Debug, Deserialize, Serialize)]
struct FlashbotsMessage {
    method: String,
    params: serde_json::Value,
    #[serde(default)]
    id: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Metadata {
    pub receipts: HashMap<String, OpReceipt>,
    pub new_account_balances: HashMap<String, String>, // Address -> Balance (hex)
    pub block_number: u64,
}

// Simplify actor messages to just handle shutdown
#[derive(Debug)]
enum ActorMessage {
    BestPayload { payload: FlashblocksPayloadV1 },
}

pub struct FlashblocksClient {
    sender: mpsc::Sender<ActorMessage>,
    mailbox: mpsc::Receiver<ActorMessage>,
    cache: Arc<Cache>,
    metrics: Metrics,
}

impl FlashblocksClient {
    pub fn new(cache: Arc<Cache>) -> Self {
        let (sender, mailbox) = mpsc::channel(100);

        Self {
            sender,
            mailbox,
            cache,
            metrics: Metrics::default(),
        }
    }

    pub fn init(&mut self, ws_url: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = Url::parse(&ws_url)?;
        println!("trying to connect to {:?}", url);
        let sender = self.sender.clone();
        let cache_clone = self.cache.clone();

        // Take ownership of mailbox for the actor loop
        let mut mailbox = std::mem::replace(&mut self.mailbox, mpsc::channel(1).1);

        // Spawn WebSocket handler with integrated actor loop
        let metrics = self.metrics.clone(); // Clone here for the first spawn
        tokio::spawn(async move {
            let mut backoff = std::time::Duration::from_secs(1);
            const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

            loop {
                match connect_async(url.as_str()).await {
                    Ok((ws_stream, _)) => {
                        println!("WebSocket connected!");
                        let (_write, mut read) = ws_stream.split();
                        // Handle incoming messages
                        while let Some(msg) = read.next().await {
                            metrics.upstream_messages.increment(1);
                            let msg_start_time = Instant::now();

                            match msg {
                                Ok(Message::Binary(bytes)) => {
                                    // Decode binary message to string first
                                    let text = match String::from_utf8(bytes.to_vec()) {
                                        Ok(text) => text,
                                        Err(e) => {
                                            error!("Failed to decode binary message: {}", e);
                                            continue;
                                        }
                                    };

                                    // Then parse JSON
                                    let payload: FlashblocksPayloadV1 =
                                        match serde_json::from_str(&text) {
                                            Ok(m) => m,
                                            Err(e) => {
                                                error!("failed to parse message: {}", e);
                                                continue;
                                            }
                                        };

                                    let _ =
                                        sender.send(ActorMessage::BestPayload { payload }).await;
                                    metrics
                                        .websocket_processing_duration
                                        .record(msg_start_time.elapsed());
                                }
                                Ok(Message::Close(_)) => break,
                                Err(e) => {
                                    metrics.upstream_errors.increment(1);
                                    error!("Error receiving message: {}", e);
                                    break;
                                }
                                _ => {} // Handle other message types if needed
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            "WebSocket connection error, retrying in {:?}: {}",
                            backoff, e
                        );
                        tokio::time::sleep(backoff).await;
                        // Double the backoff time, but cap at MAX_BACKOFF
                        backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                        continue;
                    }
                }
            }
        });

        // Spawn actor's event loop
        tokio::spawn(async move {
            while let Some(message) = mailbox.recv().await {
                match message {
                    ActorMessage::BestPayload { payload } => {
                        process_payload(payload, cache_clone.clone());
                    }
                }
            }
        });

        Ok(())
    }
}

fn process_payload(payload: FlashblocksPayloadV1, cache: Arc<Cache>) {
    let metrics = Metrics::default();
    let msg_processing_start_time = Instant::now();

    // Convert metadata with error handling
    let metadata: Metadata = match serde_json::from_value(payload.metadata) {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to deserialize metadata: {}", e);
            return;
        }
    };

    let block_number = metadata.block_number;
    let diff = payload.diff;
    let withdrawals = diff.withdrawals.clone();
    let diff_transactions = diff.transactions.clone();

    // Skip if index is 0 and base is not cached, likely the first payload
    // Can't do pending block with this because already missing blocks
    if payload.index != 0
        && cache
            .get::<ExecutionPayloadBaseV1>(&format!("base:{:?}", block_number))
            .is_none()
    {
        return;
    }

    // Track flashblock indices and record metrics
    update_flashblocks_index(payload.index, &cache, &metrics);

    // Prevent updating to older blocks
    let current_block = cache.get::<OpBlock>("pending");
    if current_block.is_some() && current_block.unwrap().number > block_number {
        return;
    }

    // base only appears once in the first payload index
    let base = if let Some(base) = payload.base {
        if let Err(e) = cache.set(&format!("base:{:?}", block_number), &base, Some(10)) {
            error!("Failed to set base in cache: {}", e);
            return;
        }
        base
    } else {
        match cache.get(&format!("base:{:?}", block_number)) {
            Some(base) => base,
            None => {
                error!("Failed to get base from cache");
                return;
            }
        }
    };

    let transactions = match get_and_set_transactions(
        diff_transactions,
        payload.index,
        block_number,
        cache.clone(),
    ) {
        Ok(txs) => txs,
        Err(e) => {
            error!("Failed to get and set transactions: {}", e);
            return;
        }
    };

    let execution_payload: ExecutionPayloadV3 = ExecutionPayloadV3 {
        blob_gas_used: 0,
        excess_blob_gas: 0,
        payload_inner: ExecutionPayloadV2 {
            withdrawals,
            payload_inner: ExecutionPayloadV1 {
                parent_hash: base.parent_hash,
                fee_recipient: base.fee_recipient,
                state_root: diff.state_root,
                receipts_root: diff.receipts_root,
                logs_bloom: diff.logs_bloom,
                prev_randao: base.prev_randao,
                block_number: base.block_number,
                gas_limit: base.gas_limit,
                gas_used: diff.gas_used,
                timestamp: base.timestamp,
                extra_data: base.extra_data,
                base_fee_per_gas: U256::from(1000),
                block_hash: diff.block_hash,
                transactions,
            },
        },
    };

    let block: OpBlock = match execution_payload.try_into_block() {
        Ok(block) => block,
        Err(e) => {
            error!("Failed to convert execution payload to block: {}", e);
            return;
        }
    };

    // "pending" because users query the block using "pending" tag
    // This is an optimistic update will likely need to tweak in the future
    if let Err(e) = cache.set("pending", &block, Some(10)) {
        error!("Failed to set pending block in cache: {}", e);
        return;
    }

    // set block to block number as well
    if let Err(e) = cache.set(&format!("block:{:?}", block_number), &block, Some(10)) {
        error!("Failed to set block in cache: {}", e);
        return;
    }

    let diff_receipts = match get_and_set_txs_and_receipts(
        block.clone(),
        block_number,
        cache.clone(),
        metadata.clone(),
    ) {
        Ok(receipts) => receipts,
        Err(e) => {
            error!("Failed to get and set receipts: {}", e);
            return;
        }
    };

    // update all receipts
    let _receipts = match get_and_set_all_receipts(
        payload.index,
        block_number,
        cache.clone(),
        diff_receipts.clone(),
    ) {
        Ok(receipts) => receipts,
        Err(e) => {
            error!("Failed to get and set all receipts: {}", e);
            return;
        }
    };

    // Store account balances
    for (address, balance) in metadata.new_account_balances.iter() {
        if let Err(e) = cache.set(address, &balance, Some(10)) {
            error!("Failed to set account balance in cache: {}", e);
        }
    }

    metrics
        .block_processing_duration
        .record(msg_processing_start_time.elapsed());

    // check duration on the most heavy payload
    if payload.index == 0 {
        println!(
            "block processing time: {:?}",
            msg_processing_start_time.elapsed()
        );
    }
}

fn update_flashblocks_index(index: u64, cache: &Arc<Cache>, metrics: &Metrics) {
    if index == 0 {
        // Get highest index from previous block
        if let Some(prev_highest_index) = cache.get::<u64>("highest_payload_index") {
            // Record metric: total flash blocks = highest_index + 1 (since it's 0-indexed)
            metrics
                .flashblocks_in_block
                .record((prev_highest_index + 1) as f64);
            println!("Previous block had {} flash blocks", prev_highest_index + 1);
        }

        // Reset highest index to 0 for new block
        if let Err(e) = cache.set("highest_payload_index", &0u64, Some(10)) {
            error!("Failed to reset highest flash index: {}", e);
        }
    } else {
        // Update highest index if current index is higher
        let current_highest = cache.get::<u64>("highest_payload_index").unwrap_or(0);
        if index > current_highest {
            if let Err(e) = cache.set("highest_payload_index", &index, Some(10)) {
                error!("Failed to update highest flash index: {}", e);
            }
        }
    }
}

fn get_and_set_transactions(
    transactions: Vec<Bytes>,
    payload_index: u64,
    block_number: u64,
    cache: Arc<Cache>,
) -> Result<Vec<Bytes>, Box<dyn std::error::Error>> {
    // update incremental transactions
    let transactions = if payload_index == 0 {
        transactions
    } else {
        let existing =
            match cache.get::<Vec<Bytes>>(&format!("diff:transactions:{:?}", block_number)) {
                Some(existing) => existing,
                None => return Err("Failed to get pending transactions from cache".into()),
            };
        existing
            .into_iter()
            .chain(transactions.iter().cloned())
            .collect()
    };

    cache.set(
        &format!("diff:transactions:{:?}", block_number),
        &transactions,
        Some(10),
    )?;

    Ok(transactions)
}

fn get_and_set_txs_and_receipts(
    block: OpBlock,
    block_number: u64,
    cache: Arc<Cache>,
    metadata: Metadata,
) -> Result<Vec<OpReceipt>, Box<dyn std::error::Error>> {
    let mut diff_receipts: Vec<OpReceipt> = vec![];
    let mut tx_hashes: Vec<String> = vec![];

    if let Some(existing_hashes) = cache.get::<Vec<String>>(&format!("tx_hashes:{}", block_number))
    {
        tx_hashes = existing_hashes;
    }

    for (idx, transaction) in block.body.transactions.iter().enumerate() {
        let tx_hash = transaction.tx_hash().to_string();

        // Add transaction hash to the ordered list if not already present
        if !tx_hashes.contains(&tx_hash) {
            tx_hashes.push(tx_hash.clone());
        }

        let existing_tx = cache.get::<OpTransactionSigned>(&tx_hash);
        if existing_tx.is_none() {
            if let Err(e) = cache.set(&tx_hash, &transaction, Some(10)) {
                error!("Failed to set transaction in cache: {}", e);
                continue;
            }
            if let Err(e) = cache.set(&format!("tx_idx:{}", transaction.tx_hash()), &idx, Some(10))
            {
                error!("Failed to set transaction index in cache: {}", e);
                continue;
            }

            if let Ok(from) = transaction.recover_signer() {
                let current_count = cache
                    .get::<u64>(&format!("tx_count:{}:{}", from, block_number))
                    .unwrap_or(0);
                if let Err(e) = cache.set(
                    &format!("tx_count:{}:{}", from, block_number),
                    &(current_count + 1),
                    Some(10),
                ) {
                    error!("Failed to set transaction count in cache: {}", e);
                }

                if let Err(e) = cache.set(
                    &format!("tx_sender:{}", transaction.tx_hash()),
                    &from,
                    Some(10),
                ) {
                    error!("Failed to set transaction sender in cache: {}", e);
                }

                if let Err(e) = cache.set(
                    &format!("tx_block_number:{}", transaction.tx_hash()),
                    &block_number,
                    Some(10),
                ) {
                    error!("Failed to set transaction sender in cache: {}", e);
                }
            }
        }

        // TODO: move this into the transaction check
        if metadata.receipts.contains_key(&tx_hash) {
            // find receipt in metadata and set it in cache
            let receipt = metadata.receipts.get(&tx_hash).unwrap();
            if let Err(e) = cache.set(&format!("receipt:{:?}", tx_hash), receipt, Some(10)) {
                error!("Failed to set receipt in cache: {}", e);
                continue;
            }
            // map receipt's block number as well
            if let Err(e) = cache.set(
                &format!("receipt_block:{:?}", tx_hash),
                &block_number,
                Some(10),
            ) {
                error!("Failed to set receipt block in cache: {}", e);
                continue;
            }

            diff_receipts.push(receipt.clone());
        }
    }

    if let Err(e) = cache.set(&format!("tx_hashes:{}", block_number), &tx_hashes, Some(10)) {
        error!("Failed to update transaction hashes list in cache: {}", e);
    }

    Ok(diff_receipts)
}

fn get_and_set_all_receipts(
    payload_index: u64,
    block_number: u64,
    cache: Arc<Cache>,
    diff_receipts: Vec<OpReceipt>,
) -> Result<Vec<OpReceipt>, Box<dyn std::error::Error>> {
    // update all receipts
    let receipts = if payload_index == 0 {
        // get receipts and sort by cumulative gas used
        diff_receipts
    } else {
        let existing =
            match cache.get::<Vec<OpReceipt>>(&format!("pending_receipts:{:?}", block_number)) {
                Some(existing) => existing,
                None => {
                    return Err("Failed to get pending receipts from cache".into());
                }
            };
        existing
            .into_iter()
            .chain(diff_receipts.iter().cloned())
            .collect()
    };

    cache.set(
        &format!("pending_receipts:{:?}", block_number),
        &receipts,
        Some(10),
    )?;

    Ok(receipts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Receipt, TxReceipt};
    use alloy_primitives::{Address, B256};
    use alloy_rpc_types_engine::PayloadId;
    use rollup_boost::primitives::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
    use std::str::FromStr;

    fn create_first_payload() -> FlashblocksPayloadV1 {
        // First payload (index 0) setup remains the same
        let base = ExecutionPayloadBaseV1 {
            parent_hash: Default::default(),
            parent_beacon_block_root: Default::default(),
            fee_recipient: Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            block_number: 1,
            gas_limit: 1000000,
            timestamp: 1234567890,
            prev_randao: Default::default(),
            extra_data: Default::default(),
            base_fee_per_gas: U256::from(1000),
        };

        let delta = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![],
            withdrawals: vec![],
            state_root: Default::default(),
            receipts_root: Default::default(),
            logs_bloom: Default::default(),
            gas_used: 0,
            block_hash: Default::default(),
        };

        let metadata = Metadata {
            block_number: 1,
            receipts: HashMap::default(),
            new_account_balances: HashMap::default(),
        };

        FlashblocksPayloadV1 {
            index: 0,
            payload_id: PayloadId::new([0; 8]),
            base: Some(base),
            diff: delta,
            metadata: serde_json::to_value(metadata).unwrap(),
        }
    }

    // Create payload with specific index and block number
    fn create_payload_with_index(index: u64, block_number: u64) -> FlashblocksPayloadV1 {
        let base = if index == 0 {
            Some(ExecutionPayloadBaseV1 {
                parent_hash: Default::default(),
                parent_beacon_block_root: Default::default(),
                fee_recipient: Address::from_str("0x1234567890123456789012345678901234567890")
                    .unwrap(),
                block_number,
                gas_limit: 1000000,
                timestamp: 1234567890,
                prev_randao: Default::default(),
                extra_data: Default::default(),
                base_fee_per_gas: U256::from(1000),
            })
        } else {
            None
        };

        let delta = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![],
            withdrawals: vec![],
            state_root: B256::repeat_byte(index as u8),
            receipts_root: B256::repeat_byte((index + 1) as u8),
            logs_bloom: Default::default(),
            gas_used: 21000 * index,
            block_hash: B256::repeat_byte((index + 2) as u8),
        };

        let metadata = Metadata {
            block_number,
            receipts: HashMap::default(),
            new_account_balances: HashMap::default(),
        };

        FlashblocksPayloadV1 {
            index,
            payload_id: PayloadId::new([0; 8]),
            base,
            diff: delta,
            metadata: serde_json::to_value(metadata).unwrap(),
        }
    }

    fn create_second_payload() -> FlashblocksPayloadV1 {
        // Create second payload (index 1) with transactions
        // tx1 hash: 0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c
        // tx2 hash: 0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8
        let tx1 = Bytes::from_str("0x02f87483014a3482017e8459682f0084596830a98301f1d094b01866f195533de16eb929b73f87280693ca0cb480844e71d92dc001a0a658c18bdba29dd4022ee6640fdd143691230c12b3c8c86cf5c1a1f1682cc1e2a0248a28763541ebed2b87ecea63a7024b5c2b7de58539fa64c887b08f5faf29c1").unwrap();
        let tx2 = Bytes::from_str("0xf8cd82016d8316e5708302c01c94f39635f2adf40608255779ff742afe13de31f57780b8646e530e9700000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000001bc16d674ec8000000000000000000000000000000000000000000000000000156ddc81eed2a36d68302948ba0a608703e79b22164f74523d188a11f81c25a65dd59535bab1cd1d8b30d115f3ea07f4cfbbad77a139c9209d3bded89091867ff6b548dd714109c61d1f8e7a84d14").unwrap();

        let delta2 = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![tx1.clone(), tx2.clone()],
            withdrawals: vec![],
            state_root: B256::repeat_byte(0x1),
            receipts_root: B256::repeat_byte(0x2),
            logs_bloom: Default::default(),
            gas_used: 21000,
            block_hash: B256::repeat_byte(0x3),
        };

        let metadata2 = Metadata {
            block_number: 1,
            receipts: {
                let mut receipts = HashMap::default();
                receipts.insert(
                    "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c"
                        .to_string(), // transaction hash as string
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 21000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8"
                        .to_string(), // transaction hash as string
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 42000,
                        logs: vec![],
                    }),
                );
                receipts
            },
            new_account_balances: {
                let mut map = HashMap::default();
                map.insert(
                    "0x1234567890123456789012345678901234567890".to_string(),
                    "0x1234".to_string(),
                );
                map
            },
        };

        FlashblocksPayloadV1 {
            index: 1,
            payload_id: PayloadId::new([0; 8]),
            base: None,
            diff: delta2,
            metadata: serde_json::to_value(metadata2.clone()).unwrap(),
        }
    }

    #[test]
    fn test_process_payload() {
        let cache = Arc::new(Cache::default());

        let payload = create_first_payload();

        // Process first payload
        process_payload(payload, cache.clone());

        let payload2 = create_second_payload();
        // Process second payload
        process_payload(payload2, cache.clone());

        // Verify final state
        let final_block = cache.get::<OpBlock>("pending").unwrap();
        assert_eq!(final_block.body.transactions.len(), 2);
        assert_eq!(final_block.header.state_root, B256::repeat_byte(0x1));
        assert_eq!(final_block.header.receipts_root, B256::repeat_byte(0x2));
        assert_eq!(final_block.header.gas_used, 21000);

        // Verify account balance was updated
        let balance = cache
            .get::<String>("0x1234567890123456789012345678901234567890")
            .unwrap();
        assert_eq!(balance, "0x1234");

        let tx1_receipt = cache
            .get::<OpReceipt>(&format!(
                "receipt:{:?}",
                "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c"
            ))
            .unwrap();
        assert_eq!(tx1_receipt.cumulative_gas_used(), 21000);

        let tx2_receipt = cache
            .get::<OpReceipt>(&format!(
                "receipt:{:?}",
                "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8"
            ))
            .unwrap();
        assert_eq!(tx2_receipt.cumulative_gas_used(), 42000);

        // verify tx_sender, tx_block_number, tx_idx
        let tx_sender = cache
            .get::<Address>(&format!(
                "tx_sender:{}",
                "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c"
            ))
            .unwrap();
        assert_eq!(
            tx_sender,
            Address::from_str("0xb63d5fd2e6c53fe06680c47736aba771211105e4").unwrap()
        );

        let tx_block_number = cache
            .get::<u64>(&format!(
                "tx_block_number:{}",
                "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c"
            ))
            .unwrap();
        assert_eq!(tx_block_number, 1);

        let tx_idx = cache
            .get::<u64>(&format!(
                "tx_idx:{}",
                "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c"
            ))
            .unwrap();
        assert_eq!(tx_idx, 0);

        let tx_sender2 = cache
            .get::<Address>(&format!(
                "tx_sender:{}",
                "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8"
            ))
            .unwrap();
        assert_eq!(
            tx_sender2,
            Address::from_str("0x6e5e56b972374e4fde8390df0033397df931a49d").unwrap()
        );

        let tx_block_number2 = cache
            .get::<u64>(&format!(
                "tx_block_number:{}",
                "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8"
            ))
            .unwrap();
        assert_eq!(tx_block_number2, 1);

        let tx_idx2 = cache
            .get::<u64>(&format!(
                "tx_idx:{}",
                "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8"
            ))
            .unwrap();
        assert_eq!(tx_idx2, 1);
    }

    #[test]
    fn test_skip_initial_non_zero_index_payload() {
        let cache = Arc::new(Cache::default());

        let metadata = Metadata {
            block_number: 1,
            receipts: HashMap::default(),
            new_account_balances: HashMap::default(),
        };

        let payload = FlashblocksPayloadV1 {
            payload_id: PayloadId::new([0; 8]),
            index: 1, // Non-zero index but no base in cache
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: serde_json::to_value(metadata).unwrap(),
        };

        // Process payload
        process_payload(payload, cache.clone());

        // Verify no block was stored, since it skips the first payload
        assert!(cache.get::<OpBlock>("pending").is_none());
    }

    #[test]
    fn test_flash_block_tracking() {
        // Create cache
        let cache = Arc::new(Cache::default());

        // Process first block with 3 flash blocks
        // Block 1, payload 0 (starts a new block)
        let payload1_0 = create_payload_with_index(0, 1);
        process_payload(payload1_0, cache.clone());

        // Check that highest_payload_index was set to 0
        let highest = cache.get::<u64>("highest_payload_index").unwrap();
        assert_eq!(highest, 0);

        // Block 1, payload 1
        let payload1_1 = create_payload_with_index(1, 1);
        process_payload(payload1_1, cache.clone());

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>("highest_payload_index").unwrap();
        assert_eq!(highest, 1);

        // Block 1, payload 2
        let payload1_2 = create_payload_with_index(2, 1);
        process_payload(payload1_2, cache.clone());

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>("highest_payload_index").unwrap();
        assert_eq!(highest, 2);

        // Now start a new block (block 2, payload 0)
        let payload2_0 = create_payload_with_index(0, 2);
        process_payload(payload2_0, cache.clone());

        // Check that highest_payload_index was reset to 0
        let highest = cache.get::<u64>("highest_payload_index").unwrap();
        assert_eq!(highest, 0);

        // Block 2, payload 1 (out of order with payload 3)
        let payload2_1 = create_payload_with_index(1, 2);
        process_payload(payload2_1, cache.clone());

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>("highest_payload_index").unwrap();
        assert_eq!(highest, 1);

        // Block 2, payload 3 (skipping 2)
        let payload2_3 = create_payload_with_index(3, 2);
        process_payload(payload2_3, cache.clone());

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>("highest_payload_index").unwrap();
        assert_eq!(highest, 3);

        // Block 2, payload 2 (out of order, should not change highest)
        let payload2_2 = create_payload_with_index(2, 2);
        process_payload(payload2_2, cache.clone());

        // Check that highest_payload_index is still 3
        let highest = cache.get::<u64>("highest_payload_index").unwrap();
        assert_eq!(highest, 3);

        // Start block 3, payload 0
        let payload3_0 = create_payload_with_index(0, 3);
        process_payload(payload3_0, cache.clone());

        // Check that highest_payload_index was reset to 0
        // Also verify metric would have been recorded (though we can't directly check the metric's value)
        let highest = cache.get::<u64>("highest_payload_index").unwrap();
        assert_eq!(highest, 0);
    }

    #[test]
    fn test_tx_hash_list_storage_and_deduplication() {
        let cache = Arc::new(Cache::default());
        let block_number = 1;

        let base = ExecutionPayloadBaseV1 {
            parent_hash: Default::default(),
            parent_beacon_block_root: Default::default(),
            fee_recipient: Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            block_number,
            gas_limit: 1000000,
            timestamp: 1234567890,
            prev_randao: Default::default(),
            extra_data: Default::default(),
            base_fee_per_gas: U256::from(1000),
        };

        let tx1 = Bytes::from_str("0x02f87483014a3482017e8459682f0084596830a98301f1d094b01866f195533de16eb929b73f87280693ca0cb480844e71d92dc001a0a658c18bdba29dd4022ee6640fdd143691230c12b3c8c86cf5c1a1f1682cc1e2a0248a28763541ebed2b87ecea63a7024b5c2b7de58539fa64c887b08f5faf29c1").unwrap();

        let delta1 = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![tx1.clone()],
            withdrawals: vec![],
            state_root: Default::default(),
            receipts_root: Default::default(),
            logs_bloom: Default::default(),
            gas_used: 21000,
            block_hash: Default::default(),
        };

        let tx1_hash =
            "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c".to_string();

        let metadata1 = Metadata {
            block_number,
            receipts: {
                let mut receipts = HashMap::default();
                receipts.insert(
                    tx1_hash.clone(),
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 21000,
                        logs: vec![],
                    }),
                );
                receipts
            },
            new_account_balances: HashMap::default(),
        };

        let payload1 = FlashblocksPayloadV1 {
            index: 0,
            payload_id: PayloadId::new([0; 8]),
            base: Some(base),
            diff: delta1,
            metadata: serde_json::to_value(metadata1).unwrap(),
        };

        process_payload(payload1, cache.clone());

        let tx_hashes1 = cache
            .get::<Vec<String>>(&format!("tx_hashes:{}", block_number))
            .unwrap();
        assert_eq!(tx_hashes1.len(), 1);
        assert_eq!(tx_hashes1[0], tx1_hash);

        let tx2 = Bytes::from_str("0xf8cd82016d8316e5708302c01c94f39635f2adf40608255779ff742afe13de31f57780b8646e530e9700000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000001bc16d674ec8000000000000000000000000000000000000000000000000000156ddc81eed2a36d68302948ba0a608703e79b22164f74523d188a11f81c25a65dd59535bab1cd1d8b30d115f3ea07f4cfbbad77a139c9209d3bded89091867ff6b548dd714109c61d1f8e7a84d14").unwrap();

        let tx2_hash =
            "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8".to_string();

        let delta2 = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![tx1.clone(), tx2.clone()], // Note tx1 is repeated
            withdrawals: vec![],
            state_root: B256::repeat_byte(0x1),
            receipts_root: B256::repeat_byte(0x2),
            logs_bloom: Default::default(),
            gas_used: 42000,
            block_hash: B256::repeat_byte(0x3),
        };

        let metadata2 = Metadata {
            block_number,
            receipts: {
                let mut receipts = HashMap::default();
                receipts.insert(
                    tx1_hash.clone(),
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 21000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    tx2_hash.clone(),
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 42000,
                        logs: vec![],
                    }),
                );
                receipts
            },
            new_account_balances: HashMap::default(),
        };

        let payload2 = FlashblocksPayloadV1 {
            index: 1,
            payload_id: PayloadId::new([0; 8]),
            base: None,
            diff: delta2,
            metadata: serde_json::to_value(metadata2.clone()).unwrap(),
        };

        process_payload(payload2, cache.clone());

        let tx_hashes2 = cache
            .get::<Vec<String>>(&format!("tx_hashes:{}", block_number))
            .unwrap();
        assert_eq!(
            tx_hashes2.len(),
            2,
            "Should have 2 unique transaction hashes"
        );
        assert_eq!(tx_hashes2[0], tx1_hash, "First hash should be tx1");
        assert_eq!(tx_hashes2[1], tx2_hash, "Second hash should be tx2");

        let delta3 = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![tx2.clone(), tx1.clone()], // Different order
            withdrawals: vec![],
            state_root: B256::repeat_byte(0x1),
            receipts_root: B256::repeat_byte(0x2),
            logs_bloom: Default::default(),
            gas_used: 42000,
            block_hash: B256::repeat_byte(0x3),
        };

        let payload3 = FlashblocksPayloadV1 {
            index: 2,
            payload_id: PayloadId::new([0; 8]),
            base: None,
            diff: delta3,
            metadata: serde_json::to_value(metadata2).unwrap(), // Same metadata
        };

        process_payload(payload3, cache.clone());

        let tx_hashes3 = cache
            .get::<Vec<String>>(&format!("tx_hashes:{}", block_number))
            .unwrap();
        assert_eq!(
            tx_hashes3.len(),
            2,
            "Should still have 2 unique transaction hashes"
        );
        assert_eq!(tx_hashes3[0], tx1_hash, "First hash should be tx1");
        assert_eq!(tx_hashes3[1], tx2_hash, "Second hash should be tx2");
    }
}

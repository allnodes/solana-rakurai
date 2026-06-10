use {
    crate::{
        banking_stage::{
            PostPackConfirmationSignatures,
            scheduler_messages::MaxAge,
            transaction_scheduler::{
                receive_and_buffer::PacketHandlingError,
                transaction_state_container::{
                    RuntimeTransactionView, StateContainer, TransactionViewStateContainer,
                },
            },
        },
        bundle_stage::bundle_packet_deserializer::BundlePacketDeserializer,
        packet_bundle::VerifiedPacketBundle,
        proxy::block_engine_stage::BlockEngineConfig,
    },
    ahash::HashSet,
    arc_swap::ArcSwap,
    arrayvec::ArrayVec,
    crossbeam_channel::Sender,
    log::info,
    min_max_heap::MinMaxHeap,
    solana_address::Address,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::transaction_meta::TransactionMeta,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_signature::Signature,
    std::{
        collections::{HashMap, VecDeque},
        str::FromStr,
        sync::{Arc, RwLock},
    },
};

#[derive(Debug, PartialEq, Eq)]
pub enum BundleStorageError {
    EmptyBatch,
    ContainerFull,
    PacketMarkedDiscard(usize),
    PacketFilterError((PacketHandlingError, usize /* packet index */)),
    BundleTooLarge,
    DuplicateTransaction,
    DuplicateNonce,
    ZeroTipAmount(String),
}

struct BundleTransactionId {
    container_ids: Vec<(usize, u64, u64)>,
    bundle_priority: u64,
}

impl Ord for BundleTransactionId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.bundle_priority.cmp(&other.bundle_priority)
    }
}

impl PartialOrd for BundleTransactionId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for BundleTransactionId {
    fn eq(&self, other: &Self) -> bool {
        self.bundle_priority == other.bundle_priority
    }
}

impl Eq for BundleTransactionId {}

const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

#[allow(dead_code)]
pub fn jito_tip_accounts_map() -> HashMap<Pubkey, f64> {
    JITO_TIP_ACCOUNTS
        .iter()
        .filter_map(|account| Pubkey::from_str(account).ok().map(|pubkey| (pubkey, 1.0)))
        .collect()
}

#[cfg(feature = "build_validator")]
unsafe extern "C" {
    #[allow(improper_ctypes)]
    #[allow(unused)]
    fn fetch_bundle_tip(transaction: &RuntimeTransactionView) -> u64;
}

pub struct BundleStorageEntry {
    pub container_ids: Vec<(usize, u64 /*priority*/, u64 /*cost */)>,
    pub bundle_priority: u64,
    pub transactions: Vec<RuntimeTransactionView>,
    pub max_ages: Vec<MaxAge>,
}

/// Bundle storage has two deques: one for unprocessed bundles and another for ones that exceeded
/// the cost model and need to get retried next slot.
pub struct BundleStorage {
    last_slot: Slot,
    transaction_capacity: usize,
    transaction_view_state_container: TransactionViewStateContainer,
    unprocessed_bundles: MinMaxHeap<BundleTransactionId>,
    // Storage for bundles that exceeded the cost model for the slot they were last attempted
    // execution on
    cost_model_buffered_bundles: VecDeque<BundleTransactionId>,
}

impl BundleStorage {
    fn refresh_slot_boundary(&mut self, slot: Slot) {
        if slot != self.last_slot {
            // the cost_model_buffered_bundles has the oldest bundles at the front of the queue
            // we need to pop from the back of that queue and insert to the front of the unprocessed_bundles queue so by the time we reach the front,
            // the oldest bundle is at the front of the unprocessed_bundles queue
            while let Some(bundle) = self.cost_model_buffered_bundles.pop_back() {
                self.unprocessed_bundles.push(bundle);
            }

            self.last_slot = slot;
        }
    }

    pub fn peek_bundle_priority(&mut self, slot: Slot) -> Option<u64> {
        self.refresh_slot_boundary(slot);
        self.unprocessed_bundles
            .peek_max()
            .map(|bundle| bundle.bundle_priority)
    }

    const MAX_PACKETS_PER_BUNDLE: usize = 5;

    #[allow(unused)]
    pub fn with_capacity(transaction_capacity: usize) -> Self {
        Self {
            last_slot: Slot::default(),
            transaction_capacity,
            transaction_view_state_container: TransactionViewStateContainer::with_capacity(
                transaction_capacity,
                true,
            ),
            unprocessed_bundles: MinMaxHeap::with_capacity(transaction_capacity),
            cost_model_buffered_bundles: VecDeque::with_capacity(transaction_capacity),
        }
    }

    pub fn unprocessed_bundles_len(&self) -> usize {
        self.unprocessed_bundles.len()
    }

    pub fn cost_model_buffered_bundles_len(&self) -> usize {
        self.cost_model_buffered_bundles.len()
    }

    pub fn num_packets_buffered(&self) -> usize {
        self.transaction_view_state_container.buffer_size()
    }

    /// Retries a bundle by inserting the transactions back into the transaction_view_state_container.
    /// The bundle is then pushed back to the cost_model_buffered_bundles queue.
    pub fn retry_bundle(&mut self, bundle: BundleStorageEntry) {
        for ((container_id, _, _), transaction) in bundle
            .container_ids
            .iter()
            .zip(bundle.transactions.into_iter())
        {
            self.transaction_view_state_container
                .get_mut_transaction_state(*container_id)
                .unwrap()
                .retry_transaction(transaction);
        }
        self.cost_model_buffered_bundles
            .push_back(BundleTransactionId {
                container_ids: bundle.container_ids,
                bundle_priority: bundle.bundle_priority,
            });
    }

    /// Destroys a bundle by removing the transactions from the transaction_view_state_container.
    /// It's important that transactions in the BundleStorageEntry are not used after this call
    /// as it will lead to panic inside the TransactionViewStateContainer.
    pub fn destroy_bundle(&mut self, bundle: BundleStorageEntry) {
        for (container_id, _, _) in bundle.container_ids.into_iter() {
            self.transaction_view_state_container
                .remove_by_id(container_id);
        }
    }

    /// Pops a bundle from the unprocessed_bundles queue and returns it as a BundleStorageEntry.
    /// Returns None if there are no bundles to pop.
    pub fn pop_bundle(&mut self, slot: Slot) -> Option<BundleStorageEntry> {
        self.refresh_slot_boundary(slot);

        // only want to pop from the unprocessed bundles queue and wait for slot boundary to refresh from cost_model_buffered_bundles
        let bundle = self.unprocessed_bundles.pop_max()?;

        let (bundle_transactions, bundle_max_ages): (Vec<RuntimeTransactionView>, Vec<MaxAge>) =
            bundle
                .container_ids
                .iter()
                .map(|(id, _, _)| {
                    self.transaction_view_state_container
                        .get_mut_transaction_state(*id)
                        .unwrap()
                        .take_transaction_for_scheduling()
                })
                .collect();

        Some(BundleStorageEntry {
            container_ids: bundle.container_ids,
            bundle_priority: bundle.bundle_priority,
            transactions: bundle_transactions,
            max_ages: bundle_max_ages,
        })
    }

    /// Preconf hashes are sent in proto `meta.addr` as `hash.to_string()` and stored in
    /// `packet.meta.remote_pubkey` in `proto_packet_to_packet`.
    fn postpackconf_hash_matches_packet_remote_pubkey(
        postpackconf_hash: &solana_blake3_hasher::Hash,
        remote_pubkey: &Pubkey,
    ) -> bool {
        let matches = postpackconf_hash.as_ref() == remote_pubkey.as_ref();
        info!(
            "postpackconf hash comparison: postpackconf_hash={postpackconf_hash}, packet_remote_pubkey={remote_pubkey}, matches={matches}"
        );
        matches
    }

    /// For secondary block engine bundles whose lead transaction was already published via
    /// scheduler updates, strip the first transaction when it succeeded on-chain (backrun).
    fn maybe_strip_secondary_backrun_lead_transaction(
        &mut self,
        block_engine_url: &str,
        primary_block_engine_url: &str,
        scheduler_postpack_conf_signatures: &PostPackConfirmationSignatures,
        working_bank: &Bank,
        first_packet_remote_pubkey: Option<Pubkey>,
        container_ids: &mut Vec<(usize, u64, u64)>,
    ) -> bool {
        if block_engine_url == primary_block_engine_url || container_ids.len() < 2 {
            return false;
        }

        let first_container_id = container_ids[0].0;
        let Some(first_signature) = self
            .transaction_view_state_container
            .get_transaction(first_container_id)
            .and_then(|tx| tx.signatures().first().copied())
        else {
            return false;
        };

        {
            let Ok(seen) = scheduler_postpack_conf_signatures.read() else {
                return false;
            };
            let Some(postpackconf_hash) = seen.get(&first_signature) else {
                return false;
            };
            let Some(packet_remote_pubkey) = first_packet_remote_pubkey else {
                return false;
            };
            if !Self::postpackconf_hash_matches_packet_remote_pubkey(
                postpackconf_hash,
                &packet_remote_pubkey,
            ) {
                return false;
            }
        }

        if working_bank
            .get_signature_status(&first_signature)
            .is_some_and(|status| status.is_err())
        {
            return true;
        }

        let (removed_id, _, _) = container_ids.remove(0);
        self.transaction_view_state_container
            .remove_by_id(removed_id);

        info!(
            "stripped lead transaction {first_signature} from secondary block engine bundle \
             ({block_engine_url}); remaining txns: {}",
            container_ids.len()
        );

        true
    }

    pub fn insert_bundle(
        &mut self,
        bundle: VerifiedPacketBundle,
        root_bank: &Bank,
        working_bank: &Bank,
        blacklisted_accounts: &HashSet<Pubkey>,
        nonce_packets: &Arc<RwLock<HashMap<(Address, Hash), (Signature, u64)>>>,
        nonce_packet_sender: &Sender<Signature>,
        block_engine_config: &ArcSwap<BlockEngineConfig>,
        scheduler_postpack_conf_signatures: &PostPackConfirmationSignatures,
    ) -> Result<(), BundleStorageError> {
        let block_engine_url = bundle.block_engine_url().to_string();
        let batch = bundle.take();

        // Packet checks
        if batch.is_empty() {
            return Err(BundleStorageError::EmptyBatch);
        }
        if batch.len() > Self::MAX_PACKETS_PER_BUNDLE {
            return Err(BundleStorageError::BundleTooLarge);
        }
        if let Some(idx) = batch
            .iter()
            .enumerate()
            .find_map(|(idx, packet)| packet.meta().discard().then_some(idx))
        {
            return Err(BundleStorageError::PacketMarkedDiscard(idx));
        }

        // Container checks
        if self
            .transaction_view_state_container
            .buffer_size()
            .saturating_add(batch.len())
            > self.transaction_capacity
        {
            return Err(BundleStorageError::ContainerFull);
        }

        let first_packet_remote_pubkey = batch.get(0).unwrap().meta().remote_pubkey;

        let mut container_ids: Vec<(usize, u64, u64)> = Vec::with_capacity(batch.len());
        let mut maybe_error = Ok(());
        let enable_instruction_accounts_limit = working_bank
            .feature_set
            .is_active(&agave_feature_set::limit_instruction_accounts::id());
        let transaction_account_lock_limit = working_bank.get_transaction_account_lock_limit();

        let mut total_bundle_tip_amount = 0;
        let mut total_bundle_reward = 0;
        let mut nonce = None;
        let mut blockhash = None;

        for (idx, packet) in batch.iter().enumerate() {
            // bundles shall contain all valid packets; checked above
            let packet_data = packet.data(..).unwrap();

            let mut tx_reward = 0u64;
            // try to insert the packet into the container
            if let Some(container_id) = self
                .transaction_view_state_container
                .try_insert_map_only_with_data(packet_data, |bytes| {
                    match BundlePacketDeserializer::try_handle_packet(
                        bytes,
                        root_bank,
                        working_bank,
                        enable_instruction_accounts_limit,
                        transaction_account_lock_limit,
                        blacklisted_accounts,
                    ) {
                        Ok((state, reward_fee)) => {
                            tx_reward = reward_fee;
                            Ok(state)
                        }
                        Err(e) => {
                            maybe_error = Err(e);
                            Err(())
                        }
                    }
                })
            {
                let transaction_state = self
                    .transaction_view_state_container
                    .get_mut_transaction_state(container_id)
                    .unwrap();
                let tip_amount;
                #[cfg(feature = "build_validator")]
                unsafe {
                    tip_amount = fetch_bundle_tip(transaction_state.transaction())
                };
                #[cfg(not(feature = "build_validator"))]
                {
                    tip_amount = 0;
                }

                let cus = transaction_state.cost();
                total_bundle_tip_amount += tip_amount;

                total_bundle_reward += tip_amount + tx_reward;
                if nonce.is_none() {
                    nonce = transaction_state
                        .transaction()
                        .as_sanitized_transaction()
                        .get_durable_nonce()
                        .copied();
                    if let Some(_nonce) = nonce {
                        blockhash =
                            Some(transaction_state.transaction().recent_blockhash().clone());
                    }
                }

                container_ids.push((container_id, tx_reward as u64, cus as u64));
            } else {
                // any error shall rollback any transactions added to the container
                for (container_id, _, _) in container_ids.iter() {
                    self.transaction_view_state_container
                        .remove_by_id(*container_id);
                }
                return Err(BundleStorageError::PacketFilterError((
                    maybe_error.unwrap_err(),
                    idx,
                )));
            }
        }

        let is_duplicate_hashes = self.does_contain_duplicate_hashes(
            &container_ids
                .iter()
                .map(|(id, _, _)| *id)
                .collect::<Vec<usize>>(),
        );
        if is_duplicate_hashes {
            for (container_id, _, _) in container_ids.iter() {
                self.transaction_view_state_container
                    .remove_by_id(*container_id);
            }
            return Err(BundleStorageError::DuplicateTransaction);
        }

        if total_bundle_tip_amount == 0 {
            for (container_id, _, _) in container_ids.iter() {
                self.transaction_view_state_container
                    .remove_by_id(*container_id);
            }
            return Err(BundleStorageError::ZeroTipAmount(block_engine_url));
        }

        let primary_block_engine_url = block_engine_config.load().block_engine_url.clone();
        let postpackconf_hash_matched = self.maybe_strip_secondary_backrun_lead_transaction(
            &block_engine_url,
            &primary_block_engine_url,
            scheduler_postpack_conf_signatures,
            working_bank,
            first_packet_remote_pubkey,
            &mut container_ids,
        );

        let mut total_rewards = 0;
        let mut total_cus = 0;

        for (_, reward, cus) in container_ids.iter() {
            total_rewards += reward;
            total_cus += cus;
        }

        if postpackconf_hash_matched {
            total_bundle_tip_amount = total_bundle_tip_amount
                .saturating_mul(120)
                .saturating_div(100);
        }
        total_rewards = total_rewards + total_bundle_tip_amount;

        let bundle_priority = total_rewards
            .saturating_mul(1_000_000)
            .saturating_div(total_cus.saturating_add(1));

        if let (Some(nonce), Some(blockhash)) = (nonce, blockhash) {
            if let Ok(nonce_packets) = nonce_packets.read() {
                if let Some((max_sig, max_reward)) = nonce_packets.get(&(nonce, blockhash)) {
                    if *max_reward >= total_bundle_reward {
                        for (container_id, _, _) in container_ids.iter() {
                            self.transaction_view_state_container
                                .remove_by_id(*container_id);
                        }
                        let _ = nonce_packet_sender.send(*max_sig);
                        return Err(BundleStorageError::DuplicateNonce);
                    }
                }
            }
        }

        self.unprocessed_bundles.push(BundleTransactionId {
            container_ids,
            bundle_priority,
        });

        Ok(())
    }

    fn does_contain_duplicate_hashes(&self, container_ids: &[usize]) -> bool {
        let mut transaction_hashes = ArrayVec::<_, { Self::MAX_PACKETS_PER_BUNDLE }>::new();
        for container_id in container_ids.iter() {
            let transaction_hash = self
                .transaction_view_state_container
                .get_transaction(*container_id)
                .unwrap()
                .message_hash();
            if transaction_hashes.contains(&transaction_hash) {
                return true;
            }
            transaction_hashes.push(transaction_hash);
        }
        false
    }

    pub fn clear(&mut self) {
        while let Some(bundle) = self.unprocessed_bundles.pop_max() {
            for (id, _, _) in bundle.container_ids.iter() {
                self.transaction_view_state_container.remove_by_id(*id);
            }
        }
        for bundle in self.cost_model_buffered_bundles.drain(..) {
            for (id, _, _) in bundle.container_ids.iter() {
                self.transaction_view_state_container.remove_by_id(*id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            banking_stage::transaction_scheduler::{
                receive_and_buffer::PacketHandlingError,
                transaction_state_container::StateContainer,
            },
            bundle_stage::bundle_storage::{BundleStorage, BundleStorageError},
            packet_bundle::VerifiedPacketBundle,
        },
        ahash::{HashSet, HashSetExt},
        solana_genesis_config::GenesisConfig,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_perf::packet::{BytesPacket, PacketBatch},
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        solana_transaction::Transaction,
    };

    pub fn test_tx() -> Transaction {
        let keypair1 = Keypair::new();
        let pubkey1 = keypair1.pubkey();
        solana_system_transaction::transfer(&keypair1, &pubkey1, 42, Hash::default())
    }

    #[test]
    fn test_bundle_too_large() {
        let mut bundle_storage = BundleStorage::with_capacity(10);

        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let packets: Vec<BytesPacket> = (0..BundleStorage::MAX_PACKETS_PER_BUNDLE + 1)
            .map(|_| BytesPacket::from_data(None, test_tx()).unwrap())
            .collect();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(packets));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());

        assert_matches!(result, Err(BundleStorageError::BundleTooLarge));
        assert_eq!(bundle_storage.unprocessed_bundles.len(), 0);
        assert_eq!(bundle_storage.cost_model_buffered_bundles.len(), 0);
        assert!(bundle_storage.transaction_view_state_container.is_empty());
    }

    #[test]
    fn test_bundle_marked_discard() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let packet_1 = BytesPacket::from_data(None, test_tx()).unwrap();
        let mut packet_2 = BytesPacket::from_data(None, test_tx()).unwrap();
        packet_2.meta_mut().set_discard(true);
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet_1, packet_2]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert_matches!(result, Err(BundleStorageError::PacketMarkedDiscard(1)));
    }

    #[test]
    fn test_bundle_storage_exceeds_capacity() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());

        for i in 0..10 {
            let packet = BytesPacket::from_data(None, test_tx()).unwrap();
            let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet]));
            bundle_storage
                .insert_bundle(bundle, &bank, &bank, &HashSet::new())
                .unwrap();
            assert_eq!(bundle_storage.unprocessed_bundles.len(), i + 1);
            assert_eq!(
                bundle_storage
                    .transaction_view_state_container
                    .buffer_size(),
                i + 1
            );
        }

        let packet = BytesPacket::from_data(None, test_tx()).unwrap();

        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert_eq!(result, Err(BundleStorageError::ContainerFull));
        assert_eq!(bundle_storage.unprocessed_bundles.len(), 10);
        assert_eq!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size(),
            10
        );
    }

    #[test]
    fn test_bundle_empty() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert_matches!(result, Err(BundleStorageError::EmptyBatch));
    }

    #[test]
    fn test_bundle_duplicate_hashes() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let packet_1 = BytesPacket::from_data(None, test_tx()).unwrap();
        let packet_2 = packet_1.clone();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet_1, packet_2]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert_matches!(result, Err(BundleStorageError::DuplicateTransaction));
        assert!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size()
                == 0
        );
        assert!(bundle_storage.unprocessed_bundles.is_empty());
        assert!(bundle_storage.cost_model_buffered_bundles.is_empty());
    }

    #[test]
    fn test_retry_bundle() {
        let mut bundle_storage = BundleStorage::with_capacity(10);

        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let packet_1 = BytesPacket::from_data(None, test_tx()).unwrap();
        let packet_2 = BytesPacket::from_data(None, test_tx()).unwrap();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet_1, packet_2]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert!(result.is_ok());

        let bundle_storage_entry = bundle_storage.pop_bundle(bank.slot()).unwrap();
        bundle_storage.retry_bundle(bundle_storage_entry);

        assert!(bundle_storage.pop_bundle(bank.slot()).is_none());
        assert!(bundle_storage.unprocessed_bundles.is_empty());
        assert_eq!(bundle_storage.cost_model_buffered_bundles.len(), 1);
        assert_eq!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size(),
            2
        );

        bundle_storage.pop_bundle(bank.slot() + 1).unwrap();
    }

    #[test]
    fn test_bundle_blacklisted_account() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let tx = test_tx();
        let pubkey = tx.message().account_keys[0];
        let blacklisted_accounts = HashSet::from_iter([pubkey]);
        let packet = BytesPacket::from_data(None, tx).unwrap();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &blacklisted_accounts);
        assert_matches!(
            result,
            Err(BundleStorageError::PacketFilterError((
                PacketHandlingError::FilterKey,
                0
            )))
        );
    }

    #[test]
    fn test_retry_bundle_ordering_preserved() {
        let mut bundle_storage = BundleStorage::with_capacity(100);
        let bank = Bank::new_for_tests(&GenesisConfig::default());

        let tx_1 = test_tx();
        let tx_2 = test_tx();
        let tx_3 = test_tx();
        let tx_4 = test_tx();

        let packet_batch_1 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(None, &tx_1).unwrap(),
        ]));
        let packet_batch_2 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(None, &tx_2).unwrap(),
        ]));
        let packet_batch_3 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(None, &tx_3).unwrap(),
        ]));
        let packet_batch_4 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(None, &tx_4).unwrap(),
        ]));

        bundle_storage
            .insert_bundle(packet_batch_1, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_2, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_3, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_4, &bank, &bank, &HashSet::new())
            .unwrap();

        let bundle_storage_entry_1 = bundle_storage.pop_bundle(bank.slot()).unwrap();
        assert_eq!(
            bundle_storage_entry_1.transactions[0].signatures()[0],
            tx_1.signatures[0]
        );
        let bundle_storage_entry_2 = bundle_storage.pop_bundle(bank.slot()).unwrap();
        assert_eq!(
            bundle_storage_entry_2.transactions[0].signatures()[0],
            tx_2.signatures[0]
        );

        bundle_storage.retry_bundle(bundle_storage_entry_1);
        bundle_storage.destroy_bundle(bundle_storage_entry_2);

        let bundle_storage_entry_1 = bundle_storage.pop_bundle(bank.slot() + 1).unwrap();
        assert_eq!(
            bundle_storage_entry_1.transactions[0].signatures()[0],
            tx_1.signatures[0]
        );
        let bundle_storage_entry_3 = bundle_storage.pop_bundle(bank.slot() + 1).unwrap();
        assert_eq!(
            bundle_storage_entry_3.transactions[0].signatures()[0],
            tx_3.signatures[0]
        );
        let bundle_storage_entry_4 = bundle_storage.pop_bundle(bank.slot() + 1).unwrap();
        assert_eq!(
            bundle_storage_entry_4.transactions[0].signatures()[0],
            tx_4.signatures[0]
        );
    }

    #[test]
    fn test_destroy_bundle() {
        let mut bundle_storage = BundleStorage::with_capacity(100);
        let bank = Bank::new_for_tests(&GenesisConfig::default());

        let tx_1 = test_tx();
        let tx_2 = test_tx();

        let packet_batch_1 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(None, &tx_1).unwrap(),
        ]));
        let packet_batch_2 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(None, &tx_2).unwrap(),
        ]));

        bundle_storage
            .insert_bundle(packet_batch_1, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_2, &bank, &bank, &HashSet::new())
            .unwrap();

        let bundle_storage_entry_1 = bundle_storage.pop_bundle(bank.slot()).unwrap();
        bundle_storage.destroy_bundle(bundle_storage_entry_1);
        assert!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size()
                == 1
        );
        let bundle_storage_entry_2 = bundle_storage.pop_bundle(bank.slot()).unwrap();
        bundle_storage.destroy_bundle(bundle_storage_entry_2);
        assert!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size()
                == 0
        );
    }

    #[test]
    fn test_clear() {
        let mut bundle_storage = BundleStorage::with_capacity(100);
        let bank = Bank::new_for_tests(&GenesisConfig::default());

        let tx_1 = test_tx();
        let tx_2 = test_tx();

        let packet_batch_1 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(None, &tx_1).unwrap(),
        ]));
        let packet_batch_2 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(None, &tx_2).unwrap(),
        ]));

        bundle_storage
            .insert_bundle(packet_batch_1, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_2, &bank, &bank, &HashSet::new())
            .unwrap();

        bundle_storage.clear();
        assert!(bundle_storage.unprocessed_bundles.is_empty());
        assert!(bundle_storage.cost_model_buffered_bundles.is_empty());
        assert!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size()
                == 0
        );
    }
}

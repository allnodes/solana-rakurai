use {
    super::{LikeClusterInfo, transaction_scheduler::transaction_state_container::SharedBytes},
    crate::banking_stage::{
        DecisionState, SchedulerError, SchedulerObj,
        decision_maker::{BufferedPacketsDecision, DecisionMaker},
        scheduler_messages::MaxAge,
        transaction_scheduler::{
            scheduler_controller::translate_decision_into_decision_state,
            transaction_state::TransactionState,
        },
    },
    agave_reserved_account_keys::ReservedAccountKeys,
    agave_transaction_view::{
        resolved_transaction_view::ResolvedTransactionView,
        transaction_view::SanitizedTransactionView,
    },
    anchor_lang::{AccountDeserialize, prelude::Pubkey as AnchorPubkey},
    crossbeam_channel::Sender,
    jito_tip_distribution::{
        sdk::derive_tip_distribution_account_address,
        state::{ClaimStatus, TipDistributionAccount},
    },
    rakurai_activation::{
        sdk::{
            derive_activation_account_address,
            derive_config_account_address as derive_activation_config_account_address,
        },
        state::{RakuraiActivationAccount, RakuraiActivationConfigAccount},
    },
    rakurai_tip_manager::{
        TipManagerConfigAccount,
        sdk::{
            derive_rakurai_tip_manager_config_account_address,
            derive_rakurai_tip_payment_account_pdas,
            instruction::{
                ChangeTipReceiverAccounts, ChangeTipReceiverArgs, change_tip_receiver_ix,
            },
        },
    },
    reward_distribution::{
        sdk::{
            derive_config_account_address, derive_reward_collection_account_address,
            instruction::{
                InitializeRewardCollectionAccountAccounts, InitializeRewardCollectionAccountArgs,
                TransferBlockBuilderCommissionOnMevCommissionAccounts,
                TransferBlockBuilderCommissionOnMevCommissionArgs, TransferStakerRewardsAccounts,
                TransferStakerRewardsArgs, initialize_reward_collection_account_ix,
                transfer_block_builder_commission_on_mev_commission_ix, transfer_staker_rewards_ix,
            },
        },
        state::RewardCollectionAccount,
    },
    solana_account::ReadableAccount,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_ledger::blockstore::Blockstore,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_meta::TransactionMeta,
        transaction_with_meta::TransactionWithMeta,
    },
    solana_sdk_ids::system_program,
    solana_signature::Signature,
    solana_svm_transaction::{svm_message::SVMStaticMessage, svm_transaction::SVMTransaction},
    solana_transaction::{Transaction, sanitized::MessageHash, versioned::VersionedTransaction},
    solana_transaction_status::RewardType,
    std::{
        collections::HashMap,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering::Relaxed},
        },
        time::{Duration, Instant},
        u64,
    },
    thiserror::Error,
};

#[cfg(feature = "build_validator")]
use crate::banking_stage::rakurai_enabled;

#[cfg(feature = "build_validator")]
unsafe extern "C" {
    #[allow(improper_ctypes)]
    pub fn reset_rakurai();
}

#[derive(Clone, Debug, Default)]
pub struct RakuraiOpTxn {
    pub txn: Option<RuntimeTransaction<ResolvedTransactionView<Arc<Vec<u8>>>>>,
    pub landed: bool,
}

impl RakuraiOpTxn {
    pub fn default() -> Self {
        Self {
            txn: None,
            landed: false,
        }
    }

    /// Create a new pending transaction
    pub fn txn(&mut self, txn: RuntimeTransaction<ResolvedTransactionView<Arc<Vec<u8>>>>) {
        self.txn = Some(txn);
    }

    /// Reset state
    pub fn reset(&mut self) {
        self.txn = None;
        self.landed = false;
    }

    /// Mark transaction as landed and release txn memory
    pub fn landed(&mut self) {
        self.txn = None;
        self.landed = true;
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct LatestBankPair {
    pub root_bank: Arc<Bank>,
    pub working_bank: Arc<Bank>,
}

impl LatestBankPair {
    pub fn new(root_bank: Arc<Bank>, working_bank: Arc<Bank>) -> Self {
        Self {
            root_bank,
            working_bank,
        }
    }
}
#[derive(Debug, Clone)]
pub struct TxnsHistory {
    pub message_hash: Hash,
    pub blockhash: Hash,
    pub send_slot: u64,
    pub rewards: u64,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct RewardDistributionConfig {
    pub rakurai_activation_program_id: Pubkey,
    pub reward_distribution_program_id: Pubkey,
    pub rakurai_tip_manager_program_id: Pubkey,
    pub rewards_merkle_root_authority: Pubkey,
    pub tip_distribution_program_id: Pubkey,
    pub vote_account: Pubkey,
}

impl Default for RewardDistributionConfig {
    fn default() -> Self {
        Self {
            rakurai_activation_program_id: Pubkey::new_unique(),
            reward_distribution_program_id: Pubkey::new_unique(),
            rakurai_tip_manager_program_id: Pubkey::new_unique(),
            rewards_merkle_root_authority: Pubkey::new_unique(),
            tip_distribution_program_id: Pubkey::new_unique(),
            vote_account: Pubkey::new_unique(),
        }
    }
}

#[derive(PartialEq, Debug)]
pub enum RakuraiCommissionOnMevStatus {
    NotDeducted,
    Deducted,
    SkippedThisEpoch,
    TransactionSent,
}

pub struct RewardDistributor {
    cluster_info: Arc<ClusterInfo>,
    blockstore: Arc<Blockstore>,
    bank_forks: Arc<RwLock<BankForks>>,
    rakurai_commission_on_mev_commission_stats: RakuraiCommissionOnMevStatus,
    distribution_config: RewardDistributionConfig,
    shared_decision: (Arc<RwLock<DecisionState>>, Arc<AtomicBool>),
    high_priority_transaction_sender:
        Option<Sender<SchedulerObj<RuntimeTransaction<ResolvedTransactionView<SharedBytes>>>>>,
    txns_history: HashMap<Signature, TxnsHistory>,
    accumulated_reward: u64,
    decision_maker: DecisionMaker,
    shared_bank_update: Arc<RwLock<LatestBankPair>>,
    input_tx_signature_sender: Option<(Sender<String>, Arc<AtomicBool>)>,
    #[allow(unused)]
    reset_rakurai: Arc<AtomicBool>,
}

impl RewardDistributor {
    pub fn new(
        cluster_info: Arc<ClusterInfo>,
        blockstore: Arc<Blockstore>,
        bank_forks: Arc<RwLock<BankForks>>,
        distribution_config: RewardDistributionConfig,
        shared_decision: (Arc<RwLock<DecisionState>>, Arc<AtomicBool>),
        high_priority_transaction_sender: Option<
            Sender<SchedulerObj<RuntimeTransaction<ResolvedTransactionView<SharedBytes>>>>,
        >,
        decision_maker: DecisionMaker,
        shared_bank_update: Arc<RwLock<LatestBankPair>>,
        input_tx_signature_sender: Option<(Sender<String>, Arc<AtomicBool>)>,
        reset_rakurai: Arc<AtomicBool>,
    ) -> Self {
        Self {
            cluster_info,
            blockstore,
            bank_forks,
            rakurai_commission_on_mev_commission_stats: RakuraiCommissionOnMevStatus::NotDeducted,
            distribution_config,
            shared_decision,
            high_priority_transaction_sender,
            txns_history: HashMap::new(),
            accumulated_reward: 0,
            decision_maker,
            shared_bank_update,
            input_tx_signature_sender,
            reset_rakurai,
        }
    }

    pub fn read_rewards(&self, slot: Slot) -> Option<u64> {
        let bank_forks_r = self.bank_forks.read().ok()?;

        let bank = bank_forks_r.banks().get(&slot)?;
        if !bank.is_frozen() {
            return None;
        }

        let cloned_bank = bank.clone_without_scheduler();
        let rewards = cloned_bank.rewards.read().ok()?;

        let total = rewards
            .iter()
            .filter_map(|(_, reward_info)| {
                (reward_info.reward_type == RewardType::Fee).then_some(reward_info.lamports)
            })
            .sum::<i64>();

        Some(total as u64)
    }

    fn transfer_mev_commission(
        &mut self,
        bank: &Bank,
        rca_pda: AnchorPubkey,
        tda_pda: AnchorPubkey,
    ) -> Result<Option<Instruction>, RewardDistributorError> {
        let (derive_mev_claim_status_pda_address, _bump) = Pubkey::find_program_address(
            &[
                ClaimStatus::SEED,
                &self.distribution_config.vote_account.to_bytes(),
                &tda_pda.to_bytes(),
            ],
            &self.distribution_config.tip_distribution_program_id,
        );
        match bank.get_account(&derive_mev_claim_status_pda_address) {
            None => {
                // Tip not distributed yet
                return Ok(None);
            }
            Some(account_shared_data) => {
                let mut account_data = account_shared_data.data();
                let claim_status_account = ClaimStatus::try_deserialize(&mut account_data)
                    .ok()
                    .unwrap();

                let account_shared_data = bank
                    .get_account(&Pubkey::new_from_array(rca_pda.as_array().clone()))
                    .ok_or(RewardDistributorError::RcaAccountNotFound)?;
                let mut account_data = account_shared_data.data();
                let reward_collection_account =
                    RewardCollectionAccount::try_deserialize(&mut account_data)
                        .ok()
                        .ok_or(RewardDistributorError::RcaDeserializationFailed)?;

                let reward_distribution_program_id = AnchorPubkey::from(
                    self.distribution_config
                        .reward_distribution_program_id
                        .as_array()
                        .clone(),
                );
                let system_program = AnchorPubkey::from(system_program::id().as_array().clone());
                let identity =
                    AnchorPubkey::from(self.cluster_info.id().clone().as_array().clone());

                let mut instruction = transfer_block_builder_commission_on_mev_commission_ix(
                    reward_distribution_program_id,
                    TransferBlockBuilderCommissionOnMevCommissionArgs {
                        mev_rewards: claim_status_account.amount,
                    },
                    TransferBlockBuilderCommissionOnMevCommissionAccounts {
                        reward_collection_account: rca_pda,
                        block_builder_commission_account: reward_collection_account
                            .block_builder_commission_account,
                        system_program: system_program,
                        signer: identity,
                    },
                );
                let acct_metas: Vec<AccountMeta> = instruction
                    .accounts
                    .iter_mut()
                    .map(|acct| AccountMeta {
                        pubkey: Pubkey::from(acct.pubkey.as_array().clone()),
                        is_signer: acct.is_signer,
                        is_writable: acct.is_writable,
                    })
                    .collect();

                Ok(Some(Instruction::new_with_bytes(
                    self.distribution_config.reward_distribution_program_id,
                    &instruction.data,
                    acct_metas,
                )))
            }
        }
    }

    fn should_deduct_mev_commission(
        &mut self,
        bank: &Bank,
        threshold: u8,
    ) -> (bool, Option<AnchorPubkey>, Option<AnchorPubkey>) {
        let epoch = bank.epoch();
        let epoch_schedule = bank.epoch_schedule();
        let first_slot = epoch_schedule.get_first_slot_in_epoch(epoch);
        let last_slot = epoch_schedule.get_last_slot_in_epoch(epoch);
        let current_slot = bank.slot();

        // ---- Step 2: Check epoch progress
        let total_slots = last_slot - first_slot;
        let completed_slots = current_slot - first_slot;
        let completed_percent = (completed_slots as f64 / total_slots as f64) * 100.0;

        if completed_percent < threshold as f64 {
            trace!(
                "Not enough progress yet epoch: {epoch:}, completed_slots: {completed_slots:}, completed_percent: {completed_percent:}"
            );
            return (false, None, None);
        }

        let reward_distribution_program_id = AnchorPubkey::from(
            self.distribution_config
                .reward_distribution_program_id
                .as_array()
                .clone(),
        );
        let tip_distribution_program_id = AnchorPubkey::from(
            self.distribution_config
                .tip_distribution_program_id
                .as_array()
                .clone(),
        );
        let vote_account =
            AnchorPubkey::from(self.distribution_config.vote_account.as_array().clone());

        // ---- Step 2: Derive PDA addresses for last epoch
        let (rca_pda, _) = derive_reward_collection_account_address(
            &reward_distribution_program_id,
            &vote_account,
            epoch - 1,
        );
        let (tda_pda, _) = derive_tip_distribution_account_address(
            &tip_distribution_program_id,
            &vote_account,
            epoch - 1,
        );

        // ---- Step 3: Check RCA
        let rca_ok = bank
            .get_account(&Pubkey::from(rca_pda.as_array().clone()))
            .and_then(|account_shared_data| {
                let mut account_data = account_shared_data.data();
                RewardCollectionAccount::try_deserialize(&mut account_data).ok()
            })
            .map_or(false, |rca| {
                match rca.block_builder_mev_commission_deducted {
                    None => {
                        self.rakurai_commission_on_mev_commission_stats =
                            RakuraiCommissionOnMevStatus::SkippedThisEpoch;
                        false
                    }
                    Some(0) => true,
                    Some(_) => {
                        self.rakurai_commission_on_mev_commission_stats =
                            RakuraiCommissionOnMevStatus::Deducted;
                        false
                    }
                }
            });

        if !rca_ok {
            return (false, Some(rca_pda), Some(tda_pda));
        }

        // ---- Step 4: Check TDA
        let tda_ok = bank
            .get_account(&Pubkey::from(tda_pda.as_array().clone()))
            .and_then(|account_shared_data| {
                let mut account_data = account_shared_data.data();
                TipDistributionAccount::try_deserialize(&mut account_data).ok()
            })
            .map_or(false, |tda| {
                if tda.validator_commission_bps == 0 {
                    debug!("TDA has zero commission, skipping MEV commission deduction");
                    self.rakurai_commission_on_mev_commission_stats =
                        RakuraiCommissionOnMevStatus::SkippedThisEpoch;
                    false
                } else if tda.merkle_root.is_none() {
                    debug!("TDA has valid commission, proceeding with MEV commission deduction");
                    false
                } else {
                    debug!("TDA has valid commission, proceeding with MEV commission deduction");
                    true
                }
            });

        if !tda_ok {
            return (false, Some(rca_pda), Some(tda_pda));
        }

        (true, Some(rca_pda), Some(tda_pda))
    }

    fn create_transfer_rca_instruction(
        &mut self,
        total_rewards: u64,
        reward_account: Pubkey,
        bank: &Bank,
    ) -> Result<Option<Instruction>, RewardDistributorError> {
        let account_shared_data = bank
            .get_account(&reward_account)
            .ok_or(RewardDistributorError::RcaAccountNotFound)?;
        let mut account_data = account_shared_data.data();
        let reward_collection_account = RewardCollectionAccount::try_deserialize(&mut account_data)
            .ok()
            .ok_or(RewardDistributorError::RcaDeserializationFailed)?;

        if reward_collection_account.block_reward_commission_bps == 10_000
            && reward_collection_account.block_builder_commission_bps == 0
        {
            self.accumulated_reward = 0;
            return Ok(None);
        }

        let reward_distribution_program_id = AnchorPubkey::from(
            self.distribution_config
                .reward_distribution_program_id
                .as_array()
                .clone(),
        );
        let reward_account = AnchorPubkey::from(reward_account.as_array().clone());
        let system_program = AnchorPubkey::from(system_program::id().as_array().clone());
        let identity = AnchorPubkey::from(self.cluster_info.id().clone().to_bytes());
        let mut instruction = transfer_staker_rewards_ix(
            reward_distribution_program_id,
            TransferStakerRewardsArgs { total_rewards },
            TransferStakerRewardsAccounts {
                reward_collection_account: reward_account,
                block_builder_commission_account: reward_collection_account
                    .block_builder_commission_account,
                system_program,
                signer: identity,
            },
        );

        let acct_metas: Vec<AccountMeta> = instruction
            .accounts
            .iter_mut()
            .map(|acct| AccountMeta {
                pubkey: Pubkey::from(acct.pubkey.as_array().clone()),
                is_signer: acct.is_signer,
                is_writable: acct.is_writable,
            })
            .collect();

        Ok(Some(Instruction::new_with_bytes(
            self.distribution_config.reward_distribution_program_id,
            &instruction.data,
            acct_metas,
        )))
    }

    fn get_reward_collection_pda_status(&mut self, bank: &Bank) -> (bool, Pubkey) {
        let reward_distribution_program_id = AnchorPubkey::from(
            self.distribution_config
                .reward_distribution_program_id
                .as_array()
                .clone(),
        );
        let vote_account =
            AnchorPubkey::from(self.distribution_config.vote_account.as_array().clone());
        let (pda, _) = derive_reward_collection_account_address(
            &reward_distribution_program_id,
            &vote_account,
            bank.epoch(),
        );
        let pda = Pubkey::from(pda.as_array().clone());

        let rca_created = match bank.get_account(&pda) {
            None => false,
            Some(account) => {
                if account.owner() == &self.distribution_config.reward_distribution_program_id {
                    true
                } else {
                    false
                }
            }
        };
        (rca_created, pda)
    }

    fn initialize_reward_collection_account_instruction(
        &self,
        bank: &Bank,
    ) -> Result<Instruction, RewardDistributorError> {
        let rakurai_activation_program_id = AnchorPubkey::from(
            self.distribution_config
                .rakurai_activation_program_id
                .as_array()
                .clone(),
        );
        let activation_config_account_pubkey =
            derive_activation_config_account_address(&rakurai_activation_program_id).0;
        let config_account_shared_data = bank
            .get_account(&Pubkey::from(
                activation_config_account_pubkey.as_array().clone(),
            ))
            .ok_or(RewardDistributorError::RaaConfigAccountNotFound)?;
        let mut config_account_data = config_account_shared_data.data();
        let rakurai_activation_config =
            RakuraiActivationConfigAccount::try_deserialize(&mut config_account_data)
                .ok()
                .ok_or(RewardDistributorError::RaaConfigDeserializationFailed)?;

        let identity = AnchorPubkey::from(self.cluster_info.id().clone().as_array().clone());
        let activation_account_pubkey =
            derive_activation_account_address(&rakurai_activation_program_id, &identity).0;

        let account_shared_data = bank
            .get_account(&Pubkey::from(activation_account_pubkey.as_array().clone()))
            .ok_or(RewardDistributorError::RaaAccountNotFound)?;
        let mut account_data = account_shared_data.data();
        let rakurai_activation = RakuraiActivationAccount::try_deserialize(&mut account_data)
            .ok()
            .ok_or(RewardDistributorError::RaaDeserializationFailed)?;

        let reward_distribution_program_id = AnchorPubkey::from(
            self.distribution_config
                .reward_distribution_program_id
                .as_array()
                .clone(),
        );
        let vote_account =
            AnchorPubkey::from(self.distribution_config.vote_account.as_array().clone());
        let (reward_collection_account, bump) = derive_reward_collection_account_address(
            &reward_distribution_program_id,
            &vote_account,
            bank.epoch(),
        );

        let rewards_merkle_root_authority = AnchorPubkey::from(
            self.distribution_config
                .rewards_merkle_root_authority
                .as_array()
                .clone(),
        );
        let system_program = AnchorPubkey::from(system_program::id().as_array().clone());
        let mut instruction = initialize_reward_collection_account_ix(
            reward_distribution_program_id,
            InitializeRewardCollectionAccountArgs {
                merkle_root_upload_authority: rewards_merkle_root_authority,
                block_reward_commission_bps: rakurai_activation.block_reward_commission_bps,
                block_builder_commission_account: rakurai_activation_config
                    .block_builder_commission_account,
                block_builder_commission_bps: rakurai_activation.block_builder_commission_bps,
                bump,
            },
            InitializeRewardCollectionAccountAccounts {
                config: derive_config_account_address(&reward_distribution_program_id).0,
                reward_collection_account,
                validator_vote_account: vote_account,
                signer: identity,
                system_program,
            },
        );

        let acct_metas: Vec<AccountMeta> = instruction
            .accounts
            .iter_mut()
            .map(|acct| AccountMeta {
                pubkey: Pubkey::from(acct.pubkey.as_array().clone()),
                is_signer: acct.is_signer,
                is_writable: acct.is_writable,
            })
            .collect();

        Ok(Instruction::new_with_bytes(
            self.distribution_config.reward_distribution_program_id,
            &instruction.data,
            acct_metas,
        ))
    }

    fn create_runtime_transaction(
        &self,
        bank: &Bank,
        instructions: &[Instruction],
    ) -> Option<RuntimeTransaction<ResolvedTransactionView<SharedBytes>>> {
        let message = Message::new(&instructions, Some(&self.cluster_info.id()));
        let tx = Transaction::new(
            &[self.cluster_info.keypair().clone()],
            message,
            bank.last_blockhash(),
        );
        let enable_static_instruction_limit = bank
            .feature_set
            .is_active(&agave_feature_set::static_instruction_limit::ID);

        let serialized_transaction = {
            let transaction = VersionedTransaction::from(tx);
            bincode::serialize(&transaction).unwrap()
        };
        let transaction = SanitizedTransactionView::try_new_sanitized(
            Arc::clone(&Arc::new(serialized_transaction)),
            enable_static_instruction_limit,
        )
        .unwrap();

        let static_runtime_transaction =
            RuntimeTransaction::<SanitizedTransactionView<SharedBytes>>::try_new(
                transaction,
                MessageHash::Compute,
                None,
            )
            .ok()?;

        let dynamic_runtime_transaction =
            RuntimeTransaction::<ResolvedTransactionView<SharedBytes>>::try_new(
                static_runtime_transaction,
                None,
                &ReservedAccountKeys::empty_key_set(),
            );

        if dynamic_runtime_transaction.is_ok() {
            Some(dynamic_runtime_transaction.unwrap())
        } else {
            None
        }
    }

    fn check_txn_status(&mut self) {
        let bank_forks_r = self.bank_forks.read();
        if bank_forks_r.is_ok() {
            let working_bank = bank_forks_r.unwrap().working_bank();
            let current_slot = working_bank.slot();

            self.txns_history.retain(|_sig, history| {
                let is_root = self.blockstore.is_root(history.send_slot);
                if !is_root {
                    return true;
                }

                let within_range = current_slot <= history.send_slot + 150;
                if !within_range {
                    return true;
                }
                let stats_cache_r = working_bank.status_cache.read();
                let status = if stats_cache_r.is_ok() {
                    stats_cache_r.unwrap().get_status(
                        &history.message_hash,
                        &history.blockhash,
                        &working_bank.ancestors,
                    )
                } else {
                    None
                };

                let has_status = status.is_some();
                if has_status {
                    return false;
                } else {
                    return true;
                }
            });
        }
    }

    pub fn change_tip_receiver_instruction(
        &mut self,
        bank: &Arc<Bank>,
    ) -> Result<Option<Instruction>, RewardDistributorError> {
        // Get TipManager config Account
        let rakurai_tip_manager_program_id = AnchorPubkey::from(
            self.distribution_config
                .rakurai_tip_manager_program_id
                .as_array()
                .clone(),
        );
        let identity = AnchorPubkey::from(self.cluster_info.id().clone().as_array().clone());
        let tip_manager_config_pda =
            derive_rakurai_tip_manager_config_account_address(&rakurai_tip_manager_program_id);
        let account_data = bank
            .get_account(&Pubkey::new_from_array(
                *tip_manager_config_pda.0.as_array(),
            ))
            .ok_or(RewardDistributorError::TipConfigAccountNotFound)?;
        let tip_manager_config = TipManagerConfigAccount::try_deserialize(&mut account_data.data())
            .ok()
            .ok_or(RewardDistributorError::TipConfigDeserializationFailed)?;
        let tip_accounts = derive_rakurai_tip_payment_account_pdas(&rakurai_tip_manager_program_id);

        // Run change_tip_receiver instruction if active validator_tip_receiver_account != validator identity
        // Todo:
        // - phase-1: transfer tips directly to validator identity (done)
        // - phase-2: transfer tip to RCA and option to distribute tip along with block rewards
        // - phase-3: convert tips to block reward
        if tip_manager_config.validator_tip_receiver_account != identity {
            let mut instruction = change_tip_receiver_ix(
                rakurai_tip_manager_program_id,
                ChangeTipReceiverArgs,
                ChangeTipReceiverAccounts {
                    tip_manager_config: tip_manager_config_pda.0,
                    old_tip_receiver: tip_manager_config.validator_tip_receiver_account,
                    new_tip_receiver: identity,
                    block_builder_commission_account: tip_manager_config
                        .block_builder_commission_account,
                    rakurai_tip_account_0: tip_accounts[0].0,
                    rakurai_tip_account_1: tip_accounts[1].0,
                    rakurai_tip_account_2: tip_accounts[2].0,
                    rakurai_tip_account_3: tip_accounts[3].0,
                    rakurai_tip_account_4: tip_accounts[4].0,
                    rakurai_tip_account_5: tip_accounts[5].0,
                    rakurai_tip_account_6: tip_accounts[6].0,
                    rakurai_tip_account_7: tip_accounts[7].0,
                    signer: identity,
                },
            );

            let acct_metas: Vec<AccountMeta> = instruction
                .accounts
                .iter_mut()
                .map(|acct| AccountMeta {
                    pubkey: Pubkey::from(acct.pubkey.as_array().clone()),
                    is_signer: acct.is_signer,
                    is_writable: acct.is_writable,
                })
                .collect();

            return Ok(Some(Instruction::new_with_bytes(
                self.distribution_config.rakurai_tip_manager_program_id,
                &instruction.data,
                acct_metas,
            )));
        }
        Ok(None)
    }

    fn send_transaction(
        &self,
        runtime_tx: RuntimeTransaction<ResolvedTransactionView<SharedBytes>>,
    ) {
        let transaction_state = TransactionState::new(runtime_tx, MaxAge::MAX, u64::MAX, 150);
        if let Some(sender) = &self.high_priority_transaction_sender {
            // -----------------------------------------------------------------------------
            // TX Input Signature Reporting
            //
            // This block sends all incoming transaction signatures (`tx_in_signature`) to
            // the HouseKeeper via `input_tx_signature_sender`. Each
            // transaction in the batch is processed to extract its signature. If a
            // transaction fails deserialization, a serialized packet fallback is used
            // to still identify the transaction.
            //
            // See the "tx_io_check_readme.md" for details on how these
            // tx_in_signature messages are recorded and analyzed:
            //   <repo-root>/tx_io_check_readme.md
            //
            // Collecting tx_in_signature ensures end-to-end auditing of transaction
            // entry into the scheduler, enabling detection of missing or censored
            // transactions and providing full transparency of scheduler behavior.
            // -----------------------------------------------------------------------------
            if let Some((input_tx_signature_sender, exit)) = &self.input_tx_signature_sender {
                if !exit.load(Relaxed) {
                    let _ = input_tx_signature_sender.try_send(
                        transaction_state
                            .transaction()
                            .signatures()
                            .first()
                            .unwrap()
                            .to_string(),
                    );
                }
            }
            let _ = sender.send(SchedulerObj {
                scheduler_work_load: vec![transaction_state],
            });
        }
    }

    fn read_rewards_and_check_txn_history(
        &mut self,
        slot_rewards: &mut HashMap<Slot, u64>,
        buffered_slots: &mut Vec<u64>,
    ) {
        slot_rewards.retain(|slot, reward| {
            if self.blockstore.is_root(*slot) {
                self.accumulated_reward += *reward;
                false
            } else if self.blockstore.is_skipped(*slot) {
                false //remove from record if skipped | missing from ledger
            } else {
                true //do not remove from record if !(skipped | rooted)
            }
        });
        buffered_slots.retain(|slot| match self.read_rewards(*slot) {
            Some(reward) => {
                info!(
                    "reward_distributor read-rewards-slot={:?},reward={}",
                    slot, reward
                );
                slot_rewards.insert(*slot, reward);
                false
            }
            None => {
                if self.blockstore.is_skipped(*slot) {
                    false
                } else {
                    true
                }
            }
        });

        if !self.txns_history.is_empty() {
            self.check_txn_status();
        }
    }

    fn is_slot_changed(slot: &u64, previous_slot: &mut u64) -> bool {
        if slot != previous_slot {
            *previous_slot = *slot;
            true
        } else {
            false
        }
    }

    pub fn run(mut self) -> Result<(), SchedulerError> {
        let mut decision;
        let mut slot_rewards: HashMap<Slot, u64> = HashMap::new();
        let mut buffered_slots = Vec::new();
        let mut prev_decision = BufferedPacketsDecision::Hold;
        let mut switching_point;
        let mut slot;
        let mut previous_slot = 0;
        #[cfg(feature = "build_validator")]
        let mut rakurai_enabled_flag;
        // leader's last slot, used to detect the change in slot
        let mut last_leader_slot: u64 = 0;
        // used to detect the turn has started, it will be set at first consume decision and reset at first forward decision
        let mut turn_started: bool = false;

        // this is const of 5ms because make_consume_or_forward_decision updates its decision after every 5 ms
        // so polling make_consume_or_forward_decision at a higher frequency is not needed
        let timeout_ms = Duration::from_millis(5);
        let mut last_epoch = 0;

        let mut rakurai_op_txn = RakuraiOpTxn::default();
        let mut is_tip_receiver_changed;
        // Retry interval while RCA is not initialized or tip receiver is not yet updated
        let rakurai_op_txn_retry_interval_ms = Duration::from_millis(25);
        let mut instant = Instant::now();
        let mut instructions = Vec::new();
        let mut last_log_slot = 0;

        loop {
            std::thread::sleep(timeout_ms);
            instructions.clear();

            #[cfg(feature = "build_validator")]
            if self.reset_rakurai.load(Relaxed) {
                unsafe {
                    info!("resetting rakurai");
                    reset_rakurai();
                }

                self.reset_rakurai.store(false, Relaxed);
            }
            // Get the current decision and switching point flag from the decision maker
            (decision, switching_point, slot) =
                self.decision_maker.make_consume_or_forward_decision();

            if decision != prev_decision {
                match &decision {
                    BufferedPacketsDecision::Consume(bank_start) => {
                        let root_bank = self.bank_forks.read().unwrap().root_bank();
                        if let Ok(mut shared_bank_update) = self.shared_bank_update.write() {
                            *shared_bank_update = LatestBankPair::new(
                                root_bank.clone(),
                                bank_start.working_bank.clone(),
                            );
                        }
                        previous_slot = bank_start.working_bank.slot();
                    }
                    _ => {
                        let root_bank = self.bank_forks.read().unwrap().root_bank();
                        let working_bank = self.bank_forks.read().unwrap().working_bank();
                        if let Ok(mut shared_bank_update) = self.shared_bank_update.write() {
                            *shared_bank_update =
                                LatestBankPair::new(root_bank.clone(), working_bank.clone());
                        }
                        previous_slot = working_bank.slot();
                    }
                }
            } else if Self::is_slot_changed(&slot, &mut previous_slot) {
                if let Ok(bank_forks_read_lock) = self.bank_forks.read() {
                    let root_bank = bank_forks_read_lock.root_bank();
                    let working_bank = bank_forks_read_lock.working_bank();
                    if let Ok(mut shared_bank_update) = self.shared_bank_update.write() {
                        *shared_bank_update =
                            LatestBankPair::new(root_bank.clone(), working_bank.clone());
                    }
                    previous_slot = working_bank.slot();
                }
            }

            // Update the switching point flag when in forwarding because it only changes during forwarding decision
            if let BufferedPacketsDecision::Forward = decision {
                self.shared_decision.1.store(switching_point, Relaxed);
            }

            // Only update decision state if it has changed
            if decision != prev_decision {
                prev_decision = decision.clone();

                let mut decision_state_lock = self.shared_decision.0.write().unwrap();
                *decision_state_lock = translate_decision_into_decision_state(&decision);

                if let BufferedPacketsDecision::Consume(bank_start) = &decision {
                    let new_leader_slot = bank_start.working_bank.slot();
                    if new_leader_slot != last_leader_slot {
                        last_leader_slot = new_leader_slot;
                        #[cfg(feature = "build_validator")]
                        unsafe {
                            rakurai_enabled_flag = rakurai_enabled();
                        }
                        #[cfg(feature = "build_validator")]
                        {
                            if rakurai_enabled_flag {
                                // Push new slots if Rakurai is enabled
                                buffered_slots.push(new_leader_slot);
                            }
                        }

                        if !turn_started {
                            // Mark the turn as started
                            turn_started = true;

                            // Check for epoch change
                            let current_epoch = bank_start.working_bank.epoch();
                            if last_epoch != 0 && last_epoch != current_epoch {
                                info!(
                                    "reward_distributor epoch changed from {} to {}, slot {}",
                                    last_epoch, current_epoch, slot
                                );
                                // Reset MEV commission status on epoch change
                                self.rakurai_commission_on_mev_commission_stats =
                                    RakuraiCommissionOnMevStatus::NotDeducted;
                            }
                            last_epoch = current_epoch;

                            // Reset MEV commission status for new turn (except if already deducted or skipped)
                            if self.rakurai_commission_on_mev_commission_stats
                                == RakuraiCommissionOnMevStatus::TransactionSent
                            {
                                self.rakurai_commission_on_mev_commission_stats =
                                    RakuraiCommissionOnMevStatus::NotDeducted;
                            }
                        }
                    }
                }
            }

            match decision {
                BufferedPacketsDecision::ForwardAndHold => {
                    self.read_rewards_and_check_txn_history(&mut slot_rewards, &mut buffered_slots);

                    let bank_forks_r = self.bank_forks.read();
                    if bank_forks_r.is_ok() {
                        let current_slot = bank_forks_r.unwrap().working_bank().slot();
                        self.txns_history.retain(|_, history| {
                            if current_slot > history.send_slot + 150 {
                                self.accumulated_reward += history.rewards;
                                false
                            } else {
                                true
                            }
                        });
                    }
                    turn_started = false;
                }

                BufferedPacketsDecision::Consume(bank_start) => {
                    if rakurai_op_txn.landed == false && rakurai_op_txn.txn.is_none() {
                        is_tip_receiver_changed = false;
                        instant = Instant::now();

                        let working_bank = bank_start.working_bank;
                        let (rca_created, reward_collection_account) =
                            self.get_reward_collection_pda_status(&working_bank);

                        // Initalized RCA
                        if !rca_created {
                            match self
                                .initialize_reward_collection_account_instruction(&working_bank)
                            {
                                Ok(ix) => {
                                    debug!(
                                        "reward_distributor initialize_reward_collection_account_instruction"
                                    );
                                    instructions.push(ix);
                                }
                                Err(e) => {
                                    error!(
                                        "reward_distributor error in initialize_reward_collection_account_instruction: {e}"
                                    );
                                }
                            }
                        }

                        // Change Tip Receiver
                        match self.change_tip_receiver_instruction(&working_bank) {
                            Ok(maybe_ix) => match maybe_ix {
                                Some(ix) => {
                                    debug!("reward_distributor change_tip_receiver_instruction");
                                    is_tip_receiver_changed = true;
                                    instructions.push(ix);
                                }
                                None => {
                                    debug!(
                                        "reward_distributor change_tip_receiver_instruction not required"
                                    );
                                }
                            },
                            Err(e) => {
                                error!(
                                    "reward_distributor error in change_tip_receiver_instruction: {e}"
                                );
                            }
                        }

                        // Transfer block rewards to RCA
                        if self.accumulated_reward > 0 {
                            match self.create_transfer_rca_instruction(
                                self.accumulated_reward,
                                reward_collection_account,
                                &working_bank,
                            ) {
                                Ok(maybe_ix) => match maybe_ix {
                                    Some(ix) => {
                                        debug!(
                                            "reward_distributor create_transfer_rca_instruction"
                                        );
                                        instructions.push(ix);
                                    }
                                    None => {
                                        debug!(
                                            "reward_distributor create_transfer_rca_instruction not required"
                                        );
                                    }
                                },
                                Err(e) => {
                                    error!(
                                        "reward_distributor error in create_transfer_rca_instruction: {e}"
                                    );
                                }
                            }
                        }

                        // Check & Create MEV commission transfer instruction if applicable
                        match self.rakurai_commission_on_mev_commission_stats {
                            RakuraiCommissionOnMevStatus::NotDeducted => {
                                let (should_deduct, rca_pda, tda_pda) =
                                    self.should_deduct_mev_commission(&working_bank, 50);
                                if should_deduct && rca_pda.is_some() && tda_pda.is_some() {
                                    info!("reward_distributor deducting mev commission");
                                    match self.transfer_mev_commission(
                                        &working_bank,
                                        rca_pda.unwrap(),
                                        tda_pda.unwrap(),
                                    ) {
                                        Ok(maybe_ix) => match maybe_ix {
                                            Some(ix) => {
                                                debug!(
                                                    "reward_distributor transfer_mev_commission"
                                                );
                                                instructions.push(ix);
                                            }
                                            None => {
                                                debug!(
                                                    "reward_distributor transfer_mev_commission not required"
                                                );
                                            }
                                        },
                                        Err(e) => {
                                            error!("error in transfer_mev_commission: {e}");
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }

                        // Create and Send Txn
                        if instructions.len() > 0 {
                            if let Some(runtime_tx) =
                                self.create_runtime_transaction(&working_bank, &instructions)
                            {
                                let signature = runtime_tx.signature().clone();
                                info!(
                                    "reward_distributor txn_sent instructions={},sig={signature}",
                                    instructions.len()
                                );
                                self.txns_history.insert(
                                    signature,
                                    TxnsHistory {
                                        rewards: self.accumulated_reward,
                                        message_hash: runtime_tx.message_hash().clone(),
                                        send_slot: working_bank.slot(),
                                        blockhash: runtime_tx.recent_blockhash().clone(),
                                    },
                                );

                                rakurai_op_txn.txn(runtime_tx.clone());
                                self.accumulated_reward = 0;
                                self.send_transaction(runtime_tx);
                            }
                        }

                        // Log once per leader slot
                        if working_bank.slot() != last_log_slot {
                            last_log_slot = working_bank.slot();
                            info!(
                                "reward_distributor decision=consume, rca={}, epoch={}, slot={}, rca_status={:?}, rakurai_commission_on_mev_commission_stats={:?}, change_tip_receiver_instruction={}, pending_txns_count={}, pending_txns={:?}",
                                reward_collection_account,
                                working_bank.epoch(),
                                working_bank.slot(),
                                rca_created,
                                self.rakurai_commission_on_mev_commission_stats,
                                is_tip_receiver_changed,
                                self.txns_history.len(),
                                self.txns_history
                                    .iter()
                                    .map(|(sig, tx)| (sig, tx.send_slot, tx.rewards))
                                    .collect::<Vec<_>>()
                            );
                        }
                    } else {
                        // Continue is txn landed is true (for this turn)
                        if rakurai_op_txn.landed {
                            continue;
                        }
                        if instant.elapsed() <= rakurai_op_txn_retry_interval_ms {
                            continue;
                        }
                        let landed = {
                            let Some(runtime_tx) = rakurai_op_txn.txn.as_ref() else {
                                continue;
                            };
                            if bank_start
                                .working_bank
                                .get_signature_status_with_blockhash(
                                    runtime_tx.as_sanitized_transaction().signature(),
                                    runtime_tx.as_sanitized_transaction().recent_blockhash(),
                                )
                                .is_none()
                            {
                                self.send_transaction(runtime_tx.clone());
                                instant = Instant::now();
                                false
                            } else {
                                true
                            }
                        };
                        // mark txn as landed (landed=true, txn:None)
                        if landed {
                            rakurai_op_txn.landed();
                        }
                    }
                }

                BufferedPacketsDecision::Forward => {
                    self.read_rewards_and_check_txn_history(&mut slot_rewards, &mut buffered_slots);
                    turn_started = false;
                    rakurai_op_txn.reset();
                }

                BufferedPacketsDecision::Hold => {}
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum RewardDistributorError {
    #[error("RCA account not found")]
    RcaAccountNotFound,

    #[error("RCA deserialization failed")]
    RcaDeserializationFailed,

    #[error("RAA config account not found")]
    RaaConfigAccountNotFound,

    #[error("RAA config deserialization failed")]
    RaaConfigDeserializationFailed,

    #[error("RAA account not found")]
    RaaAccountNotFound,

    #[error("RAA deserialization failed")]
    RaaDeserializationFailed,

    #[error("Tip config account not found")]
    TipConfigAccountNotFound,

    #[error("Tip config deserialization failed")]
    TipConfigDeserializationFailed,
}

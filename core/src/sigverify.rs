//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.

use std::sync::{Arc, atomic::AtomicBool};

pub use solana_perf::sigverify::{
    TxOffset, count_packets_in_batches, ed25519_verify, ed25519_verify_disabled,
};
use {
    crate::{
        banking_trace::BankingPacketSender,
        sigverify_stage::{SigVerifier, SigVerifyServiceError},
    },
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::{Sender, TrySendError},
    solana_perf::{packet::PacketBatch, sigverify},
};

pub struct TransactionSigVerifier {
    thread_pool: Arc<rayon::ThreadPool>,
    banking_stage_sender: BankingPacketSender,
    forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
    reject_non_vote: bool,
    input_tx_signature_sender: Option<(Sender<String>, Arc<AtomicBool>)>,
}

impl TransactionSigVerifier {
    pub fn new_reject_non_vote(
        thread_pool: Arc<rayon::ThreadPool>,
        packet_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
    ) -> Self {
        let mut new_self = Self::new(thread_pool, packet_sender, forward_stage_sender, None);
        new_self.reject_non_vote = true;
        new_self
    }

    pub fn new(
        thread_pool: Arc<rayon::ThreadPool>,
        banking_stage_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
        input_tx_signature_sender: Option<(Sender<String>, Arc<AtomicBool>)>,
    ) -> Self {
        Self {
            thread_pool,
            banking_stage_sender,
            forward_stage_sender,
            reject_non_vote: false,
            input_tx_signature_sender,
        }
    }
}

impl SigVerifier for TransactionSigVerifier {
    type SendType = BankingPacketBatch;

    fn send_packets(
        &mut self,
        packet_batches: Vec<PacketBatch>,
    ) -> Result<(), SigVerifyServiceError<Self::SendType>> {
        let banking_packet_batch = BankingPacketBatch::new(packet_batches);
        if let Some(forward_stage_sender) = &self.forward_stage_sender {
            self.banking_stage_sender.send(
                banking_packet_batch.clone(),
                &self.input_tx_signature_sender,
            )?;
            if let Err(TrySendError::Full(_)) =
                forward_stage_sender.try_send((banking_packet_batch, self.reject_non_vote))
            {
                warn!("forwarding stage channel is full, dropping packets.");
            }
        } else {
            self.banking_stage_sender
                .send(banking_packet_batch, &self.input_tx_signature_sender)?;
        }

        Ok(())
    }

    fn verify_batches(
        &self,
        mut batches: Vec<PacketBatch>,
        valid_packets: usize,
    ) -> Vec<PacketBatch> {
        sigverify::ed25519_verify(
            &self.thread_pool,
            &mut batches,
            self.reject_non_vote,
            valid_packets,
        );
        batches
    }
}

#![cfg_attr(feature = "cargo-clippy", allow(needless_pass_by_value))]

mod block_proposal_process;
mod block_transactions_process;
pub mod compact_block;
mod compact_block_process;
mod get_block_proposal_process;
mod get_block_transactions_process;
mod transaction_process;

use self::block_proposal_process::BlockProposalProcess;
use self::block_transactions_process::BlockTransactionsProcess;
use self::compact_block::CompactBlock;
use self::compact_block_process::CompactBlockProcess;
use self::get_block_proposal_process::GetBlockProposalProcess;
use self::get_block_transactions_process::GetBlockTransactionsProcess;
use self::transaction_process::TransactionProcess;
use bigint::H256;
use ckb_chain::chain::ChainProvider;
use ckb_pow::PowEngine;
use ckb_protocol::{short_transaction_id, short_transaction_id_keys, RelayMessage, RelayPayload};
use ckb_verification::{BlockVerifier, Verifier};
use core::block::{Block, BlockBuilder};
use core::transaction::{ProposalShortId, Transaction};
use flatbuffers::{get_root, FlatBufferBuilder};
use fnv::{FnvHashMap, FnvHashSet};
use futures::future;
use futures::future::lazy;
use network::{CKBProtocolContext, CKBProtocolHandler, PeerIndex, TimerToken};
use pool::txs_pool::TransactionPool;
use std::sync::Arc;
use std::time::Duration;
use tokio;
use util::Mutex;
use AcceptBlockError;

pub const TX_PROPOSAL_TOKEN: TimerToken = 0;

pub struct Relayer<C> {
    pub chain: Arc<C>,
    pub pow: Arc<dyn PowEngine>,
    pub state: Arc<RelayState>,
    pub tx_pool: Arc<TransactionPool<C>>,
}

impl<C> Clone for Relayer<C>
where
    C: ChainProvider,
{
    fn clone(&self) -> Relayer<C> {
        Relayer {
            chain: Arc::clone(&self.chain),
            pow: Arc::clone(&self.pow),
            state: Arc::clone(&self.state),
            tx_pool: Arc::clone(&self.tx_pool),
        }
    }
}

impl<C> Relayer<C>
where
    C: ChainProvider + 'static,
{
    pub fn new(
        chain: &Arc<C>,
        pow: &Arc<dyn PowEngine>,
        tx_pool: &Arc<TransactionPool<C>>,
    ) -> Self {
        Relayer {
            chain: Arc::clone(chain),
            pow: Arc::clone(pow),
            state: Arc::new(RelayState::default()),
            tx_pool: Arc::clone(tx_pool),
        }
    }

    fn process(&self, nc: &CKBProtocolContext, peer: PeerIndex, message: RelayMessage) {
        match message.payload_type() {
            RelayPayload::CompactBlock => CompactBlockProcess::new(
                &message.payload_as_compact_block().unwrap(),
                self,
                peer,
                nc,
            ).execute(),
            RelayPayload::Transaction => {
                TransactionProcess::new(&message.payload_as_transaction().unwrap(), self, peer, nc)
                    .execute()
            }
            RelayPayload::GetBlockTransactions => GetBlockTransactionsProcess::new(
                &message.payload_as_get_block_transactions().unwrap(),
                self,
                peer,
                nc,
            ).execute(),
            RelayPayload::BlockTransactions => BlockTransactionsProcess::new(
                &message.payload_as_block_transactions().unwrap(),
                self,
                peer,
            ).execute(),
            RelayPayload::GetBlockProposal => GetBlockProposalProcess::new(
                &message.payload_as_get_block_proposal().unwrap(),
                self,
                peer,
                nc,
            ).execute(),
            RelayPayload::BlockProposal => {
                BlockProposalProcess::new(&message.payload_as_block_proposal().unwrap(), self)
                    .execute()
            }
            RelayPayload::NONE => {}
        }
    }

    pub fn request_proposal_txs(
        &self,
        nc: &CKBProtocolContext,
        peer: PeerIndex,
        block: &CompactBlock,
    ) {
        let mut inflight = self.state.inflight_proposals.lock();
        let unknown_ids = block
            .proposal_transactions
            .iter()
            .chain(
                block
                    .uncles
                    .iter()
                    .flat_map(|uncle| uncle.proposal_transactions()),
            ).filter(|x| !self.tx_pool.contains_key(x) && inflight.insert((*x).clone()))
            .cloned()
            .collect::<Vec<_>>();

        let fbb = &mut FlatBufferBuilder::new();
        let message =
            RelayMessage::build_get_block_proposal(fbb, block.header.number(), &unknown_ids);
        fbb.finish(message, None);

        let _ = nc.send(peer, fbb.finished_data().to_vec());
    }

    pub fn accept_block(&self, _peer: PeerIndex, block: &Block) -> Result<(), AcceptBlockError> {
        BlockVerifier::new(block, &self.chain, &self.pow).verify()?;
        self.chain.process_block(&block)?;
        Ok(())
    }

    pub fn reconstruct_block(
        &self,
        compact_block: &CompactBlock,
        transactions: Vec<Transaction>,
    ) -> (Option<Block>, Option<Vec<usize>>) {
        let (key0, key1) =
            short_transaction_id_keys(compact_block.header.nonce(), compact_block.nonce);

        let mut txs = transactions;
        txs.extend(self.tx_pool.get_potential_transactions());

        let mut txs_map = FnvHashMap::default();
        for tx in txs {
            let short_id = short_transaction_id(key0, key1, &tx.hash());
            txs_map.insert(short_id, tx);
        }

        let mut block_transactions = Vec::with_capacity(compact_block.short_ids.len());
        let mut missing_indexes = Vec::new();
        for (index, short_id) in compact_block.short_ids.iter().enumerate() {
            match txs_map.remove(short_id) {
                Some(tx) => block_transactions.insert(index, tx),
                None => missing_indexes.push(index),
            }
        }

        if missing_indexes.is_empty() {
            let block = BlockBuilder::default()
                .header(compact_block.header.clone())
                .uncles(compact_block.uncles.clone())
                .commit_transactions(block_transactions)
                .proposal_transactions(compact_block.proposal_transactions.clone())
                .build();

            (Some(block), None)
        } else {
            (None, Some(missing_indexes))
        }
    }

    fn prune_tx_proposal_request(&self, nc: &CKBProtocolContext) {
        let mut pending_proposals_request = self.state.pending_proposals_request.lock();
        let mut peer_txs = FnvHashMap::default();
        let mut remove_ids = Vec::new();

        for (id, peers) in pending_proposals_request.iter() {
            if let Some(tx) = self.tx_pool.get(id) {
                for peer in peers {
                    let mut tx_set = peer_txs.entry(*peer).or_insert_with(Vec::new);
                    tx_set.push(tx.clone());
                }
            }
            remove_ids.push(*id);
        }

        for id in remove_ids {
            pending_proposals_request.remove(&id);
        }

        for (peer, txs) in peer_txs {
            let fbb = &mut FlatBufferBuilder::new();
            let message = RelayMessage::build_block_proposal(
                fbb,
                &txs.into_iter().map(Into::into).collect::<Vec<_>>(),
            );
            fbb.finish(message, None);

            let _ = nc.send(peer, fbb.finished_data().to_vec());
        }
    }

    pub fn get_block(&self, hash: &H256) -> Option<Block> {
        self.chain.block(hash)
    }
}

impl<C> CKBProtocolHandler for Relayer<C>
where
    C: ChainProvider + 'static,
{
    fn initialize(&self, nc: Box<CKBProtocolContext>) {
        let _ = nc.register_timer(TX_PROPOSAL_TOKEN, Duration::from_millis(100));
    }

    fn received(&self, nc: Box<CKBProtocolContext>, peer: PeerIndex, data: &[u8]) {
        let data = data.to_owned();
        let relayer = self.clone();
        tokio::spawn(lazy(move || {
            // TODO use flatbuffers verifier
            let msg = get_root::<RelayMessage>(&data);
            debug!(target: "relay", "msg {:?}", msg.payload_type());
            relayer.process(nc.as_ref(), peer, msg);
            future::ok(())
        }));
    }

    fn connected(&self, _nc: Box<CKBProtocolContext>, peer: PeerIndex) {
        info!(target: "sync", "peer={} RelayProtocol.connected", peer);
        // do nothing
    }

    fn disconnected(&self, _nc: Box<CKBProtocolContext>, peer: PeerIndex) {
        info!(target: "sync", "peer={} RelayProtocol.disconnected", peer);
        // TODO
    }

    fn timer_triggered(&self, nc: Box<CKBProtocolContext>, token: TimerToken) {
        let relayer = self.clone();
        tokio::spawn(lazy(move || {
            match token as usize {
                TX_PROPOSAL_TOKEN => relayer.prune_tx_proposal_request(nc.as_ref()),
                _ => unreachable!(),
            }
            future::ok(())
        }));
    }
}

#[derive(Default)]
pub struct RelayState {
    // TODO add size limit or use bloom filter
    pub received_blocks: Mutex<FnvHashSet<H256>>,
    pub received_transactions: Mutex<FnvHashSet<H256>>,
    pub pending_compact_blocks: Mutex<FnvHashMap<H256, CompactBlock>>,
    pub inflight_proposals: Mutex<FnvHashSet<ProposalShortId>>,
    pub pending_proposals_request: Mutex<FnvHashMap<ProposalShortId, FnvHashSet<PeerIndex>>>,
}
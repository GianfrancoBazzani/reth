//! Implementation of [`BlockIndices`] related to [`super::BlockchainTree`]

use super::chain::{BlockChainId, Chain, ForkBlock};
use reth_primitives::{BlockHash, BlockNumber, SealedBlockWithSenders};
use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet};

/// Internal indices of the blocks and chains.  This is main connection
/// between blocks, chains and canonical chain.
///
/// It contains list of canonical block hashes, forks to childs blocks
/// and block hash to chain id.
pub struct BlockIndices {
    /// Last finalized block.
    last_finalized_block: BlockNumber,
    /// For EVM's "BLOCKHASH" opcode we require last 256 block hashes. So we need to specify
    /// at least `additional_canonical_block_hashes`+`max_reorg_depth`, for eth that would be
    /// 256+64.
    num_of_additional_canonical_block_hashes: u64,
    /// Canonical chain. Contains N number (depends on `finalization_depth`) of blocks.
    /// These blocks are found in fork_to_child but not inside `blocks_to_chain` or
    /// `number_to_block` as those are chain specific indices.
    canonical_chain: BTreeMap<BlockNumber, BlockHash>,
    /// Index needed when discarding the chain, so we can remove connected chains from tree.
    /// NOTE: It contains just a blocks that are forks as a key and not all blocks.
    fork_to_child: HashMap<BlockHash, HashSet<BlockHash>>,
    /// Block hashes and side chain they belong
    blocks_to_chain: HashMap<BlockHash, BlockChainId>,
    /// Utility index. Block number to block hash. Can be used for
    /// RPC to fetch all pending block in chain by its number.
    index_number_to_block: HashMap<BlockNumber, HashSet<BlockHash>>,
}

impl BlockIndices {
    /// Create new block indices structure
    pub fn new(
        last_finalized_block: BlockNumber,
        num_of_additional_canonical_block_hashes: u64,
        canonical_chain: BTreeMap<BlockNumber, BlockHash>,
    ) -> Self {
        Self {
            last_finalized_block,
            num_of_additional_canonical_block_hashes,
            fork_to_child: Default::default(),
            canonical_chain,
            blocks_to_chain: Default::default(),
            index_number_to_block: Default::default(),
        }
    }

    /// Return number of additional canonical block hashes that we need
    /// to have to be able to have enought information for EVM execution.
    pub fn num_of_additional_canonical_block_hashes(&self) -> u64 {
        self.num_of_additional_canonical_block_hashes
    }

    /// Return fork to child indices
    pub fn fork_to_child(&self) -> &HashMap<BlockHash, HashSet<BlockHash>> {
        &self.fork_to_child
    }

    /// Return block to chain id
    pub fn blocks_to_chain(&self) -> &HashMap<BlockHash, BlockChainId> {
        &self.blocks_to_chain
    }

    /// Returns `true` if the Tree knowns the block hash.
    pub fn contains_pending_block_hash(&self, block_hash: BlockHash) -> bool {
        self.blocks_to_chain.contains_key(&block_hash)
    }

    /// Check if block hash belongs to canonical chain.
    pub fn is_block_hash_canonical(&self, block_hash: &BlockHash) -> bool {
        self.canonical_chain.range(self.last_finalized_block..).any(|(_, &h)| h == *block_hash)
    }

    /// Last finalized block
    pub fn last_finalized_block(&self) -> BlockNumber {
        self.last_finalized_block
    }

    /// Insert non fork block.
    pub fn insert_non_fork_block(
        &mut self,
        block_number: BlockNumber,
        block_hash: BlockHash,
        chain_id: BlockChainId,
    ) {
        self.index_number_to_block.entry(block_number).or_default().insert(block_hash);
        self.blocks_to_chain.insert(block_hash, chain_id);
    }

    /// Insert block to chain and fork child indices of the new chain
    pub fn insert_chain(&mut self, chain_id: BlockChainId, chain: &Chain) {
        for (number, block) in chain.blocks().iter() {
            // add block -> chain_id index
            self.blocks_to_chain.insert(block.hash(), chain_id);
            // add number -> block
            self.index_number_to_block.entry(*number).or_default().insert(block.hash());
        }
        let first = chain.first();
        // add parent block -> block index
        self.fork_to_child.entry(first.parent_hash).or_default().insert(first.hash());
    }

    /// Get the chain ID the block belongs to
    pub fn get_blocks_chain_id(&self, block: &BlockHash) -> Option<BlockChainId> {
        self.blocks_to_chain.get(block).cloned()
    }

    /// Update all block hashes. iterate over present and new list of canonical hashes and compare
    /// them. Remove all missmatches, disconnect them and return all chains that needs to be
    /// removed.
    pub fn update_block_hashes(
        &mut self,
        hashes: BTreeMap<u64, BlockHash>,
    ) -> BTreeSet<BlockChainId> {
        let mut new_hashes = hashes.iter();
        let mut old_hashes = self.canonical_chain().clone().into_iter();

        let mut remove = Vec::new();

        let mut new_hash = new_hashes.next();
        let mut old_hash = old_hashes.next();

        loop {
            let Some(old_block_value) = old_hash else {
                // end of old_hashes canonical chain. New chain has more block then old chain.
                break
            };
            let Some(new_block_value) = new_hash else  {
                // Old canonical chain had more block than new chain.
                // remove all present block.
                // this is mostly not going to happen as reorg should make new chain in Tree.
                while let Some(rem) = old_hash {
                    remove.push(rem);
                    old_hash = old_hashes.next();
                }
                break;
            };
            // compare old and new canonical block number
            match new_block_value.0.cmp(&old_block_value.0) {
                std::cmp::Ordering::Less => {
                    // new chain has more past blocks than old chain
                    new_hash = new_hashes.next();
                }
                std::cmp::Ordering::Equal => {
                    if *new_block_value.1 != old_block_value.1 {
                        // remove block hash as it is different
                        remove.push(old_block_value);
                    }
                    new_hash = new_hashes.next();
                    old_hash = old_hashes.next();
                }
                std::cmp::Ordering::Greater => {
                    // old chain has more past blocks that new chain
                    remove.push(old_block_value);
                    old_hash = old_hashes.next()
                }
            }
        }
        self.canonical_chain = hashes;

        remove.into_iter().fold(BTreeSet::new(), |mut fold, (number, hash)| {
            fold.extend(self.remove_block(number, hash));
            fold
        })
    }

    /// Remove chain from indices and return dependent chains that needs to be removed.
    /// Does the cleaning of the tree and removing blocks from the chain.
    pub fn remove_chain(&mut self, chain: &Chain) -> BTreeSet<BlockChainId> {
        let mut lose_chains = BTreeSet::new();
        for (block_number, block) in chain.blocks().iter() {
            let block_hash = block.hash();
            lose_chains.extend(self.remove_block(*block_number, block_hash))
        }
        lose_chains
    }

    /// Remove Blocks from indices.
    fn remove_block(
        &mut self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> BTreeSet<BlockChainId> {
        // rm number -> block
        if let Entry::Occupied(mut entry) = self.index_number_to_block.entry(block_number) {
            let set = entry.get_mut();
            set.remove(&block_hash);
            // remove set if empty
            if set.is_empty() {
                entry.remove();
            }
        }

        // rm block -> chain_id
        self.blocks_to_chain.remove(&block_hash);

        // rm fork -> child
        let removed_fork = self.fork_to_child.remove(&block_hash);
        removed_fork
            .map(|fork_blocks| {
                fork_blocks
                    .into_iter()
                    .filter_map(|fork_child| self.blocks_to_chain.remove(&fork_child))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Remove all blocks from canonical list and insert new blocks to it.
    ///
    /// It is assumed that blocks are interconnected and that they connect to canonical chain
    pub fn canonicalize_blocks(&mut self, blocks: &BTreeMap<BlockNumber, SealedBlockWithSenders>) {
        if blocks.is_empty() {
            return
        }

        // Remove all blocks from canonical chain
        let first_number = *blocks.first_key_value().unwrap().0;

        // this will remove all blocks numbers that are going to be replaced.
        self.canonical_chain.retain(|num, _| *num < first_number);

        // remove them from block to chain_id index
        blocks.iter().map(|(_, b)| (b.number, b.hash(), b.parent_hash)).for_each(
            |(number, hash, parent_hash)| {
                // rm block -> chain_id
                self.blocks_to_chain.remove(&hash);

                // rm number -> block
                if let Entry::Occupied(mut entry) = self.index_number_to_block.entry(number) {
                    let set = entry.get_mut();
                    set.remove(&hash);
                    // remove set if empty
                    if set.is_empty() {
                        entry.remove();
                    }
                }
                // rm fork block -> hash
                if let Entry::Occupied(mut entry) = self.fork_to_child.entry(parent_hash) {
                    let set = entry.get_mut();
                    set.remove(&hash);
                    // remove set if empty
                    if set.is_empty() {
                        entry.remove();
                    }
                }
            },
        );

        // insert new canonical
        self.canonical_chain.extend(blocks.iter().map(|(number, block)| (*number, block.hash())))
    }

    /// Used for finalization of block.
    /// Return list of chains for removal that depend on finalized canonical chain.
    pub fn finalize_canonical_blocks(
        &mut self,
        finalized_block: BlockNumber,
    ) -> BTreeSet<BlockChainId> {
        // get finalized chains. blocks between [self.last_finalized,finalized_block).
        // Dont remove finalized_block, as sidechain can point to it.
        let finalized_blocks: Vec<BlockHash> = self
            .canonical_chain
            .iter()
            .filter(|(&number, _)| number >= self.last_finalized_block && number < finalized_block)
            .map(|(_, hash)| *hash)
            .collect();

        // remove unneeded canonical hashes.
        let remove_until =
            finalized_block.saturating_sub(self.num_of_additional_canonical_block_hashes);
        self.canonical_chain.retain(|&number, _| number >= remove_until);

        let mut lose_chains = BTreeSet::new();

        for block_hash in finalized_blocks.into_iter() {
            // there is a fork block.
            if let Some(fork_blocks) = self.fork_to_child.remove(&block_hash) {
                lose_chains = fork_blocks.into_iter().fold(lose_chains, |mut fold, fork_child| {
                    if let Some(lose_chain) = self.blocks_to_chain.remove(&fork_child) {
                        fold.insert(lose_chain);
                    }
                    fold
                });
            }
        }

        // set last finalized block.
        self.last_finalized_block = finalized_block;

        lose_chains
    }

    /// get canonical hash
    pub fn canonical_hash(&self, block_number: &BlockNumber) -> Option<BlockHash> {
        self.canonical_chain.get(block_number).cloned()
    }

    /// get canonical tip
    pub fn canonical_tip(&self) -> ForkBlock {
        let (&number, &hash) =
            self.canonical_chain.last_key_value().expect("There is always the canonical chain");
        ForkBlock { number, hash }
    }

    /// Canonical chain needs for execution of EVM. It should contains last 256 block hashes.
    pub fn canonical_chain(&self) -> &BTreeMap<BlockNumber, BlockHash> {
        &self.canonical_chain
    }
}

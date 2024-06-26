use crate::rollup_lib::{BaseRollupState, ExecutionEngine, SignedTransaction, TransactionData};
use alloy_primitives::{Address as AlloyAddress, U256};
use kinode_process_lib::{get_blob, get_typed_state, http, set_state};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::str::FromStr;

const DEFAULT_QUEUE_RESPONSE_TIMEOUT_SECONDS: u8 = 1;
const DEFAULT_SERVE_TIMEOUT_SECONDS: u16 = 60;
const DEFAULT_MAX_OUTSTANDING_PAYMENTS: u8 = 3;
const DEFAULT_PAYMENT_PERIOD_HOURS: u8 = 24;

/// Current on-chain state of DAO
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DaoState {
    pub routers: Vec<String>,  // length 1 for now
    pub members: HashMap<String, AlloyAddress>,
    pub proposals: HashMap<u64, ProposalInProgress>,
    // pub client_blacklist: Vec<String>,
    // pub member_blacklist: Vec<String>,
    pub queue_response_timeout_seconds: u8,
    pub serve_timeout_seconds: u16, // TODO
    pub max_outstanding_payments: u8,
    pub payment_period_hours: u8,
}

/// Possible changes to on-chain DAO state:
/// * Proposing changes to state
/// * Voting on changes to state
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum DaoTransaction {
    Propose(Proposal),
    Vote { item: u64, vote: SignedVote },
    Join(AlloyAddress),
    // Payment: TODO
    // * from clients to treasury for work done
    // * to providers from treasury for work done
    //   * should payout to providers be subject to vote?
    //   * ideally it should be provable and then Just Work
}

/// Possible proposals
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Proposal {
    ChangeRootNode(String),
    ChangeQueueResponseTimeoutSeconds(u8),
    ChangeMaxOutstandingPayments(u8),
    ChangePaymentPeriodHours(u8),
    Kick(String),
}

/// Possible proposals
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProposalInProgress {
    pub proposal: Proposal,
    pub votes: HashMap<String, SignedVote>,
}

/// A vote on a proposal
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Vote {
    pub proposal_hash: u64,
    pub is_yea: bool,
}

/// A signed vote on a proposal
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SignedVote {
    vote: Vote,
    signature: u64,
}

pub type FullRollupState = BaseRollupState<DaoState, DaoTransaction>;

impl Hash for Proposal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Proposal::ChangeRootNode(node) => {
                0.hash(state);
                node.hash(state);
            }
            Proposal::ChangeQueueResponseTimeoutSeconds(timeout) => {
                1.hash(state);
                timeout.hash(state);
            }
            Proposal::ChangeMaxOutstandingPayments(max) => {
                2.hash(state);
                max.hash(state);
            }
            Proposal::ChangePaymentPeriodHours(period) => {
                3.hash(state);
                period.hash(state);
            }
            Proposal::Kick(node) => {
                4.hash(state);
                node.hash(state);
            }
        }
    }
}

impl Default for FullRollupState {
    fn default() -> Self {
        Self {
            sequenced: vec![],
            balances: HashMap::new(),
            nonces: HashMap::new(),
            withdrawals: vec![],
            batches: vec![],
            l1_block: U256::ZERO,
            state: DaoState {
                routers: vec![],
                members: HashMap::new(),
                proposals: HashMap::new(),
                queue_response_timeout_seconds: DEFAULT_QUEUE_RESPONSE_TIMEOUT_SECONDS,
                serve_timeout_seconds: DEFAULT_SERVE_TIMEOUT_SECONDS,
                max_outstanding_payments: DEFAULT_MAX_OUTSTANDING_PAYMENTS,
                payment_period_hours: DEFAULT_PAYMENT_PERIOD_HOURS,
            },
        }
    }
}

/// This is where all of the business logic for the rollup lives.
/// The `execute` function is called by the sequencer to process a single transaction.
impl ExecutionEngine<DaoTransaction> for FullRollupState {
    // process a single transaction
    fn execute(
        &mut self,
        stx: SignedTransaction<DaoTransaction>,
        node: Option<String>,
    ) -> anyhow::Result<()> {
        let decode_stx = stx.clone();

        // DO NOT verify a signature for a bridge transaction
        if let TransactionData::BridgeTokens { amount, block } = decode_stx.tx.data {
            self.balances.insert(
                stx.pub_key.clone(),
                self.balances.get(&stx.pub_key).unwrap_or(&U256::ZERO) + amount,
            );
            self.l1_block = block;
            return Ok(());
        }

        if decode_stx.tx.nonce != *self.nonces.get(&stx.pub_key).unwrap_or(&U256::ZERO) {
            return Err(anyhow::anyhow!("bad nonce"));
        }

        // verify the signature
        if decode_stx
            .sig
            // TODO json doesn't (de)serialize deterministically. Alternatively, use ETH RLP?
            .recover_address_from_msg(&serde_json::to_string(&decode_stx.tx).unwrap().as_bytes())
            .unwrap()
            != decode_stx.pub_key
        {
            return Err(anyhow::anyhow!("bad sig"));
        }

        self.nonces
            .insert(stx.pub_key.clone(), decode_stx.tx.nonce + U256::from(1));

        // TODO check for underflows everywhere
        match decode_stx.tx.data {
            TransactionData::BridgeTokens { .. } => Err(anyhow::anyhow!("shouldn't happen")),
            TransactionData::WithdrawTokens(amount) => {
                if self.balances.get(&stx.pub_key).unwrap() < &amount {
                    return Err(anyhow::anyhow!("insufficient funds"));
                }

                self.balances.insert(
                    stx.pub_key.clone(),
                    self.balances.get(&stx.pub_key).unwrap_or(&U256::ZERO) - amount,
                );
                self.withdrawals.push((stx.pub_key, amount));
                Ok(())
            }
            TransactionData::Transfer { from, to, amount } => {
                if self.balances.get(&from).unwrap() < &amount {
                    return Err(anyhow::anyhow!("insufficient funds"));
                }

                self.balances.insert(
                    from.clone(),
                    self.balances.get(&from).unwrap_or(&U256::ZERO) - amount,
                );
                self.balances.insert(
                    to.clone(),
                    self.balances.get(&to).unwrap_or(&U256::ZERO) + amount,
                );
                Ok(())
            }
            // TransactionData::Extension includes the business logic for the rollup
            TransactionData::Extension(ext) => {
                match ext {
                    DaoTransaction::Propose(proposal) => {
                        let mut hasher = DefaultHasher::new();
                        proposal.hash(&mut hasher);
                        let hash = hasher.finish();
                        if self.state.proposals.contains_key(&hash) {
                            return Err(anyhow::anyhow!("proposal already exists"));
                        }
                        self.state.proposals.insert(hash, ProposalInProgress {
                            proposal,
                            votes: HashMap::new()
                        });
                    }
                    DaoTransaction::Vote { item, vote } => {
                        let node = node.ok_or_else(|| anyhow::anyhow!("only a Kinode may vote"))?;
                        if !self.state.proposals.contains_key(&item) {
                            return Err(anyhow::anyhow!("proposal does not exist"));
                        }
                        // TODO: confirm AlloyAddress sig
                        self.state.proposals.entry(item)
                            .and_modify(|proposal| {
                                proposal.votes.insert(node, vote);
                            });
                    }
                    DaoTransaction::Join(chain_address) => {
                        let node = node.ok_or_else(|| anyhow::anyhow!("can only add a Kinode"))?;
                        self.state.members.insert(node, chain_address);
                    }
                }
                Ok(())
            }
        }
    }

    // logic for saving our state to kinode sequencer
    // I would not modify this function, but you can if you require special logic
    // NOTE: normally I would use bincode but serde_json makes manual modification of the state much easier
    fn save(&self) -> anyhow::Result<()> {
        set_state(&serde_json::to_vec(&self).unwrap());
        Ok(())
    }

    // logic for loading our state from kinode sequencer
    // I would not modify this function, but you can if you require special logic
    fn load() -> Self
    where
        Self: Sized,
    {
        match get_typed_state(|bytes| Ok(serde_json::from_slice::<FullRollupState>(bytes)?)) {
            Some(rs) => rs,
            None => FullRollupState::default(),
        }
    }
}

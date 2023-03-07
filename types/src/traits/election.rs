//! The election trait, used to decide which node is the leader and determine if a vote is valid.
#![allow(clippy::missing_docs_in_private_items)]
#![allow(missing_docs)]

use super::node_implementation::{NodeImplementation, NodeType};
use super::signature_key::{EncodedPublicKey, EncodedSignature};
use crate::certificate::{DACertificate, QuorumCertificate};
use crate::data::DAProposal;
use crate::data::ProposalType;
use crate::data::ValidatingProposal;
use crate::message::ConsensusMessage;
use crate::message::Message;
use crate::traits::network::CommunicationChannel;
use crate::traits::network::NetworkMsg;
use crate::vote::VoteAccumulator;
use crate::vote::{DAVote, QuorumVote, TimeoutVote, VoteType, YesOrNoVote};
use crate::{data::LeafType, traits::signature_key::SignatureKey};
use bincode::Options;
use commit::{Commitment, Committable};
use either::Either;
use hotshot_utils::bincode::bincode_opts;
use nll::nll_todo::nll_todo;
use serde::Deserialize;
use serde::{de::DeserializeOwned, Serialize};
use snafu::Snafu;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;
use std::num::NonZeroU64;
/// Error for election problems
#[derive(Snafu, Debug)]
pub enum ElectionError {
    /// stub error to be filled in
    StubError,
    /// Math error doing something
    /// NOTE: it would be better to make Election polymorphic over
    /// the election error and then have specific math errors
    MathError,
}

/// For items that will always have the same validity outcome on a successful check,
/// allows for the case of "not yet possible to check" where the check might be
/// attempted again at a later point in time, but saves on repeated checking when
/// the outcome is already knowable.
///
/// This would be a useful general utility.
pub enum Checked<T> {
    /// This item has been checked, and is valid
    Valid(T),
    /// This item has been checked, and is not valid
    Inval(T),
    /// This item has not been checked
    Unchecked(T),
}

/// Data to vote on for different types of votes.
#[derive(Serialize)]
pub enum VoteData<TYPES: NodeType, LEAF: LeafType> {
    DA(Commitment<TYPES::BlockType>),
    Yes(Commitment<LEAF>),
    No(Commitment<LEAF>),
    Timeout(TYPES::Time),
}

impl<TYPES: NodeType, LEAF: LeafType> VoteData<TYPES, LEAF> {
    /// Convert vote data into bytes.
    ///
    /// # Panics
    /// Panics if the serialization fails.
    pub fn as_bytes(&self) -> Vec<u8> {
        bincode_opts().serialize(&self).unwrap()
    }
}

/// Proof of this entity's right to vote, and of the weight of those votes
pub trait VoteToken:
    Clone
    + Debug
    + Send
    + Sync
    + serde::Serialize
    + for<'de> serde::Deserialize<'de>
    + PartialEq
    + Hash
    + Committable
{
    // type StakeTable;
    // type KeyPair: SignatureKey;
    // type ConsensusTime: ConsensusTime;

    /// the count, which validation will confirm
    fn vote_count(&self) -> NonZeroU64;
}

/// election config
pub trait ElectionConfig:
    Default + Clone + Serialize + DeserializeOwned + Sync + Send + core::fmt::Debug
{
}

pub trait SignedCertificate<SIGNATURE: SignatureKey, TIME, TOKEN, LEAF>
where
    Self: Send + Sync + Clone + Serialize + for<'a> Deserialize<'a>,
    LEAF: Committable,
{
    /// Build a QC from the threshold signature and commitment
    fn from_signatures_and_commitment(
        view_number: TIME,
        signatures: BTreeMap<EncodedPublicKey, (EncodedSignature, TOKEN)>,
        commit: Commitment<LEAF>,
    ) -> Self;

    /// Get the view number.
    fn view_number(&self) -> TIME;

    /// Get signatures.
    fn signatures(&self) -> BTreeMap<EncodedPublicKey, (EncodedSignature, TOKEN)>;

    // TODO (da) the following functions should be refactored into a QC-specific trait.

    // Get the leaf commitment.
    fn leaf_commitment(&self) -> Commitment<LEAF>;

    // Set the leaf commitment.
    fn set_leaf_commitment(&mut self, commitment: Commitment<LEAF>);

    /// Get whether the certificate is for the genesis block.
    fn is_genesis(&self) -> bool;

    /// To be used only for generating the genesis quorum certificate; will fail if used anywhere else
    fn genesis() -> Self;
}

pub trait Membership<TYPES: NodeType>: Clone + Eq + PartialEq + Send + Sync + 'static {
    type StakeTable: Send + Sync;

    /// generate a default election configuration
    fn default_election_config(num_nodes: u64) -> TYPES::ElectionConfigType;

    /// create an election
    /// TODO may want to move this to a testableelection trait
    fn create_election(keys: Vec<TYPES::SignatureKey>, config: TYPES::ElectionConfigType) -> Self;

    /// Returns the table from the current committed state
    fn get_stake_table(
        &self,
        view_number: TYPES::Time,
        state: &TYPES::StateType,
    ) -> Self::StakeTable;

    fn get_leader(&self, view_number: TYPES::Time) -> TYPES::SignatureKey;

    fn get_committee(&self, view_number: TYPES::Time) -> BTreeSet<TYPES::SignatureKey>;

    /// Attempts to generate a vote token for self
    ///
    /// Returns `None` if the number of seats would be zero
    /// # Errors
    /// TODO tbd
    fn make_vote_token(
        &self,
        view_number: TYPES::Time,
        priv_key: &<TYPES::SignatureKey as SignatureKey>::PrivateKey,
    ) -> Result<Option<TYPES::VoteTokenType>, ElectionError>;

    /// Checks the claims of a received vote token
    ///
    /// # Errors
    /// TODO tbd
    fn validate_vote_token(
        &self,
        view_number: TYPES::Time,
        pub_key: TYPES::SignatureKey,
        token: Checked<TYPES::VoteTokenType>,
    ) -> Result<Checked<TYPES::VoteTokenType>, ElectionError>;

    /// Returns the threshold for a specific `Membership` implementation
    fn threshold(&self) -> NonZeroU64;
}

pub trait ConsensusExchange<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>, M: NetworkMsg>:
    Send + Sync
{
    type Proposal: ProposalType<NodeType = TYPES>;
    type Vote: VoteType<TYPES>;
    type Certificate: SignedCertificate<TYPES::SignatureKey, TYPES::Time, TYPES::VoteTokenType, LEAF>
        + Hash
        + Eq;
    // type VoteAccumulator: Accumulator<TYPES::VoteTokenType, LEAF>;
    type Membership: Membership<TYPES>;
    type Networking: CommunicationChannel<TYPES, M, Self::Proposal, Self::Vote, Self::Membership>;

    fn create(keys: Vec<TYPES::SignatureKey>, config: TYPES::ElectionConfigType) -> Self;

    fn network(&self) -> &Self::Networking;
    fn get_leader(&self, view_number: TYPES::Time) -> TYPES::SignatureKey {
        self.membership().get_leader(view_number)
    }
    fn is_leader(&self, view_number: TYPES::Time) -> bool {
        &self.get_leader(view_number) == self.public_key()
    }
    fn threshold(&self) -> NonZeroU64 {
        self.membership().threshold()
    }

    fn make_vote_token(
        &self,
        view_number: TYPES::Time,
    ) -> std::result::Result<std::option::Option<TYPES::VoteTokenType>, ElectionError>;
    /// Validate a QC.
    fn is_valid_cert<C: Committable>(&self, qc: &Self::Certificate, commit: Commitment<C>) -> bool;

    /// Validate a vote.
    fn is_valid_vote(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        data: VoteData<TYPES, LEAF>,
        view_number: TYPES::Time,
        vote_token: Checked<TYPES::VoteTokenType>,
    ) -> bool;

    /// Add a vote to the accumulating signature.  Return The certificate if the vote
    /// brings us over the threshould, Else return the accumulator.
    fn accumulate_vote<C: Committable>(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        leaf_commitment: Commitment<C>,
        vote_token: TYPES::VoteTokenType,
        view_number: TYPES::Time,
        accumlator: VoteAccumulator<TYPES::VoteTokenType, C>,
    ) -> Either<VoteAccumulator<TYPES::VoteTokenType, C>, Self::Certificate>;

    fn membership(&self) -> Self::Membership;
    fn public_key(&self) -> &TYPES::SignatureKey;

    // TODO (DA): Move vote related functions back to ConsensusExchange trait once it is implemented.
    // fn is_valid_dac(
    //     &self,
    //     dac: &<I::Leaf as LeafType>::DACertificate,
    //     block_commitment: Commitment<TYPES::BlockType>,
    // ) -> bool {
    //     let stake = dac
    //         .signatures()
    //         .iter()
    //         .filter(|signature| {
    //             self.is_valid_vote(
    //                 signature.0,
    //                 &signature.1 .0,
    //                 VoteData::DA(block_commitment),
    //                 dac.view_number(),
    //                 Checked::Unchecked(signature.1 .1.clone()),
    //             )
    //         })
    //         .fold(0, |acc, x| (acc + u64::from(x.1 .1.vote_count())));

    //     stake >= u64::from(self.threshold())
    // }

    // /// Validate a QC by checking its votes.
    // fn is_valid_qc(&self, qc: &<I::Leaf as LeafType>::QuorumCertificate) -> bool {
    //     if qc.is_genesis() && qc.view_number() == TYPES::Time::genesis() {
    //         return true;
    //     }
    //     let leaf_commitment = qc.leaf_commitment();

    //     let stake = qc
    //         .signatures()
    //         .iter()
    //         .filter(|signature| {
    //             self.is_valid_vote(
    //                 signature.0,
    //                 &signature.1 .0,
    //                 VoteData::Yes(leaf_commitment),
    //                 qc.view_number(),
    //                 Checked::Unchecked(signature.1 .1.clone()),
    //             )
    //         })
    //         .fold(0, |acc, x| (acc + u64::from(x.1 .1.vote_count())));

    //     stake >= u64::from(self.threshold())
    // }

    // /// Validate a vote by checking its signature and token.
    // fn is_valid_vote(
    //     &self,
    //     encoded_key: &EncodedPublicKey,
    //     encoded_signature: &EncodedSignature,
    //     data: VoteData<TYPES, I::Leaf>,
    //     view_number: TYPES::Time,
    //     vote_token: Checked<TYPES::VoteTokenType>,
    // ) -> bool {
    //     let mut is_valid_vote_token = false;
    //     let mut is_valid_signature = false;
    //     if let Some(key) = <TYPES::SignatureKey as SignatureKey>::from_bytes(encoded_key) {
    //         is_valid_signature = key.validate(encoded_signature, &data.as_bytes());
    //         let valid_vote_token =
    //             self.inner
    //                 .membership
    //                 .validate_vote_token(view_number, key, vote_token);
    //         is_valid_vote_token = match valid_vote_token {
    //             Err(_) => {
    //                 error!("Vote token was invalid");
    //                 false
    //             }
    //             Ok(Checked::Valid(_)) => true,
    //             Ok(Checked::Inval(_) | Checked::Unchecked(_)) => false,
    //         };
    //     }
    //     is_valid_signature && is_valid_vote_token
    // }
    // fn accumulate_vote<C: Committable, Cert>(
    //     &self,
    //     vota_meta: VoteMetaData<TYPES, C, TYPES::VoteTokenType, TYPES::Time, I::Leaf>,
    //     accumulator: VoteAccumulator<TYPES, C>,
    // ) -> Either<VoteAccumulator<TYPES, C>, Cert>
    // where
    //     Cert: SignedCertificate<TYPES::SignatureKey, TYPES::Time, TYPES::VoteTokenType, C>,
    // {
    //     if !self.is_valid_vote(
    //         &vota_meta.encoded_key,
    //         &vota_meta.encoded_signature,
    //         vota_meta.data,
    //         vota_meta.view_number,
    //         // Ignoring deserialization errors below since we are getting rid of it soon
    //         Checked::Unchecked(vota_meta.vote_token.clone()),
    //     ) {
    //         return Either::Left(accumulator);
    //     }

    //     match accumulator.append((
    //         vota_meta.commitment,
    //         (
    //             vota_meta.encoded_key.clone(),
    //             (vota_meta.encoded_signature.clone(), vota_meta.vote_token),
    //         ),
    //     )) {
    //         Either::Left(accumulator) => Either::Left(accumulator),
    //         Either::Right(signatures) => Either::Right(Cert::from_signatures_and_commitment(
    //             vota_meta.view_number,
    //             signatures,
    //             vota_meta.commitment,
    //         )),
    //     }
    // }

    // fn accumulate_qc_vote(
    //     &self,
    //     encoded_key: &EncodedPublicKey,
    //     encoded_signature: &EncodedSignature,
    //     leaf_commitment: Commitment<I::Leaf>,
    //     vote_token: TYPES::VoteTokenType,
    //     view_number: TYPES::Time,
    //     accumlator: VoteAccumulator<TYPES, I::Leaf>,
    // ) -> Either<VoteAccumulator<TYPES, I::Leaf>, QuorumCertificate<TYPES, I::Leaf>> {
    //     let meta = VoteMetaData {
    //         encoded_key: encoded_key.clone(),
    //         encoded_signature: encoded_signature.clone(),
    //         commitment: leaf_commitment,
    //         data: VoteData::Yes(leaf_commitment),
    //         vote_token,
    //         view_number,
    //     };
    //     self.accumulate_vote(meta, accumlator)
    // }
    // fn accumulate_da_vote(
    //     &self,
    //     encoded_key: &EncodedPublicKey,
    //     encoded_signature: &EncodedSignature,
    //     block_commitment: Commitment<TYPES::BlockType>,
    //     vote_token: TYPES::VoteTokenType,
    //     view_number: TYPES::Time,
    //     accumlator: VoteAccumulator<TYPES, TYPES::BlockType>,
    // ) -> Either<VoteAccumulator<TYPES, TYPES::BlockType>, DACertificate<TYPES>> {
    //     let meta = VoteMetaData {
    //         encoded_key: encoded_key.clone(),
    //         encoded_signature: encoded_signature.clone(),
    //         commitment: block_commitment,
    //         data: VoteData::DA(block_commitment),
    //         vote_token,
    //         view_number,
    //     };
    //     self.accumulate_vote(meta, accumlator)
    // }

    // async fn store_leaf(
    //     &self,
    //     old_anchor_view: TYPES::Time,
    //     leaf: I::Leaf,
    // ) -> std::result::Result<(), hotshot_types::traits::storage::StorageError> {
    //     let view_to_insert = StoredView::from(leaf);
    //     let storage = &self.inner.storage;
    //     storage.append_single_view(view_to_insert).await?;
    //     storage.cleanup_storage_up_to_view(old_anchor_view).await?;
    //     storage.commit().await?;
    //     Ok(())
    // }
}

pub trait CommitteeExchangeType<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>, M: NetworkMsg>:
    ConsensusExchange<TYPES, LEAF, M>
{
    fn sign_da_proposal(&self, block_commitment: &Commitment<TYPES::BlockType>)
        -> EncodedSignature;
    fn sign_da_vote(
        &self,
        block_commitment: Commitment<TYPES::BlockType>,
    ) -> (EncodedPublicKey, EncodedSignature);
    fn create_da_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
        block_commitment: Commitment<TYPES::BlockType>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> ConsensusMessage<TYPES, I>
    where
        I::ComitteeExchange:
            ConsensusExchange<TYPES, I::Leaf, Message<TYPES, I>, Vote = DAVote<TYPES, I::Leaf>>;
}
pub struct CommitteeExchange<
    TYPES: NodeType,
    LEAF: LeafType<NodeType = TYPES>,
    MEMBERSHIP: Membership<TYPES>,
    NETWORK: CommunicationChannel<TYPES, M, DAProposal<TYPES>, DAVote<TYPES, LEAF>, MEMBERSHIP>,
    M: NetworkMsg,
> {
    network: NETWORK,
    membership: MEMBERSHIP,
    public_key: TYPES::SignatureKey,
    private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    _pd: PhantomData<(TYPES, LEAF, MEMBERSHIP, M)>,
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        MEMBERSHIP: Membership<TYPES>,
        NETWORK: CommunicationChannel<TYPES, M, DAProposal<TYPES>, DAVote<TYPES, LEAF>, MEMBERSHIP>,
        M: NetworkMsg,
    > CommitteeExchangeType<TYPES, LEAF, M>
    for CommitteeExchange<TYPES, LEAF, MEMBERSHIP, NETWORK, M>
{
    /// Sign a DA proposal.
    fn sign_da_proposal(
        &self,
        block_commitment: &Commitment<TYPES::BlockType>,
    ) -> EncodedSignature {
        let signature = TYPES::SignatureKey::sign(&self.private_key, block_commitment.as_ref());
        signature
    }
    /// Sign a vote on DA proposal.
    ///
    /// The block commitment and the type of the vote (DA) are signed, which is the minimum amount
    /// of information necessary for checking that this node voted on that block.
    fn sign_da_vote(
        &self,
        block_commitment: Commitment<TYPES::BlockType>,
    ) -> (EncodedPublicKey, EncodedSignature) {
        let signature = TYPES::SignatureKey::sign(
            &self.private_key,
            &VoteData::<TYPES, LEAF>::DA(block_commitment).as_bytes(),
        );
        (self.public_key.to_bytes(), signature)
    }
    /// Create a message with a vote on DA proposal.
    fn create_da_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
        block_commitment: Commitment<TYPES::BlockType>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> ConsensusMessage<TYPES, I>
    where
        I::ComitteeExchange:
            ConsensusExchange<TYPES, I::Leaf, Message<TYPES, I>, Vote = DAVote<TYPES, I::Leaf>>,
    {
        let signature = self.sign_da_vote(block_commitment);
        ConsensusMessage::<TYPES, I>::DAVote(DAVote {
            justify_qc_commitment,
            signature,
            block_commitment,
            current_view,
            vote_token,
        })
    }
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        MEMBERSHIP: Membership<TYPES>,
        NETWORK: CommunicationChannel<TYPES, M, DAProposal<TYPES>, DAVote<TYPES, LEAF>, MEMBERSHIP>,
        M: NetworkMsg,
    > ConsensusExchange<TYPES, LEAF, M> for CommitteeExchange<TYPES, LEAF, MEMBERSHIP, NETWORK, M>
{
    type Proposal = DAProposal<TYPES>;
    type Vote = DAVote<TYPES, LEAF>;
    type Certificate = DACertificate<TYPES>;
    type Membership = MEMBERSHIP;
    type Networking = NETWORK;

    fn create(keys: Vec<TYPES::SignatureKey>, config: TYPES::ElectionConfigType) -> Self {
        let membership =
            <Self as ConsensusExchange<TYPES, LEAF, M>>::Membership::create_election(keys, config);
        nll_todo()
    }
    fn network(&self) -> &NETWORK {
        &self.network
    }
    fn make_vote_token(
        &self,
        view_number: TYPES::Time,
    ) -> std::result::Result<std::option::Option<TYPES::VoteTokenType>, ElectionError> {
        self.membership
            .make_vote_token(view_number, &self.private_key)
    }

    /// Validate a QC.
    fn is_valid_cert<C: Committable>(&self, qc: &Self::Certificate, commit: Commitment<C>) -> bool {
        nll_todo()
    }

    /// Validate a vote.
    fn is_valid_vote(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        data: VoteData<TYPES, LEAF>,
        view_number: TYPES::Time,
        vote_token: Checked<TYPES::VoteTokenType>,
    ) -> bool {
        nll_todo()
    }

    /// Add a vote to the accumulating signature.  Return The certificate if the vote
    /// brings us over the threshould, Else return the accumulator.
    fn accumulate_vote<C: Committable>(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        leaf_commitment: Commitment<C>,
        vote_token: TYPES::VoteTokenType,
        view_number: TYPES::Time,
        accumlator: VoteAccumulator<TYPES::VoteTokenType, C>,
    ) -> Either<VoteAccumulator<TYPES::VoteTokenType, C>, Self::Certificate> {
        nll_todo()
    }
    fn membership(&self) -> Self::Membership {
        nll_todo()
    }
    fn public_key(&self) -> &TYPES::SignatureKey {
        &self.public_key
    }
}

pub trait QuorumExchangeType<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>, M: NetworkMsg>:
    ConsensusExchange<TYPES, LEAF, M>
{
    /// Create a message with a positive vote on validating or commitment proposal.
    fn create_yes_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc_commitment: Commitment<Self::Certificate>,
        leaf_commitment: Commitment<LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> ConsensusMessage<TYPES, I>
    where
        <Self as ConsensusExchange<TYPES, LEAF, M>>::Certificate: commit::Committable,
        I::QuorumExchange:
            ConsensusExchange<TYPES, I::Leaf, Message<TYPES, I>, Vote = QuorumVote<TYPES, LEAF>>;
    /// Sign a validating or commitment proposal.
    fn sign_validating_or_commitment_proposal<I: NodeImplementation<TYPES>>(
        &self,
        leaf_commitment: &Commitment<LEAF>,
    ) -> EncodedSignature;

    /// Sign a positive vote on validating or commitment proposal.
    ///
    /// The leaf commitment and the type of the vote (yes) are signed, which is the minimum amount
    /// of information necessary for any user of the subsequently constructed QC to check that this
    /// node voted `Yes` on that leaf. The leaf is expected to be reconstructed based on other
    /// information in the yes vote.
    fn sign_yes_vote(
        &self,
        leaf_commitment: Commitment<LEAF>,
    ) -> (EncodedPublicKey, EncodedSignature);

    /// Sign a neagtive vote on validating or commitment proposal.
    ///
    /// The leaf commitment and the type of the vote (no) are signed, which is the minimum amount
    /// of information necessary for any user of the subsequently constructed QC to check that this
    /// node voted `No` on that leaf.
    fn sign_no_vote(
        &self,
        leaf_commitment: Commitment<LEAF>,
    ) -> (EncodedPublicKey, EncodedSignature);

    /// Sign a timeout vote.
    ///
    /// We only sign the view number, which is the minimum amount of information necessary for
    /// checking that this node timed out on that view.
    ///
    /// This also allows for the high QC included with the vote to be spoofed in a MITM scenario,
    /// but it is outside our threat model.
    fn sign_timeout_vote(&self, view_number: TYPES::Time) -> (EncodedPublicKey, EncodedSignature);
    /// Create a message with a negative vote on validating or commitment proposal.
    fn create_no_message<I: NodeImplementation<TYPES>>(
        &self,
        justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
        leaf_commitment: Commitment<LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> ConsensusMessage<TYPES, I>
    where
        I::QuorumExchange:
            ConsensusExchange<TYPES, I::Leaf, Message<TYPES, I>, Vote = QuorumVote<TYPES, LEAF>>;

    /// Create a message with a timeout vote on validating or commitment proposal.
    fn create_timeout_message<I: NodeImplementation<TYPES>>(
        &self,
        justify_qc: QuorumCertificate<TYPES, LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> ConsensusMessage<TYPES, I>
    where
        I::QuorumExchange:
            ConsensusExchange<TYPES, I::Leaf, Message<TYPES, I>, Vote = QuorumVote<TYPES, LEAF>>;
}
pub struct QuorumExchange<
    TYPES: NodeType,
    LEAF: LeafType<NodeType = TYPES>,
    MEMBERSHIP: Membership<TYPES>,
    NETWORK: CommunicationChannel<
        TYPES,
        M,
        ValidatingProposal<TYPES, LEAF>,
        QuorumVote<TYPES, LEAF>,
        MEMBERSHIP,
    >,
    M: NetworkMsg,
> {
    network: NETWORK,
    membership: MEMBERSHIP,
    public_key: TYPES::SignatureKey,
    private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    _pd: PhantomData<(LEAF, MEMBERSHIP, M)>,
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        MEMBERSHIP: Membership<TYPES>,
        NETWORK: CommunicationChannel<
            TYPES,
            M,
            ValidatingProposal<TYPES, LEAF>,
            QuorumVote<TYPES, LEAF>,
            MEMBERSHIP,
        >,
        M: NetworkMsg,
    > QuorumExchangeType<TYPES, LEAF, M> for QuorumExchange<TYPES, LEAF, MEMBERSHIP, NETWORK, M>
{
    /// Create a message with a positive vote on validating or commitment proposal.
    fn create_yes_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
        leaf_commitment: Commitment<LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> ConsensusMessage<TYPES, I>
    where
        I::QuorumExchange:
            ConsensusExchange<TYPES, I::Leaf, Message<TYPES, I>, Vote = QuorumVote<TYPES, LEAF>>,
    {
        let signature = self.sign_yes_vote(leaf_commitment);
        ConsensusMessage::<TYPES, I>::Vote(QuorumVote::Yes(YesOrNoVote {
            justify_qc_commitment,
            signature,
            leaf_commitment,
            current_view,
            vote_token,
        }))
    }
    /// Sign a validating or commitment proposal.
    fn sign_validating_or_commitment_proposal<I: NodeImplementation<TYPES>>(
        &self,
        leaf_commitment: &Commitment<LEAF>,
    ) -> EncodedSignature {
        let signature = TYPES::SignatureKey::sign(&self.private_key, leaf_commitment.as_ref());
        signature
    }

    /// Sign a positive vote on validating or commitment proposal.
    ///
    /// The leaf commitment and the type of the vote (yes) are signed, which is the minimum amount
    /// of information necessary for any user of the subsequently constructed QC to check that this
    /// node voted `Yes` on that leaf. The leaf is expected to be reconstructed based on other
    /// information in the yes vote.
    fn sign_yes_vote(
        &self,
        leaf_commitment: Commitment<LEAF>,
    ) -> (EncodedPublicKey, EncodedSignature) {
        let signature = TYPES::SignatureKey::sign(
            &self.private_key,
            &VoteData::<TYPES, LEAF>::Yes(leaf_commitment).as_bytes(),
        );
        (self.public_key.to_bytes(), signature)
    }

    /// Sign a neagtive vote on validating or commitment proposal.
    ///
    /// The leaf commitment and the type of the vote (no) are signed, which is the minimum amount
    /// of information necessary for any user of the subsequently constructed QC to check that this
    /// node voted `No` on that leaf.
    fn sign_no_vote(
        &self,
        leaf_commitment: Commitment<LEAF>,
    ) -> (EncodedPublicKey, EncodedSignature) {
        let signature = TYPES::SignatureKey::sign(
            &self.private_key,
            &VoteData::<TYPES, LEAF>::No(leaf_commitment).as_bytes(),
        );
        (self.public_key.to_bytes(), signature)
    }

    /// Sign a timeout vote.
    ///
    /// We only sign the view number, which is the minimum amount of information necessary for
    /// checking that this node timed out on that view.
    ///
    /// This also allows for the high QC included with the vote to be spoofed in a MITM scenario,
    /// but it is outside our threat model.
    fn sign_timeout_vote(&self, view_number: TYPES::Time) -> (EncodedPublicKey, EncodedSignature) {
        let signature = TYPES::SignatureKey::sign(
            &self.private_key,
            &VoteData::<TYPES, LEAF>::Timeout(view_number).as_bytes(),
        );
        (self.public_key.to_bytes(), signature)
    }
    /// Create a message with a negative vote on validating or commitment proposal.
    fn create_no_message<I: NodeImplementation<TYPES>>(
        &self,
        justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
        leaf_commitment: Commitment<LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> ConsensusMessage<TYPES, I>
    where
        I::QuorumExchange:
            ConsensusExchange<TYPES, I::Leaf, Message<TYPES, I>, Vote = QuorumVote<TYPES, LEAF>>,
    {
        let signature = self.sign_no_vote(leaf_commitment);
        ConsensusMessage::<TYPES, I>::Vote(QuorumVote::No(YesOrNoVote {
            justify_qc_commitment,
            signature,
            leaf_commitment,
            current_view,
            vote_token,
        }))
    }

    /// Create a message with a timeout vote on validating or commitment proposal.
    fn create_timeout_message<I: NodeImplementation<TYPES>>(
        &self,
        justify_qc: QuorumCertificate<TYPES, LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> ConsensusMessage<TYPES, I>
    where
        I::QuorumExchange:
            ConsensusExchange<TYPES, I::Leaf, Message<TYPES, I>, Vote = QuorumVote<TYPES, LEAF>>,
    {
        let signature = self.sign_timeout_vote(current_view);
        ConsensusMessage::<TYPES, I>::Vote(QuorumVote::Timeout(TimeoutVote {
            justify_qc,
            signature,
            current_view,
            vote_token,
        }))
    }
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        MEMBERSHIP: Membership<TYPES>,
        NETWORK: CommunicationChannel<
            TYPES,
            M,
            ValidatingProposal<TYPES, LEAF>,
            QuorumVote<TYPES, LEAF>,
            MEMBERSHIP,
        >,
        M: NetworkMsg,
    > ConsensusExchange<TYPES, LEAF, M> for QuorumExchange<TYPES, LEAF, MEMBERSHIP, NETWORK, M>
{
    type Proposal = ValidatingProposal<TYPES, LEAF>;
    type Vote = QuorumVote<TYPES, LEAF>;
    type Certificate = QuorumCertificate<TYPES, LEAF>;
    type Membership = MEMBERSHIP;
    type Networking = NETWORK;

    fn create(keys: Vec<TYPES::SignatureKey>, config: TYPES::ElectionConfigType) -> Self {
        let membership =
            <Self as ConsensusExchange<TYPES, LEAF, M>>::Membership::create_election(keys, config);
        nll_todo()
    }

    fn network(&self) -> &NETWORK {
        &self.network
    }

    fn make_vote_token(
        &self,
        view_number: TYPES::Time,
    ) -> std::result::Result<std::option::Option<TYPES::VoteTokenType>, ElectionError> {
        self.membership
            .make_vote_token(view_number, &self.private_key)
    }

    /// Validate a QC.
    fn is_valid_cert<C: Committable>(&self, qc: &Self::Certificate, commit: Commitment<C>) -> bool {
        nll_todo()
    }

    /// Validate a vote.
    fn is_valid_vote(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        data: VoteData<TYPES, LEAF>,
        view_number: TYPES::Time,
        vote_token: Checked<TYPES::VoteTokenType>,
    ) -> bool {
        nll_todo()
    }

    /// Add a vote to the accumulating signature.  Return The certificate if the vote
    /// brings us over the threshould, Else return the accumulator.
    fn accumulate_vote<C: Committable>(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        leaf_commitment: Commitment<C>,
        vote_token: TYPES::VoteTokenType,
        view_number: TYPES::Time,
        accumlator: VoteAccumulator<TYPES::VoteTokenType, C>,
    ) -> Either<VoteAccumulator<TYPES::VoteTokenType, C>, Self::Certificate> {
        nll_todo()
    }
    fn membership(&self) -> Self::Membership {
        nll_todo()
    }
    fn public_key(&self) -> &TYPES::SignatureKey {
        &self.public_key
    }
}

/// Testable implementation of an [`Election`]. Will expose a method to generate a vote token used for testing.
pub trait TestableElection<TYPES: NodeType>: Membership<TYPES> {
    /// Generate a vote token used for testing.
    fn generate_test_vote_token() -> TYPES::VoteTokenType;
}

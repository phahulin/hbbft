//! # A Cryptographic Common Coin
//!
//! The Common Coin produces a pseudorandom binary value that the correct nodes agree on, and that
//! cannot be known beforehand.
//!
//! Every Common Coin instance has a _nonce_ that determines the value, without giving it away: It
//! is not feasible to compute the output from the nonce alone, and the output is uniformly
//! distributed.
//!
//! The nodes input a signal (no data, just `()`), and after _2 f + 1_ nodes have provided input,
//! everyone receives the output value. In particular, the adversary cannot know the output value
//! before at least one correct node has provided input.
//!
//! ## How it works
//!
//! The algorithm uses a threshold signature scheme with the uniqueness property: For each public
//! key and message, there is exactly one valid signature. This group signature is produced using
//! signature shares from any combination of _2 f + 1_ secret key share holders.
//!
//! * On input, a node signs the nonce and sends its signature share to everyone else.
//! * When a node has received _2 f + 1_ shares, it computes the main signature and outputs the XOR
//! of its bits.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Debug;
use std::sync::Arc;

use crypto::error as cerror;
use crypto::{Signature, SignatureShare};
use fault_log::{FaultKind, FaultLog};
use messaging::{DistAlgorithm, NetworkInfo, Step, Target, TargetedMessage};

error_chain! {
    links {
        Crypto(cerror::Error, cerror::ErrorKind);
    }

    errors {
        UnknownSender {
            description("unknown sender")
        }
        VerificationFailed {
            description("signature verification failed")
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Rand)]
pub struct CommonCoinMessage(SignatureShare);

impl CommonCoinMessage {
    pub fn new(sig: SignatureShare) -> Self {
        CommonCoinMessage(sig)
    }

    pub fn to_sig(&self) -> &SignatureShare {
        &self.0
    }
}

/// A common coin algorithm instance. On input, broadcasts our threshold signature share. Upon
/// receiving at least `num_faulty + 1` shares, attempts to combine them into a signature. If that
/// signature is valid, the instance outputs it and terminates; otherwise the instance aborts.
#[derive(Debug)]
pub struct CommonCoin<NodeUid, T> {
    netinfo: Arc<NetworkInfo<NodeUid>>,
    /// The name of this common coin. It is required to be unique for each common coin round.
    nonce: T,
    /// The result of combination of at least `num_faulty + 1` threshold signature shares.
    output: Option<bool>,
    /// Outgoing message queue.
    messages: VecDeque<CommonCoinMessage>,
    /// All received threshold signature shares.
    received_shares: BTreeMap<NodeUid, SignatureShare>,
    /// Whether we provided input to the common coin.
    had_input: bool,
    /// Termination flag.
    terminated: bool,
}

pub type CommonCoinStep<NodeUid> = Step<NodeUid, bool>;

impl<NodeUid, T> DistAlgorithm for CommonCoin<NodeUid, T>
where
    NodeUid: Clone + Debug + Ord,
    T: Clone + AsRef<[u8]>,
{
    type NodeUid = NodeUid;
    type Input = ();
    type Output = bool;
    type Message = CommonCoinMessage;
    type Error = Error;

    /// Sends our threshold signature share if not yet sent.
    fn input(&mut self, _input: Self::Input) -> Result<CommonCoinStep<NodeUid>> {
        let fault_log = if !self.had_input {
            self.had_input = true;
            self.get_coin()?
        } else {
            FaultLog::new()
        };
        self.step(fault_log)
    }

    /// Receives input from a remote node.
    fn handle_message(
        &mut self,
        sender_id: &Self::NodeUid,
        message: Self::Message,
    ) -> Result<CommonCoinStep<NodeUid>> {
        let fault_log = if !self.terminated {
            let CommonCoinMessage(share) = message;
            self.handle_share(sender_id, share)?
        } else {
            FaultLog::new()
        };
        self.step(fault_log)
    }

    /// Takes the next share of a threshold signature message for multicasting to all other nodes.
    fn next_message(&mut self) -> Option<TargetedMessage<Self::Message, Self::NodeUid>> {
        self.messages
            .pop_front()
            .map(|msg| Target::All.message(msg))
    }

    /// Whether the algorithm has terminated.
    fn terminated(&self) -> bool {
        self.terminated
    }

    fn our_id(&self) -> &Self::NodeUid {
        self.netinfo.our_uid()
    }
}

impl<NodeUid, T> CommonCoin<NodeUid, T>
where
    NodeUid: Clone + Debug + Ord,
    T: Clone + AsRef<[u8]>,
{
    pub fn new(netinfo: Arc<NetworkInfo<NodeUid>>, nonce: T) -> Self {
        CommonCoin {
            netinfo,
            nonce,
            output: None,
            messages: VecDeque::new(),
            received_shares: BTreeMap::new(),
            had_input: false,
            terminated: false,
        }
    }

    fn step(&mut self, fault_log: FaultLog<NodeUid>) -> Result<CommonCoinStep<NodeUid>> {
        Ok(Step::new(
            self.output.take().into_iter().collect(),
            fault_log,
        ))
    }

    fn get_coin(&mut self) -> Result<FaultLog<NodeUid>> {
        if !self.netinfo.is_validator() {
            self.try_output()?;
            return Ok(FaultLog::new());
        }
        let share = self.netinfo.secret_key_share().sign(&self.nonce);
        self.messages.push_back(CommonCoinMessage(share.clone()));
        let id = self.netinfo.our_uid().clone();
        self.handle_share(&id, share)
    }

    fn handle_share(
        &mut self,
        sender_id: &NodeUid,
        share: SignatureShare,
    ) -> Result<FaultLog<NodeUid>> {
        if let Some(pk_i) = self.netinfo.public_key_share(sender_id) {
            if !pk_i.verify(&share, &self.nonce) {
                // Log the faulty node and ignore the invalid share.
                let fault_kind = FaultKind::UnverifiedSignatureShareSender;
                let fault_log = FaultLog::init(sender_id.clone(), fault_kind);
                return Ok(fault_log);
            }
            self.received_shares.insert(sender_id.clone(), share);
        } else {
            return Err(ErrorKind::UnknownSender.into());
        }
        self.try_output()?;
        Ok(FaultLog::new())
    }

    fn try_output(&mut self) -> Result<()> {
        let received_shares = &self.received_shares;
        debug!(
            "{:?} received {} shares, had_input = {}",
            self.netinfo.our_uid(),
            received_shares.len(),
            self.had_input
        );
        if self.had_input && received_shares.len() > self.netinfo.num_faulty() {
            let sig = self.combine_and_verify_sig()?;
            // Output the parity of the verified signature.
            let parity = sig.parity();
            debug!("{:?} output {}", self.netinfo.our_uid(), parity);
            self.output = Some(parity);
            self.terminated = true;
        }
        Ok(())
    }

    fn combine_and_verify_sig(&self) -> Result<Signature> {
        // Pass the indices of sender nodes to `combine_signatures`.
        let ids_shares: BTreeMap<&NodeUid, &SignatureShare> = self.received_shares.iter().collect();
        let ids_u64: BTreeMap<&NodeUid, u64> = ids_shares
            .keys()
            .map(|&id| (id, self.netinfo.node_index(id).unwrap() as u64))
            .collect();
        // Convert indices to `u64` which is an interface type for `pairing`.
        let shares: BTreeMap<&u64, &SignatureShare> = ids_shares
            .iter()
            .map(|(id, &share)| (&ids_u64[id], share))
            .collect();
        let sig = self.netinfo.public_key_set().combine_signatures(shares)?;
        if !self
            .netinfo
            .public_key_set()
            .public_key()
            .verify(&sig, &self.nonce)
        {
            // Abort
            error!(
                "{:?} main public key verification failed",
                self.netinfo.our_uid()
            );
            Err(ErrorKind::VerificationFailed.into())
        } else {
            Ok(sig)
        }
    }
}

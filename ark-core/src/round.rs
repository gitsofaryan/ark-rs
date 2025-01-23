use crate::asp::Tree;
use crate::conversions::from_zkp_xonly;
use crate::conversions::to_zkp_pk;
use crate::forfeit_fee::compute_forfeit_min_relay_fee;
use crate::internal_node::VtxoTreeInternalNodeScript;
use crate::BoardingOutput;
use crate::DefaultVtxo;
use crate::Error;
use crate::ErrorContext;
use crate::VTXO_INPUT_INDEX;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::secp256k1::PublicKey;
use bitcoin::sighash::Prevouts;
use bitcoin::sighash::SighashCache;
use bitcoin::taproot;
use bitcoin::transaction;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::TapLeafHash;
use bitcoin::TapSighashType;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;
use bitcoin::XOnlyPublicKey;
use rand::CryptoRng;
use rand::Rng;
use std::collections::BTreeMap;
use zkp::new_musig_nonce_pair;
use zkp::MusigAggNonce;
use zkp::MusigKeyAggCache;
use zkp::MusigPartialSignature;
use zkp::MusigPubNonce;
use zkp::MusigSecNonce;
use zkp::MusigSession;
use zkp::MusigSessionId;

/// A UTXO that is primed to become a VTXO. Alternatively, the owner of this UTXO may decide to
/// spend it into a vanilla UTXO.
///
/// Only UTXOs with a particular script (involving an Ark server) can become VTXOs.
#[derive(Debug, Clone)]
pub struct OnChainInput {
    /// The information needed to spend the UTXO.
    ///
    /// This does not include the amount, because the Ark server will provide that during the
    /// process of signing the round transaction where this UTXO is used as an input.
    boarding_output: BoardingOutput,
    /// The location of this UTXO in the blockchain.
    outpoint: OutPoint,
}

impl OnChainInput {
    pub fn new(boarding_output: BoardingOutput, outpoint: OutPoint) -> Self {
        Self {
            boarding_output,
            outpoint,
        }
    }

    pub fn boarding_output(&self) -> &BoardingOutput {
        &self.boarding_output
    }

    pub fn outpoint(&self) -> OutPoint {
        self.outpoint
    }
}

/// Either a confirmed VTXO that needs to be refreshed, or an unconfirmed VTXO that needs
/// confirmation.
///
/// Alternatively, the owner of this VTXO may decide to spend it into a vanilla UTXO.
#[derive(Debug, Clone)]
pub struct VtxoInput {
    /// The information needed to spend the VTXO, besides the amount.
    ///
    /// TODO: Eventually we will support VTXOs beyond [`DefaultVtxo`].
    vtxo: DefaultVtxo,
    /// The amount of coins locked in the VTXO.
    amount: Amount,
    /// Where the VTXO would end up on the blockchain if it were to become a UTXO.
    outpoint: OutPoint,
}

impl VtxoInput {
    pub fn new(vtxo: DefaultVtxo, amount: Amount, outpoint: OutPoint) -> Self {
        Self {
            vtxo,
            amount,
            outpoint,
        }
    }

    pub fn outpoint(&self) -> OutPoint {
        self.outpoint
    }

    pub fn vtxo(&self) -> &DefaultVtxo {
        &self.vtxo
    }
}

/// A nonce key pair per shared internal (non-leaf) node in the VTXO tree.
///
/// The [`MusigSecNonce`] element of the tuple is an [`Option`] because it cannot be cloned or
/// copied. We use the [`Option`] to move it into the [`NonceTree`] during nonce generation, and out
/// of the [`NonceTree`] when signing the VTXO tree.
pub struct NonceTree(Vec<Vec<(Option<MusigSecNonce>, MusigPubNonce)>>);

impl NonceTree {
    /// Take ownership of the [`MusigSecNonce`] at level `i` and branch `j` in the tree.
    ///
    /// The caller must take ownership because the [`MusigSecNonce`] ensures that it can only be
    /// used once, to avoid nonce reuse.
    pub fn take_sk(&mut self, i: usize, j: usize) -> Option<MusigSecNonce> {
        self.0
            .get_mut(i)
            .and_then(|level| level.get_mut(j).map(|(sec, _)| sec.take()))
            .flatten()
    }

    /// Convert into a tree of public nonces.
    pub fn to_pub_nonce_tree(&self) -> PubNonceTree {
        let pub_nonce_tree = self
            .0
            .iter()
            .map(|level| {
                level
                    .iter()
                    .map(|(_, pub_nonce)| *pub_nonce)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        PubNonceTree(pub_nonce_tree)
    }
}

/// A public nonce per shared internal (non-leaf) node in the VTXO tree.
pub struct PubNonceTree(Vec<Vec<MusigPubNonce>>);

impl PubNonceTree {
    /// Get the [`MusigPubNonce`] at level `i` and branch `j` in the tree.
    pub fn get(&self, i: usize, j: usize) -> Option<MusigPubNonce> {
        self.0.get(i).and_then(|level| level.get(j)).copied()
    }

    /// Get the underlying matrix of [`MusigPubNonce`]s.
    pub fn into_inner(self) -> Vec<Vec<MusigPubNonce>> {
        self.0
    }
}

impl From<Vec<Vec<MusigPubNonce>>> for PubNonceTree {
    fn from(value: Vec<Vec<MusigPubNonce>>) -> Self {
        Self(value)
    }
}

/// Generate a nonce pair for each internal (non-leaf) node in the VTXO tree.
pub fn generate_nonce_tree<R>(
    rng: &mut R,
    unsigned_vtxo_tree: &Tree,
    ephemeral_pk: PublicKey,
) -> Result<NonceTree, Error>
where
    R: Rng + CryptoRng,
{
    let secp_zkp = zkp::Secp256k1::new();

    let nonce_tree = unsigned_vtxo_tree
        .levels
        .iter()
        .map(|level| {
            level
                .nodes
                .iter()
                .map(|_| {
                    let session_id = MusigSessionId::new(rng);
                    let extra_rand = rng.gen();

                    // TODO: Revisit nonce generation, because this is something
                    // that we could mess up in a non-obvious way.
                    let (nonce, pub_nonce) = new_musig_nonce_pair(
                        &secp_zkp,
                        session_id,
                        None,
                        None,
                        to_zkp_pk(ephemeral_pk),
                        None,
                        Some(extra_rand),
                    )
                    .map_err(Error::crypto)?;

                    Ok((Some(nonce), pub_nonce))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(NonceTree(nonce_tree))
}

/// A Musig partial signature per shared internal (non-leaf) node in the VTXO tree.
pub struct PartialSigTree(Vec<Vec<MusigPartialSignature>>);

impl PartialSigTree {
    /// Get the underlying matrix of [`MusigPartialSignature`]s.
    pub fn into_inner(self) -> Vec<Vec<MusigPartialSignature>> {
        self.0
    }
}

/// Sign each shared internal (non-leaf) node of the VTXO tree with `ephemeral_kp` and using
/// `our_nonce_tree` to provide our share of each aggregate nonce.
#[allow(clippy::too_many_arguments)]
pub fn sign_vtxo_tree(
    round_lifetime: bitcoin::Sequence,
    asp_pk: XOnlyPublicKey,
    // This is a cosigner key pair AFAIK.
    ephemeral_kp: &Keypair,
    vtxo_tree: &Tree,
    round_tx: &Psbt,
    cosigner_pks: Vec<PublicKey>,
    mut our_nonce_tree: NonceTree,
    aggregate_pub_nonce_tree: PubNonceTree,
) -> Result<PartialSigTree, Error> {
    let internal_node_script = VtxoTreeInternalNodeScript::new(round_lifetime, asp_pk);

    let secp = Secp256k1::new();
    let secp_zkp = zkp::Secp256k1::new();

    let mut cosigner_pks = cosigner_pks.into_iter().map(to_zkp_pk).collect::<Vec<_>>();
    cosigner_pks.sort_by_key(|k| k.serialize());

    let mut key_agg_cache = MusigKeyAggCache::new(&secp_zkp, &cosigner_pks);

    let sweep_tap_tree =
        internal_node_script.sweep_spend_leaf(&secp, from_zkp_xonly(key_agg_cache.agg_pk()));

    let tweak = zkp::SecretKey::from_slice(sweep_tap_tree.tap_tweak().as_byte_array())
        .expect("valid conversion");

    key_agg_cache
        .pubkey_xonly_tweak_add(&secp_zkp, tweak)
        .map_err(Error::crypto)?;

    let ephemeral_kp = zkp::Keypair::from_seckey_slice(&secp_zkp, &ephemeral_kp.secret_bytes())
        .expect("valid keypair");

    let mut partial_sig_tree: Vec<Vec<MusigPartialSignature>> = Vec::new();
    for (i, level) in vtxo_tree.levels.iter().enumerate() {
        let mut sigs_level = Vec::new();
        for (j, node) in level.nodes.iter().enumerate() {
            tracing::debug!(i, j, ?node, "Generating partial signature");

            let agg_pub_nonce = aggregate_pub_nonce_tree
                .get(i, j)
                .ok_or_else(|| Error::crypto(format!("missing pub nonce {i}, {j}")))?;

            // Equivalent to parsing the individual `MusigAggNonce` from a slice.
            let agg_nonce = MusigAggNonce::new(&secp_zkp, &[agg_pub_nonce]);

            let tx = &node.tx.unsigned_tx;

            // We expect a single input to a VTXO.
            let parent_txid = node.parent_txid;

            let input_vout = tx.input[VTXO_INPUT_INDEX].previous_output.vout as usize;

            let prevout = if i == 0 {
                round_tx.clone().unsigned_tx.output[input_vout].clone()
            } else {
                let parent_level = &vtxo_tree.levels[i - 1];
                let parent_tx = parent_level
                    .nodes
                    .iter()
                    .find_map(|node| {
                        (node.txid == parent_txid).then_some(node.tx.unsigned_tx.clone())
                    })
                    .ok_or(Error::crypto("missing parent for VTXO {i}, {j}"))?;

                parent_tx.output[input_vout].clone()
            };

            let prevouts = [prevout];
            let prevouts = Prevouts::All(&prevouts);

            // Here we are generating a key spend sighash, because the VTXO tree outputs are signed
            // by all parties with a VTXO in this new round, so we use a musig key spend to
            // efficiently coordinate all the parties.
            let tap_sighash = SighashCache::new(tx)
                .taproot_key_spend_signature_hash(
                    VTXO_INPUT_INDEX,
                    &prevouts,
                    TapSighashType::Default,
                )
                .map_err(Error::crypto)?;

            let msg = zkp::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

            let nonce_sk = our_nonce_tree
                .take_sk(i, j)
                .ok_or(Error::crypto("missing nonce {i}, {j}"))?;

            let sig = MusigSession::new(&secp_zkp, &key_agg_cache, agg_nonce, msg)
                .partial_sign(&secp_zkp, nonce_sk, &ephemeral_kp, &key_agg_cache)
                .map_err(Error::crypto)?;

            sigs_level.push(sig);
        }

        partial_sig_tree.push(sigs_level);
    }

    Ok(PartialSigTree(partial_sig_tree))
}

/// Build and sign a forfeit transaction per [`VtxoInput`] to be used in an upcoming round
/// transaction.
pub fn create_and_sign_forfeit_txs(
    // For now we only support a single keypair. Eventually we may need to provide something like a
    // `Sign` trait, so that the caller can find the secret key for the given `VtxoInput`.
    kp: &Keypair,
    vtxo_inputs: &[VtxoInput],
    connector_psbts: Vec<Psbt>,
    min_relay_fee_rate_sats_per_kvb: i64,
    server_forfeit_address: &Address,
    // As defined by the server.
    dust: Amount,
) -> Result<Vec<Psbt>, Error> {
    const FORFEIT_TX_CONNECTOR_INDEX: usize = 0;
    const FORFEIT_TX_VTXO_INDEX: usize = 1;

    let secp = Secp256k1::new();

    let fee_rate_sats_per_kvb = min_relay_fee_rate_sats_per_kvb as u64;
    let connector_amount = dust;

    let mut signed_forfeit_psbts = Vec::new();
    for VtxoInput {
        vtxo,
        amount: vtxo_amount,
        outpoint: vtxo_outpoint,
    } in vtxo_inputs.iter()
    {
        let min_relay_fee =
            compute_forfeit_min_relay_fee(fee_rate_sats_per_kvb, vtxo, server_forfeit_address);

        let mut connector_inputs = Vec::new();
        for connector_psbt in connector_psbts.iter() {
            let txid = connector_psbt.unsigned_tx.compute_txid();
            for (i, connector_output) in connector_psbt.unsigned_tx.output.iter().enumerate() {
                if connector_output.value == connector_amount {
                    connector_inputs.push((
                        OutPoint {
                            txid,
                            vout: i as u32,
                        },
                        connector_output,
                    ));
                }
            }
        }

        let forfeit_output = TxOut {
            value: *vtxo_amount + connector_amount - min_relay_fee,
            script_pubkey: server_forfeit_address.script_pubkey(),
        };

        let mut forfeit_psbts = Vec::new();
        // It seems like we are signing multiple forfeit transactions per VTXO i.e. it seems
        // like the ASP will be able to publish different versions of the same forfeit
        // transaction. This might be useful because it gives the ASP more flexibility?
        for (connector_outpoint, connector_output) in connector_inputs.into_iter() {
            let mut forfeit_psbt = Psbt::from_unsigned_tx(Transaction {
                version: transaction::Version::TWO,
                lock_time: LockTime::ZERO,
                input: vec![
                    TxIn {
                        previous_output: connector_outpoint,
                        ..Default::default()
                    },
                    TxIn {
                        previous_output: *vtxo_outpoint,
                        ..Default::default()
                    },
                ],
                output: vec![forfeit_output.clone()],
            })
            .map_err(Error::transaction)?;

            forfeit_psbt.inputs[FORFEIT_TX_CONNECTOR_INDEX].witness_utxo =
                Some(connector_output.clone());

            forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].witness_utxo = Some(TxOut {
                value: *vtxo_amount,
                script_pubkey: vtxo.script_pubkey(),
            });

            forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].sighash_type =
                Some(TapSighashType::Default.into());

            forfeit_psbts.push(forfeit_psbt);
        }

        for forfeit_psbt in forfeit_psbts.iter_mut() {
            let (forfeit_script, forfeit_control_block) = vtxo.forfeit_spend_info();

            let leaf_version = forfeit_control_block.leaf_version;
            forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].tap_scripts = BTreeMap::from_iter([(
                forfeit_control_block,
                (forfeit_script.clone(), leaf_version),
            )]);

            let prevouts = forfeit_psbt
                .inputs
                .iter()
                .filter_map(|i| i.witness_utxo.clone())
                .collect::<Vec<_>>();
            let prevouts = Prevouts::All(&prevouts);

            let leaf_hash = TapLeafHash::from_script(&forfeit_script, leaf_version);

            let tap_sighash = SighashCache::new(&forfeit_psbt.unsigned_tx)
                .taproot_script_spend_signature_hash(
                    FORFEIT_TX_VTXO_INDEX,
                    &prevouts,
                    leaf_hash,
                    TapSighashType::Default,
                )
                .map_err(Error::crypto)?;

            let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

            let sig = secp.sign_schnorr_no_aux_rand(&msg, kp);
            let pk = kp.x_only_public_key().0;

            secp.verify_schnorr(&sig, &msg, &pk)
                .map_err(Error::crypto)
                .context("failed to verify own forfeit signature")?;

            let sig = taproot::Signature {
                signature: sig,
                sighash_type: TapSighashType::Default,
            };

            forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].tap_script_sigs =
                BTreeMap::from_iter([((pk, leaf_hash), sig)]);

            signed_forfeit_psbts.push(forfeit_psbt.clone());
        }
    }

    Ok(signed_forfeit_psbts)
}

/// Sign every input of the `round_psbt` which is in the provided `onchain_inputs` list.
pub fn sign_round_psbt<F>(
    sign_for_pk_fn: F,
    round_psbt: &mut Psbt,
    onchain_inputs: &[OnChainInput],
) -> Result<(), Error>
where
    F: Fn(&XOnlyPublicKey, &secp256k1::Message) -> Result<schnorr::Signature, Error>,
{
    let secp = Secp256k1::new();

    let prevouts = round_psbt
        .inputs
        .iter()
        .filter_map(|i| i.witness_utxo.clone())
        .collect::<Vec<_>>();

    // Sign round transaction inputs that belong to us. For every output we
    // are boarding, we look through the round transaction inputs to find a
    // matching input.
    for OnChainInput {
        boarding_output,
        outpoint: boarding_outpoint,
    } in onchain_inputs.iter()
    {
        let (forfeit_script, forfeit_control_block) = boarding_output.forfeit_spend_info();

        for (i, input) in round_psbt.inputs.iter_mut().enumerate() {
            let previous_outpoint = round_psbt.unsigned_tx.input[i].previous_output;

            if previous_outpoint == *boarding_outpoint {
                // In the case of a boarding output, we are actually using a
                // script spend path.

                let leaf_version = forfeit_control_block.leaf_version;
                input.tap_scripts = BTreeMap::from_iter([(
                    forfeit_control_block.clone(),
                    (forfeit_script.clone(), leaf_version),
                )]);

                let prevouts = Prevouts::All(&prevouts);

                let leaf_hash = TapLeafHash::from_script(&forfeit_script, leaf_version);

                let tap_sighash = SighashCache::new(&round_psbt.unsigned_tx)
                    .taproot_script_spend_signature_hash(
                        i,
                        &prevouts,
                        leaf_hash,
                        TapSighashType::Default,
                    )
                    .map_err(Error::crypto)?;

                let msg =
                    secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());
                let pk = boarding_output.owner_pk();

                let sig = sign_for_pk_fn(&pk, &msg)?;

                secp.verify_schnorr(&sig, &msg, &pk)
                    .map_err(Error::crypto)
                    .context("failed to verify own round TX signature")?;

                let sig = taproot::Signature {
                    signature: sig,
                    sighash_type: TapSighashType::Default,
                };

                input.tap_script_sigs = BTreeMap::from_iter([((pk, leaf_hash), sig)]);
            }
        }
    }

    Ok(())
}

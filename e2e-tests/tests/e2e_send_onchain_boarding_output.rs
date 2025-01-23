#![allow(clippy::unwrap_used)]

use ark_rs::wallet::BoardingWallet;
use bitcoin::address::NetworkUnchecked;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::SecretKey;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::thread_rng;
use std::str::FromStr;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn send_onchain_boarding_output() {
    init_tracing();

    // To be able to spend a boarding output it needs to have been confirmed for at least 604_672
    // seconds.
    let outpoint_blocktime_offset = 604_672 + 10;
    let nigiri = Arc::new(Nigiri::new(Some(outpoint_blocktime_offset)));

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let alice_key = SecretKey::new(&mut rng);
    let alice_keypair = Keypair::from_secret_key(&secp, &alice_key);

    let (alice, alice_wallet) = set_up_client(
        "alice".to_string(),
        alice_keypair,
        nigiri.clone(),
        secp.clone(),
    )
    .await;

    let alice_boarding_output = {
        let alice_asp_info = alice.asp_info.clone();
        let asp_pk = alice_asp_info.pk;
        let (asp_pk, _) = asp_pk.inner.x_only_public_key();

        alice_wallet
            .new_boarding_output(
                asp_pk,
                alice_asp_info.round_lifetime,
                &alice_asp_info.boarding_descriptor_template,
                alice_asp_info.network,
            )
            .unwrap()
    };

    let boarding_output = nigiri
        .faucet_fund(alice_boarding_output.address(), Amount::ONE_BTC)
        .await;

    tracing::debug!("Boarding output: {boarding_output:?}");

    let (tx, prevouts) = alice
        .create_send_on_chain_transaction(
            bitcoin::Address::<NetworkUnchecked>::from_str(
                "bcrt1q8df4sx3hz63tq44ve3q6tr4qz0q30usk5sntpt",
            )
            .unwrap()
            .assume_checked(),
            Amount::from_btc(0.7).unwrap(),
        )
        .await
        .unwrap();

    for (i, prevout) in prevouts.iter().enumerate() {
        let script_pubkey = prevout.script_pubkey.clone();
        let amount = prevout.value;
        let spent_outputs = prevouts
            .iter()
            .map(|o| bitcoinconsensus::Utxo {
                script_pubkey: o.script_pubkey.as_bytes().as_ptr(),
                script_pubkey_len: o.script_pubkey.len() as u32,
                value: o.value.to_sat() as i64,
            })
            .collect::<Vec<_>>();

        bitcoinconsensus::verify(
            script_pubkey.as_bytes(),
            amount.to_sat(),
            bitcoin::consensus::serialize(&tx).as_slice(),
            Some(&spent_outputs),
            i,
        )
        .unwrap();
    }
}

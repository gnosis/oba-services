use crate::encoding::EncodedSettlement;
use anyhow::{Context, Result};
use contracts::GPv2Settlement;
use ethcontract::{batch::CallBatch, dyns::DynTransport, transaction::TransactionBuilder};
use futures::FutureExt;
use shared::Web3;

const SIMULATE_BATCH_SIZE: usize = 10;

/// Simulate the settlement using a web3 `call`.
// Clippy claims we don't need to collect `futures` but we do or the lifetimes with `join!` don't
// work out.
#[allow(clippy::needless_collect)]
pub async fn simulate_settlements(
    settlements: impl Iterator<Item = EncodedSettlement>,
    contract: &GPv2Settlement,
    web3: &Web3,
    network_id: &str,
) -> Result<Vec<Result<()>>> {
    let mut batch = CallBatch::new(web3.transport());
    let futures = settlements
        .map(|settlement| {
            let method =
                crate::settlement_submission::retry::settle_method_builder(contract, settlement);
            let transaction_builder = method.tx.clone();
            (method.view().batch_call(&mut batch), transaction_builder)
        })
        .collect::<Vec<_>>();

    // TODO: It would be nice to add this to the underlying web3 batch transport call used for the
    // simulations.
    let ((), current_block) = futures::join!(
        batch.execute_all(SIMULATE_BATCH_SIZE),
        web3.eth().block_number(),
    );

    let current_block = current_block
        .context("failed to get current block")?
        .as_u64();

    Ok(futures
        .into_iter()
        .map(|(future, transaction_builder)| {
            future
                .now_or_never()
                .unwrap()
                .map(|_| ())
                .context(tenderly_link(
                    current_block,
                    network_id,
                    transaction_builder,
                ))
        })
        .collect())
}

// Creates a simulation link in the gp-v2 tenderly workspace
pub fn tenderly_link(
    current_block: u64,
    network_id: &str,
    tx: TransactionBuilder<DynTransport>,
) -> String {
    format!(
        "https://dashboard.tenderly.co/gp-v2/staging/simulator/new?block={}&blockIndex=0&from={:#x}&gas=8000000&gasPrice=0&value=0&contractAddress={:#x}&rawFunctionInput=0x{}&network={}",
        current_block,
        tx.from.unwrap().address(),
        tx.to.unwrap(),
        hex::encode(tx.data.unwrap().0),
        network_id
    )
}

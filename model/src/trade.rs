//! Contains the Trade type as described by the specification with serialization as described by the openapi documentation.

use crate::order::OrderUid;
use num_bigint::BigUint;
use primitive_types::H160;
use serde::{Deserialize, Serialize};

#[derive(Eq, PartialEq, Clone, Debug, Deserialize, Serialize, Hash)]
pub struct Trade {
    pub block_number: u64,
    pub log_index: u64,
    pub order_uid: OrderUid,
    #[serde(with = "serde_with::rust::display_fromstr")]
    pub buy_amount: BigUint,
    #[serde(with = "serde_with::rust::display_fromstr")]
    pub sell_amount: BigUint,
    #[serde(with = "serde_with::rust::display_fromstr")]
    pub sell_amount_before_fees: BigUint,
    // ORDER DATA
    pub owner: H160,
    pub buy_token: H160,
    pub sell_token: H160,
}

impl Default for Trade {
    fn default() -> Trade {
        let order_uid = OrderUid::default();
        Trade {
            block_number: Default::default(),
            log_index: Default::default(),
            order_uid,
            buy_amount: Default::default(),
            sell_amount: Default::default(),
            sell_amount_before_fees: Default::default(),
            owner: Default::default(),
            buy_token: Default::default(),
            sell_token: Default::default(),
        }
    }
}
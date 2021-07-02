pub mod balancer;
pub mod offchain_orderbook;
pub mod slippage;
pub mod uniswap;

use crate::settlement::SettlementEncoder;
use anyhow::Result;
#[cfg(test)]
use model::order::Order;
use model::{order::OrderKind, TokenPair};
use num::{rational::Ratio, BigRational};
use primitive_types::{H160, U256};
use shared::balancer::pool_storage::PoolTokenState;
use std::collections::HashMap;
use std::sync::Arc;
use strum_macros::{AsStaticStr, EnumVariantNames};

/// Defines the different types of liquidity our solvers support
#[derive(Clone, AsStaticStr, EnumVariantNames, Debug)]
pub enum Liquidity {
    Limit(LimitOrder),
    ConstantProduct(ConstantProductOrder),
    WeightedProduct(WeightedProductOrder),
}

/// A trait associating some liquidity model to how it is executed and encoded
/// in a settlement (through a `SettlementHandling` reference). This allows
/// different liquidity types to be modeled the same way.
pub trait Settleable {
    type Execution;

    fn settlement_handling(&self) -> &dyn SettlementHandling<Self>;
}

/// Specifies how a liquidity execution gets encoded into a settlement.
pub trait SettlementHandling<L>: Send + Sync
where
    L: Settleable,
{
    fn encode(&self, execution: L::Execution, encoder: &mut SettlementEncoder) -> Result<()>;
}

/// Basic limit sell and buy orders
#[derive(Clone)]
pub struct LimitOrder {
    // Opaque Identifier for debugging purposes
    pub id: String,
    pub sell_token: H160,
    pub buy_token: H160,
    pub sell_amount: U256,
    pub buy_amount: U256,
    pub kind: OrderKind,
    pub partially_fillable: bool,
    pub fee_amount: U256,
    pub settlement_handling: Arc<dyn SettlementHandling<Self>>,
}

impl std::fmt::Debug for LimitOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Limit Order {}", self.id)
    }
}

impl LimitOrder {
    /// Returns the full execution amount for the specified limit order.
    pub fn full_execution_amount(&self) -> U256 {
        match self.kind {
            OrderKind::Sell => self.sell_amount,
            OrderKind::Buy => self.buy_amount,
        }
    }
}

impl Settleable for LimitOrder {
    type Execution = U256;

    fn settlement_handling(&self) -> &dyn SettlementHandling<Self> {
        &*self.settlement_handling
    }
}

#[cfg(test)]
impl From<Order> for LimitOrder {
    fn from(order: Order) -> Self {
        use self::offchain_orderbook::normalize_limit_order;
        use contracts::WETH9;
        use shared::dummy_contract;

        let native_token = dummy_contract!(WETH9, H160([0x42; 20]));
        normalize_limit_order(order, native_token)
    }
}

#[cfg(test)]
impl Default for LimitOrder {
    fn default() -> Self {
        LimitOrder {
            sell_token: Default::default(),
            buy_token: Default::default(),
            sell_amount: Default::default(),
            buy_amount: Default::default(),
            kind: Default::default(),
            partially_fillable: Default::default(),
            fee_amount: Default::default(),
            settlement_handling: tests::CapturingSettlementHandler::arc(),
            id: Default::default(),
        }
    }
}

/// 2 sided constant product automated market maker with equal reserve value and a trading fee (e.g. Uniswap, Sushiswap)
#[derive(Clone)]
pub struct ConstantProductOrder {
    pub tokens: TokenPair,
    pub reserves: (u128, u128),
    pub fee: Ratio<u32>,
    pub settlement_handling: Arc<dyn SettlementHandling<Self>>,
}

impl std::fmt::Debug for ConstantProductOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Constant Product AMM {:?}", self.tokens)
    }
}

/// 2 sided weighted product automated market maker with weighted reserves and a trading fee (e.g. BalancerV2)
#[derive(Clone)]
pub struct WeightedProductOrder {
    pub reserves: HashMap<H160, PoolTokenState>,
    pub fee: BigRational,
    pub settlement_handling: Arc<dyn SettlementHandling<Self>>,
}

impl std::fmt::Debug for WeightedProductOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Weighted Product AMM {:?}", self.reserves.keys())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmmOrderExecution {
    pub input: (H160, U256),
    pub output: (H160, U256),
}

impl ConstantProductOrder {
    pub fn constant_product(&self) -> U256 {
        U256::from(self.reserves.0) * U256::from(self.reserves.1)
    }
}

impl Settleable for ConstantProductOrder {
    type Execution = AmmOrderExecution;

    fn settlement_handling(&self) -> &dyn SettlementHandling<Self> {
        &*self.settlement_handling
    }
}

impl Settleable for WeightedProductOrder {
    type Execution = AmmOrderExecution;

    fn settlement_handling(&self) -> &dyn SettlementHandling<Self> {
        &*self.settlement_handling
    }
}

#[cfg(test)]
impl Default for ConstantProductOrder {
    fn default() -> Self {
        ConstantProductOrder {
            tokens: Default::default(),
            reserves: Default::default(),
            fee: num::Zero::zero(),
            settlement_handling: tests::CapturingSettlementHandler::arc(),
        }
    }
}

#[cfg(test)]
impl Default for WeightedProductOrder {
    fn default() -> Self {
        WeightedProductOrder {
            reserves: Default::default(),
            fee: num::Zero::zero(),
            settlement_handling: tests::CapturingSettlementHandler::arc(),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    pub struct CapturingSettlementHandler<L>
    where
        L: Settleable,
    {
        pub calls: Mutex<Vec<L::Execution>>,
    }

    // Manual implementation seems to be needed as `derive(Default)` adds an
    // unneeded `L::Execution: Default` type bound.
    impl<L> Default for CapturingSettlementHandler<L>
    where
        L: Settleable,
    {
        fn default() -> Self {
            Self {
                calls: Default::default(),
            }
        }
    }

    impl<L> CapturingSettlementHandler<L>
    where
        L: Settleable,
        L::Execution: Clone,
    {
        pub fn arc() -> Arc<Self> {
            Arc::new(Default::default())
        }

        pub fn calls(&self) -> Vec<L::Execution> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl<L> SettlementHandling<L> for CapturingSettlementHandler<L>
    where
        L: Settleable,
        L::Execution: Send + Sync,
    {
        fn encode(&self, execution: L::Execution, _: &mut SettlementEncoder) -> Result<()> {
            self.calls.lock().unwrap().push(execution);
            Ok(())
        }
    }

    #[test]
    fn limit_order_full_execution_amounts() {
        fn simple_limit_order(
            kind: OrderKind,
            sell_amount: impl Into<U256>,
            buy_amount: impl Into<U256>,
        ) -> LimitOrder {
            LimitOrder {
                id: Default::default(),
                sell_token: Default::default(),
                buy_token: Default::default(),
                sell_amount: sell_amount.into(),
                buy_amount: buy_amount.into(),
                kind,
                partially_fillable: Default::default(),
                fee_amount: Default::default(),
                settlement_handling: CapturingSettlementHandler::arc(),
            }
        }

        assert_eq!(
            simple_limit_order(OrderKind::Sell, 1, 2).full_execution_amount(),
            1.into(),
        );
        assert_eq!(
            simple_limit_order(OrderKind::Buy, 1, 2).full_execution_amount(),
            2.into(),
        );
    }
}

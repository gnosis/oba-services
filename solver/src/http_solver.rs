mod model;
mod settlement;

use self::{model::*, settlement::PreparedModel};
use crate::{
    liquidity::{AmmOrder, LimitOrder, Liquidity},
    settlement::Settlement,
    solver::Solver,
};
use ::model::order::OrderKind;
use anyhow::{ensure, Context, Result};
use primitive_types::H160;
use reqwest::{header::HeaderValue, Client, Url};
use std::collections::{HashMap, HashSet};

// TODO: limit trading for tokens that don't have uniswap - fee pool
// TODO: exclude partially fillable orders
// TODO: find correct ordering for uniswap trades
// TODO: special rounding for the prices we get from the solver?
// TODO: make sure to give the solver disconnected token islands individually

/// The configuration passed as url parameters to the solver.
#[derive(Debug, Default)]
pub struct SolverConfig {
    max_nr_exec_orders: u32,
    time_limit: u32,
    // TODO: add more parameters that we want to set
}

impl SolverConfig {
    fn add_to_query(&self, url: &mut Url) {
        url.query_pairs_mut()
            .append_pair(
                "max_nr_exec_orders",
                self.max_nr_exec_orders.to_string().as_str(),
            )
            .append_pair("time_limit", self.time_limit.to_string().as_str());
    }
}

pub struct HttpSolver {
    base: Url,
    client: Client,
    api_key: Option<String>,
    config: SolverConfig,
}

impl HttpSolver {
    pub fn new(base: Url, api_key: Option<String>, config: SolverConfig) -> Self {
        // Unwrap because we cannot handle client creation failing.
        let client = Client::builder().build().unwrap();
        Self {
            base,
            client,
            api_key,
            config,
        }
    }

    // Solver api requires specifying token as strings. We use the address as a string for now.
    // Later we could use a more meaningful name like the token symbol but we have to ensure
    // uniqueness.
    fn token_to_string(&self, token: &H160) -> String {
        // Token names must start with a letter.
        format!("t{:x}", token)
    }

    // Maps string based token index from solver api
    fn tokens(&self, orders: &[Liquidity]) -> HashMap<String, H160> {
        orders
            .iter()
            .flat_map(|liquidity| match liquidity {
                Liquidity::Limit(order) => {
                    std::iter::once(order.sell_token).chain(std::iter::once(order.buy_token))
                }
                Liquidity::Amm(amm) => {
                    std::iter::once(amm.tokens.get().0).chain(std::iter::once(amm.tokens.get().1))
                }
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|token| (self.token_to_string(&token), token))
            .collect()
    }

    // Maps string based token index from solver api
    fn token_models(&self, tokens: &HashMap<String, H160>) -> HashMap<String, TokenInfoModel> {
        // TODO: gather real decimals and store them in a cache
        tokens
            .iter()
            .map(|(index, _)| (index.clone(), TokenInfoModel { decimals: 18 }))
            .collect()
    }

    // Maps string based order index from solver api
    fn orders(&self, orders: Vec<LimitOrder>) -> HashMap<String, LimitOrder> {
        orders
            .into_iter()
            .enumerate()
            .map(|(index, order)| (index.to_string(), order))
            .collect()
    }

    // Maps string based order index from solver api
    fn order_models(&self, orders: &HashMap<String, LimitOrder>) -> HashMap<String, OrderModel> {
        orders
            .iter()
            .map(|(index, order)| {
                let order = OrderModel {
                    sell_token: self.token_to_string(&order.sell_token),
                    buy_token: self.token_to_string(&order.buy_token),
                    sell_amount: order.sell_amount,
                    buy_amount: order.buy_amount,
                    allow_partial_fill: order.partially_fillable,
                    is_sell_order: matches!(order.kind, OrderKind::Sell),
                };
                (index.clone(), order)
            })
            .collect()
    }

    // Maps string based amm index from solver api
    fn amms(&self, orders: Vec<AmmOrder>) -> HashMap<String, AmmOrder> {
        orders
            .into_iter()
            .enumerate()
            .map(|(index, amm)| (index.to_string(), amm))
            .collect()
    }

    // Maps string based amm index from solver api
    fn amm_models(&self, amms: &HashMap<String, AmmOrder>) -> HashMap<String, UniswapModel> {
        amms.iter()
            .map(|(index, amm)| {
                let uniswap = UniswapModel {
                    token1: self.token_to_string(&amm.tokens.get().0),
                    token2: self.token_to_string(&amm.tokens.get().1),
                    balance1: amm.reserves.0,
                    balance2: amm.reserves.1,
                    fee: *amm.fee.numer() as f64 / *amm.fee.denom() as f64,
                    mandatory: false,
                };
                (index.clone(), uniswap)
            })
            .collect()
    }

    fn prepare_model(&self, liquidity: Vec<Liquidity>) -> PreparedModel {
        let tokens = self.tokens(liquidity.as_slice());
        let orders = split_liquidity(liquidity);
        let limit_orders = self.orders(orders.0);
        let amm_orders = self.amms(orders.1);
        let model = BatchAuctionModel {
            tokens: self.token_models(&tokens),
            orders: self.order_models(&limit_orders),
            uniswaps: self.amm_models(&amm_orders),
            default_fee: 0.0,
        };
        PreparedModel {
            model,
            tokens,
            limit_orders,
            amm_orders,
        }
    }

    async fn send(&self, model: &BatchAuctionModel) -> Result<SettledBatchAuctionModel> {
        let mut url = self.base.clone();
        url.set_path("/solve");
        self.config.add_to_query(&mut url);
        let query = url.query().map(ToString::to_string).unwrap_or_default();
        let mut request = self.client.post(url);
        if let Some(api_key) = &self.api_key {
            let mut header = HeaderValue::from_str(api_key.as_str()).unwrap();
            header.set_sensitive(true);
            request = request.header("X-API-KEY", header);
        }
        let body = serde_json::to_string(&model).context("failed to encode body")?;
        tracing::trace!("request {}", body);
        let request = request.body(body.clone());
        let response = request.send().await.context("failed to send request")?;
        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to decode response body")?;
        tracing::trace!("response {}", text);
        let context = || {
            format!(
                "request query {}, request body {}, response body {}",
                query, body, text
            )
        };
        ensure!(
            status.is_success(),
            "solver response is not success: status {}, {}",
            status,
            context()
        );
        serde_json::from_str(text.as_str())
            .with_context(|| format!("failed to decode response json, {}", context()))
    }
}

fn split_liquidity(liquidity: Vec<Liquidity>) -> (Vec<LimitOrder>, Vec<AmmOrder>) {
    let mut limit_orders = Vec::new();
    let mut amm_orders = Vec::new();
    for order in liquidity {
        match order {
            Liquidity::Limit(order) => limit_orders.push(order),
            Liquidity::Amm(order) => amm_orders.push(order),
        }
    }
    (limit_orders, amm_orders)
}

#[async_trait::async_trait]
impl Solver for HttpSolver {
    async fn solve(&self, liquidity: Vec<Liquidity>) -> Result<Option<Settlement>> {
        let prepared = self.prepare_model(liquidity);
        let settled = self.send(&prepared.model).await?;
        tracing::trace!(?settled);
        settlement::convert_settlement(&settled, &prepared).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::liquidity::{
        AmmOrder, LimitOrder, MockAmmSettlementHandling, MockLimitOrderSettlementHandling,
    };
    use ::model::TokenPair;
    use num::Rational;
    use std::sync::Arc;

    // cargo test real_solver -- --ignored --nocapture
    // set the env variable GP_V2_OPTIMIZER_URL to use a non localhost optimizer
    #[tokio::test]
    #[ignore]
    async fn real_solver() {
        tracing_subscriber::fmt::fmt()
            .with_env_filter("solver=trace")
            .init();
        let url = std::env::var("GP_V2_OPTIMIZER_URL")
            .unwrap_or_else(|_| "http://localhost:8000".to_string());
        let solver = HttpSolver::new(
            url.parse().unwrap(),
            None,
            SolverConfig {
                max_nr_exec_orders: 100,
                time_limit: 100,
            },
        );
        let base = |x: u128| x * 10u128.pow(18);
        let orders = vec![
            Liquidity::Limit(LimitOrder {
                buy_token: H160::zero(),
                sell_token: H160::from_low_u64_be(1),
                buy_amount: base(1).into(),
                sell_amount: base(2).into(),
                kind: OrderKind::Sell,
                partially_fillable: false,
                settlement_handling: Arc::new(MockLimitOrderSettlementHandling::new()),
            }),
            Liquidity::Amm(AmmOrder {
                tokens: TokenPair::new(H160::zero(), H160::from_low_u64_be(1)).unwrap(),
                reserves: (base(100), base(100)),
                fee: Rational::new(0, 1),
                settlement_handling: Arc::new(MockAmmSettlementHandling::new()),
            }),
        ];
        let prepared = solver.prepare_model(orders);
        let settled = solver.send(&prepared.model).await.unwrap();
        dbg!(&settled);

        let exec_order = settled.orders.values().next().unwrap();
        assert_eq!(exec_order.exec_sell_amount.as_u128(), base(2));
        assert!(exec_order.exec_buy_amount.as_u128() > 0);

        let uniswap = settled.uniswaps.values().next().unwrap();
        assert!(uniswap.balance_update1 < 0);
        assert_eq!(uniswap.balance_update2 as u128, base(2));

        assert_eq!(settled.prices.len(), 2);
    }
}

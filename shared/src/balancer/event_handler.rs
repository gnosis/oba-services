use crate::{
    current_block::BlockRetrieving,
    event_handling::{BlockNumber, EventHandler, EventIndex, EventStoring},
    impl_event_retrieving,
    maintenance::Maintaining,
};
use anyhow::{anyhow, Context, Result};
use contracts::{
    balancer_v2_vault::{
        self,
        event_data::{
            PoolRegistered as ContractPoolRegistered, TokensRegistered as ContractTokensRegistered,
        },
        Event as ContractEvent,
    },
    BalancerV2Vault,
};
use ethcontract::common::DeploymentInformation;
use ethcontract::{dyns::DynWeb3, Event as EthContractEvent, EventMetadata, H160, H256};
use model::TokenPair;
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    ops::RangeInclusive,
};
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub enum BalancerEvent {
    PoolRegistered(PoolRegistered),
    TokensRegistered(TokensRegistered),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolRegistered {
    pub pool_id: H256,
    pub pool_address: H160,
    pub specialization: PoolSpecialization,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TokensRegistered {
    pub pool_id: H256,
    pub tokens: Vec<H160>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RegisteredPool {
    pub pool_id: H256,
    pub pool_address: H160,
    pub specialization: PoolSpecialization,
    pub tokens: Vec<H160>,
    pub block_created: u64,
}

impl RegisteredPool {
    pub fn test_instance() -> Self {
        Self {
            pool_id: Default::default(),
            pool_address: Default::default(),
            specialization: PoolSpecialization::General,
            tokens: vec![],
            block_created: 0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WeightedPoolBuilder {
    pool_registration: Option<PoolRegistered>,
    tokens_registration: Option<TokensRegistered>,
    /// Both Pool and Tokens Registered events are emitted in the same transaction
    /// https://github.com/balancer-labs/balancer-v2-monorepo/blob/70843e6a61ad11208c1cfabf5cfe15be216ca8d3/pkg/pool-utils/contracts/BasePool.sol#L128-L130
    /// block_number is only contained in the EventIndex
    block_created: u64,
}

impl WeightedPoolBuilder {
    fn into_pool(self) -> Result<RegisteredPool> {
        if let (Some(pool_registration), Some(tokens_registration)) = (
            self.pool_registration.clone(),
            self.tokens_registration.clone(),
        ) {
            return Ok(RegisteredPool {
                pool_id: pool_registration.pool_id,
                pool_address: pool_registration.pool_address,
                tokens: tokens_registration.tokens,
                specialization: pool_registration.specialization,
                block_created: self.block_created,
            });
        }
        Err(anyhow!(
            "Expected pool and token registration events to be emitted together {:?}",
            self
        ))
    }
}

/// The BalancerPool struct represents in-memory storage of all deployed Balancer Pools
#[derive(Debug)]
pub struct PoolRegistry {
    /// Used for O(1) access to all pool_ids for a given token
    pools_by_token: HashMap<H160, HashSet<H256>>,
    /// WeightedPool data for a given PoolId
    pools: HashMap<H256, RegisteredPool>,
    /// Temporary storage for WeightedPools containing insufficient constructor data
    pending_pools: HashMap<H256, WeightedPoolBuilder>,
}

/// There are three specialization settings for Pools,
/// which allow for cheaper swaps at the cost of reduced functionality:
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PoolSpecialization {
    /// no specialization, suited for all Pools. IGeneralPool is used for swap request callbacks,
    /// passing the balance of all tokens in the Pool. These Pools have the largest swap costs
    /// (because of the extra storage reads), which increase with the number of registered tokens.
    General = 0,
    /// IMinimalSwapInfoPool is used instead of IGeneralPool, which saves gas by only passing the
    /// balance of the two tokens involved in the swap. This is suitable for some pricing algorithms,
    /// like the weighted constant product one popularized by Balancer V1. Swap costs are
    /// smaller compared to general Pools, and are independent of the number of registered tokens.
    MinimalSwapInfo = 1,
    /// only allows two tokens to be registered. This achieves the lowest possible swap gas cost.
    /// Like minimal swap info Pools, these are called via IMinimalSwapInfoPool.
    TwoToken = 2,
}

impl PoolSpecialization {
    fn new(specialization: u8) -> Result<Self> {
        match specialization {
            0 => Ok(Self::General),
            1 => Ok(Self::MinimalSwapInfo),
            2 => Ok(Self::TwoToken),
            t => Err(anyhow!("Invalid PoolSpecialization value {} (> 2)", t)),
        }
    }
}

impl PoolRegistry {
    // Since all the fields are private, we expose helper methods to fetch relevant information
    pub fn pools_containing_pair(&self, token_pair: TokenPair) -> HashSet<RegisteredPool> {
        let empty_set = HashSet::new();
        let pools_0 = self
            .pools_by_token
            .get(&token_pair.get().0)
            .unwrap_or(&empty_set);
        let pools_1 = self
            .pools_by_token
            .get(&token_pair.get().1)
            .unwrap_or(&empty_set);
        pools_0
            .intersection(pools_1)
            .into_iter()
            .map(|pool_id| {
                self.pools
                    .get(pool_id)
                    .expect("failed iterating over known pools")
                    .clone()
            })
            .collect()
    }

    fn try_upgrade(&mut self) -> Result<()> {
        for (pool_id, pool_builder) in self.pending_pools.clone() {
            let weighted_pool = pool_builder.into_pool()?;
            // delete pending pool and add to valid pools
            tracing::info!("Upgrading Pool Builder with id {:?}", pool_id);
            self.pools.insert(pool_id, weighted_pool.clone());
            self.pending_pools.remove(&pool_id);
            for token in weighted_pool.tokens {
                self.pools_by_token
                    .entry(token)
                    .or_default()
                    .insert(pool_id);
            }
        }
        Ok(())
    }

    fn insert_events(&mut self, events: Vec<(EventIndex, BalancerEvent)>) -> Result<()> {
        for (index, event) in events {
            match event {
                BalancerEvent::PoolRegistered(event) => self.insert_pool(index, event),
                BalancerEvent::TokensRegistered(event) => self.insert_token_data(index, event),
            };
        }
        // In the future, when processing TokensDeregistered we may have to downgrade the result.
        self.try_upgrade()?;
        Ok(())
    }

    fn insert_pool(&mut self, index: EventIndex, registration: PoolRegistered) {
        let pool_builder =
            self.pending_pools
                .entry(registration.pool_id)
                .or_insert(WeightedPoolBuilder {
                    pool_registration: None,
                    tokens_registration: None,
                    block_created: index.block_number,
                });
        // Whether the entry was there already or not, we set PoolRegistered
        pool_builder.pool_registration = Some(registration);
    }

    fn insert_token_data(&mut self, index: EventIndex, registration: TokensRegistered) {
        let pool_builder =
            self.pending_pools
                .entry(registration.pool_id)
                .or_insert(WeightedPoolBuilder {
                    pool_registration: None,
                    tokens_registration: None,
                    block_created: index.block_number,
                });
        // Whether the entry was there already or not, we set TokensRegistered
        pool_builder.tokens_registration = Some(registration);
    }

    fn replace_events(
        &mut self,
        delete_from_block_number: u64,
        events: Vec<(EventIndex, BalancerEvent)>,
    ) -> Result<()> {
        self.delete_pools(delete_from_block_number)?;
        self.insert_events(events)?;
        Ok(())
    }

    fn delete_pools(&mut self, delete_from_block_number: u64) -> Result<()> {
        self.pools
            .retain(|_, pool| pool.block_created < delete_from_block_number);
        self.pending_pools
            .retain(|_, pool| pool.block_created < delete_from_block_number);
        // Note that this could result in an empty set for some tokens.
        let retained_pool_ids: HashSet<H256> = self.pools.keys().copied().collect();
        for (_, pool_set) in self.pools_by_token.iter_mut() {
            *pool_set = pool_set
                .intersection(&retained_pool_ids)
                .cloned()
                .collect::<HashSet<H256>>();
        }
        Ok(())
    }

    fn last_event_block(&self) -> u64 {
        // Technically we could keep this updated more effectively in a field on balancer pools,
        // but the maintenance seems like more overhead that needs to be tested.
        let pending_max = self
            .pending_pools
            .iter()
            .map(|(_, pool_builder)| pool_builder.block_created)
            .max()
            .unwrap_or(0);
        let pool_max = self
            .pools
            .iter()
            .map(|(_, pool)| pool.block_created)
            .max()
            .unwrap_or(0);
        pending_max.max(pool_max)
    }

    fn contract_to_balancer_events(
        &self,
        contract_events: Vec<EthContractEvent<ContractEvent>>,
    ) -> Result<Vec<(EventIndex, BalancerEvent)>> {
        contract_events
            .into_iter()
            .filter_map(|EthContractEvent { data, meta }| {
                let meta = match meta {
                    Some(meta) => meta,
                    None => return Some(Err(anyhow!("event without metadata"))),
                };
                match data {
                    ContractEvent::PoolRegistered(event) => {
                        Some(convert_pool_registered(&event, &meta))
                    }
                    ContractEvent::TokensRegistered(event) => {
                        Some(convert_tokens_registered(&event, &meta))
                    }
                    ContractEvent::TokensDeregistered(event) => {
                        tracing::error!("unexpected Token Deregistration event {:?}", event);
                        None
                    }
                    _ => {
                        // TODO - Not processing other events at the moment.
                        // https://github.com/gnosis/gp-v2-services/issues/681
                        None
                    }
                }
            })
            .collect::<Result<Vec<_>>>()
    }
}

pub struct BalancerEventUpdater(
    Mutex<EventHandler<DynWeb3, BalancerV2VaultContract, PoolRegistry>>,
);

impl BalancerEventUpdater {
    pub async fn new(contract: BalancerV2Vault, pools: PoolRegistry) -> Result<Self> {
        let deployment_block = match contract.deployment_information() {
            Some(DeploymentInformation::BlockNumber(block_number)) => Some(block_number),
            Some(DeploymentInformation::TransactionHash(hash)) => Some(
                contract
                    .raw_instance()
                    .web3()
                    .block_number_from_tx_hash(hash)
                    .await?,
            ),
            None => None,
        };
        Ok(Self(Mutex::new(EventHandler::new(
            contract.raw_instance().web3(),
            BalancerV2VaultContract(contract),
            pools,
            deployment_block,
        ))))
    }
}

#[async_trait::async_trait]
impl EventStoring<ContractEvent> for PoolRegistry {
    async fn replace_events(
        &mut self,
        events: Vec<EthContractEvent<ContractEvent>>,
        range: RangeInclusive<BlockNumber>,
    ) -> Result<()> {
        let balancer_events = self
            .contract_to_balancer_events(events)
            .context("failed to convert events")?;
        tracing::debug!(
            "replacing {} events from block number {}",
            balancer_events.len(),
            range.start().to_u64()
        );
        PoolRegistry::replace_events(self, 0, balancer_events)?;
        Ok(())
    }

    async fn append_events(&mut self, events: Vec<EthContractEvent<ContractEvent>>) -> Result<()> {
        let balancer_events = self
            .contract_to_balancer_events(events)
            .context("failed to convert events")?;
        self.insert_events(balancer_events)
    }

    async fn last_event_block(&self) -> Result<u64> {
        Ok(self.last_event_block())
    }
}

impl_event_retrieving! {
    pub BalancerV2VaultContract for balancer_v2_vault
}

#[async_trait::async_trait]
impl Maintaining for BalancerEventUpdater {
    async fn run_maintenance(&self) -> Result<()> {
        self.0.run_maintenance().await
    }
}

fn convert_pool_registered(
    registration: &ContractPoolRegistered,
    meta: &EventMetadata,
) -> Result<(EventIndex, BalancerEvent)> {
    let event = PoolRegistered {
        pool_id: H256::from(registration.pool_id.0),
        pool_address: registration.pool_address,
        specialization: PoolSpecialization::new(registration.specialization)?,
    };
    Ok((EventIndex::from(meta), BalancerEvent::PoolRegistered(event)))
}

fn convert_tokens_registered(
    registration: &ContractTokensRegistered,
    meta: &EventMetadata,
) -> Result<(EventIndex, BalancerEvent)> {
    let event = TokensRegistered {
        pool_id: H256::from(registration.pool_id.0),
        tokens: registration.tokens.clone(),
    };
    Ok((
        EventIndex::from(meta),
        BalancerEvent::TokensRegistered(event),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maplit::hashset;

    #[tokio::test]
    async fn balancer_insert_events() {
        let n = 3usize;
        let pool_ids: Vec<H256> = (0..n).map(|i| H256::from_low_u64_be(i as u64)).collect();
        let pool_addresses: Vec<H160> = (0..n).map(|i| H160::from_low_u64_be(i as u64)).collect();
        let tokens: Vec<H160> = (0..n + 1)
            .map(|i| H160::from_low_u64_be(i as u64))
            .collect();
        let specializations: Vec<PoolSpecialization> = (0..n)
            .map(|i| PoolSpecialization::new(i as u8 % 3).unwrap())
            .collect();
        let pool_registration_events: Vec<BalancerEvent> = (0..n)
            .map(|i| {
                BalancerEvent::PoolRegistered(PoolRegistered {
                    pool_id: pool_ids[i],
                    pool_address: pool_addresses[i],
                    specialization: specializations[i],
                })
            })
            .collect();
        let token_registration_events: Vec<BalancerEvent> = (0..n)
            .map(|i| {
                BalancerEvent::TokensRegistered(TokensRegistered {
                    pool_id: pool_ids[i],
                    tokens: vec![tokens[i], tokens[i + 1]],
                })
            })
            .collect();

        let events: Vec<(EventIndex, BalancerEvent)> = vec![
            // Block 1 has both Pool and Tokens registered
            (EventIndex::new(1, 0), pool_registration_events[0].clone()),
            (EventIndex::new(1, 0), token_registration_events[0].clone()),
            // Next pool registered in block 2 with tokens only coming in block 3
            // Not realistic, but we can handle it.
            (EventIndex::new(2, 0), pool_registration_events[1].clone()),
            (EventIndex::new(3, 0), token_registration_events[1].clone()),
            // Next tokens registered in block 3, but corresponding pool not received till block 4
            (EventIndex::new(3, 0), token_registration_events[2].clone()),
            (EventIndex::new(4, 0), pool_registration_events[2].clone()),
        ];

        let mut pool_store = PoolRegistry {
            pools_by_token: Default::default(),
            pools: Default::default(),
            pending_pools: Default::default(),
        };
        pool_store.insert_events(events).unwrap();
        // Note that it is never expected that blocks for events will differ,
        // but in this test block_created for the pool is the first block it receives.
        assert_eq!(pool_store.last_event_block(), 3);
        assert_eq!(
            pool_store.pools_by_token.get(&tokens[0]).unwrap(),
            &hashset! { pool_ids[0] }
        );
        assert_eq!(
            pool_store.pools_by_token.get(&tokens[1]).unwrap(),
            &hashset! { pool_ids[0], pool_ids[1] }
        );
        assert_eq!(
            pool_store.pools_by_token.get(&tokens[2]).unwrap(),
            &hashset! { pool_ids[1], pool_ids[2] }
        );
        assert_eq!(
            pool_store.pools_by_token.get(&tokens[3]).unwrap(),
            &hashset! { pool_ids[2] }
        );
        for i in 0..n {
            assert_eq!(
                pool_store.pools.get(&pool_ids[i]).unwrap(),
                &RegisteredPool {
                    pool_id: pool_ids[i],
                    pool_address: pool_addresses[i],
                    tokens: vec![tokens[i], tokens[i + 1]],
                    specialization: PoolSpecialization::new(i as u8).unwrap(),
                    block_created: i as u64 + 1
                },
                "failed assertion at index {}",
                i
            );
            assert!(pool_store.pending_pools.get(&pool_ids[i]).is_none());
        }
    }

    #[tokio::test]
    async fn balancer_replace_events() {
        let start_block = 0;
        let end_block = 5;
        // Setup all the variables to initialize Balancer Pool State
        let pool_ids: Vec<H256> = (start_block..end_block + 1)
            .map(|i| H256::from_low_u64_be(i as u64))
            .collect();
        let pool_addresses: Vec<H160> = (start_block..end_block + 1)
            .map(|i| H160::from_low_u64_be(i as u64))
            .collect();
        let tokens: Vec<H160> = (start_block..end_block + 2)
            .map(|i| H160::from_low_u64_be(i as u64))
            .collect();
        let specializations: Vec<PoolSpecialization> = (start_block..end_block + 1)
            .map(|i| PoolSpecialization::new(i as u8 % 3).unwrap())
            .collect();
        let pool_registration_events: Vec<BalancerEvent> = (start_block..end_block + 1)
            .map(|i| {
                BalancerEvent::PoolRegistered(PoolRegistered {
                    pool_id: pool_ids[i],
                    pool_address: pool_addresses[i],
                    specialization: specializations[i],
                })
            })
            .collect();
        let token_registration_events: Vec<BalancerEvent> = (start_block..end_block + 1)
            .map(|i| {
                BalancerEvent::TokensRegistered(TokensRegistered {
                    pool_id: pool_ids[i],
                    tokens: vec![tokens[i], tokens[i + 1]],
                })
            })
            .collect();

        let balancer_events: Vec<(EventIndex, BalancerEvent)> = (start_block..end_block + 1)
            .map(|i| {
                vec![
                    (
                        EventIndex::new(i as u64, 0),
                        pool_registration_events[i].clone(),
                    ),
                    (
                        EventIndex::new(i as u64, 1),
                        token_registration_events[i].clone(),
                    ),
                ]
            })
            .flatten()
            .collect();

        let mut pool_store = PoolRegistry {
            pools_by_token: Default::default(),
            pools: Default::default(),
            pending_pools: Default::default(),
        };
        pool_store.insert_events(balancer_events).unwrap();

        // Let the tests begin!
        assert_eq!(pool_store.last_event_block(), 5);
        let new_pool_id = H256::from_low_u64_be(43110);
        let new_pool_address = H160::zero();
        let new_token = H160::from_low_u64_be(808);
        let new_pool_registration = PoolRegistered {
            pool_id: new_pool_id,
            pool_address: new_pool_address,
            specialization: PoolSpecialization::General,
        };
        let new_token_registration = TokensRegistered {
            pool_id: new_pool_id,
            tokens: vec![new_token],
        };

        let new_events = vec![
            (
                EventIndex::new(3, 0),
                BalancerEvent::PoolRegistered(new_pool_registration.clone()),
            ),
            (
                EventIndex::new(4, 0),
                BalancerEvent::TokensRegistered(new_token_registration.clone()),
            ),
        ];

        pool_store.replace_events(3, new_events.clone()).unwrap();
        // Everything until block 3 is unchanged.
        for i in 0..3 {
            assert_eq!(
                pool_store.pools.get(&pool_ids[i]).unwrap(),
                &RegisteredPool {
                    pool_id: pool_ids[i],
                    pool_address: pool_addresses[i],
                    tokens: vec![tokens[i], tokens[i + 1]],
                    specialization: specializations[i],
                    block_created: i as u64
                }
            );
        }
        assert_eq!(
            pool_store.pools_by_token.get(&tokens[0]).unwrap(),
            &hashset! { pool_ids[0] }
        );
        assert_eq!(
            pool_store.pools_by_token.get(&tokens[1]).unwrap(),
            &hashset! { pool_ids[0], pool_ids[1] }
        );
        assert_eq!(
            pool_store.pools_by_token.get(&tokens[2]).unwrap(),
            &hashset! { pool_ids[1], pool_ids[2] }
        );
        assert_eq!(
            pool_store.pools_by_token.get(&tokens[3]).unwrap(),
            &hashset! { pool_ids[2] }
        );

        // Everything old from block 3 on is gone.
        for pool_id in pool_ids.iter().take(6).skip(3) {
            assert!(pool_store.pools.get(pool_id).is_none());
        }
        for token in tokens.iter().take(7).skip(4) {
            assert!(pool_store.pools_by_token.get(token).unwrap().is_empty());
        }
        let new_event_block = new_events[0]
            .0
            .block_number
            .min(new_events[1].0.block_number);
        // All new data is included.
        assert!(pool_store.pending_pools.get(&new_pool_id).is_none());
        assert_eq!(
            pool_store.pools.get(&new_pool_id).unwrap(),
            &RegisteredPool {
                pool_id: new_pool_id,
                pool_address: new_pool_address,
                tokens: new_token_registration.tokens,
                specialization: new_pool_registration.specialization,
                block_created: new_event_block
            }
        );

        assert!(pool_store.pools_by_token.get(&new_token).is_some());
        assert_eq!(pool_store.last_event_block(), new_event_block);
    }

    #[test]
    fn pools_containing_pair_test() {
        let n = 3;
        let pool_ids: Vec<H256> = (0..n).map(|i| H256::from_low_u64_be(i as u64)).collect();
        let tokens: Vec<H160> = (0..n).map(|i| H160::from_low_u64_be(i as u64)).collect();
        let token_pairs: Vec<TokenPair> = (0..n)
            .map(|i| TokenPair::new(tokens[i], tokens[(i + 1) % n]).unwrap())
            .collect();

        // Test the empty pool.
        let mut pool_store = PoolRegistry {
            pools_by_token: Default::default(),
            pools: Default::default(),
            pending_pools: Default::default(),
        };
        for token_pair in token_pairs.iter().take(n) {
            assert!(pool_store.pools_containing_pair(*token_pair).is_empty());
        }

        // Now test non-empty pool with standard form.
        let mut weighted_pools = vec![];
        for i in 0..n {
            for pool_id in pool_ids.iter().take(i + 1) {
                // This is tokens[i] => { pool_id[0], pool_id[1], ..., pool_id[i] }
                let entry = pool_store.pools_by_token.entry(tokens[i]).or_default();
                entry.insert(*pool_id);
            }
            // This is weighted_pools[i] has tokens [tokens[i], tokens[i+1], ... , tokens[n]]
            weighted_pools.push(RegisteredPool {
                pool_id: pool_ids[i],
                tokens: tokens[i..n].to_owned(),
                // None of below fields are relevant here
                specialization: PoolSpecialization::General,
                block_created: 0,
                pool_address: Default::default(),
            });
            pool_store
                .pools
                .insert(pool_ids[i], weighted_pools[i].clone());
        }
        // When n = 3, this above generates
        // pool_store.pools_by_token = hashmap! {
        //     tokens[0] => hashset! { pool_ids[0] },
        //     tokens[1] => hashset! { pool_ids[0], pool_ids[1]},
        //     tokens[2] => hashset! { pool_ids[0], pool_ids[1], pool_ids[2] },
        // };
        // pool_store.pools = hashmap! {
        //     pool_ids[0] => WeightedPool {
        //         tokens: vec![tokens[0], tokens[1], tokens[2]],
        //         ..other fields
        //     },
        //     pool_ids[1] => WeightedPool {
        //         tokens: vec![tokens[1], tokens[2]],
        //         ..other fields
        //     }
        //     pool_ids[2] => WeightedPool {
        //         tokens: vec![tokens[2]],
        //         ..other fields
        //     }
        // };

        assert_eq!(
            pool_store.pools_containing_pair(token_pairs[0]),
            hashset! { weighted_pools[0].clone() }
        );
        assert_eq!(
            pool_store.pools_containing_pair(token_pairs[1]),
            hashset! { weighted_pools[1].clone(), weighted_pools[0].clone() }
        );
        assert_eq!(
            pool_store.pools_containing_pair(token_pairs[2]),
            hashset! { weighted_pools[0].clone() }
        );
    }
}

use std::time::SystemTime;

use model::{DomainSeparator, Order, OrderCreation, OrderMetaData, OrderUid};
use tokio::sync::RwLock;

#[derive(Debug, Eq, PartialEq)]
pub enum AddOrderError {
    DuplicatedOrder,
    InvalidSignature,
    #[allow(dead_code)]
    Forbidden,
    #[allow(dead_code)]
    MissingOrderData,
    #[allow(dead_code)]
    PastValidTo,
    #[allow(dead_code)]
    InsufficientFunds,
}

#[derive(Debug)]
pub enum RemoveOrderError {
    DoesNotExist,
}

#[derive(Debug, Default)]
pub struct OrderBook {
    domain_separator: DomainSeparator,
    // TODO: Store more efficiently (for example HashMap) depending on functionality we need.
    orders: RwLock<Vec<Order>>,
}

impl OrderBook {
    pub fn new(domain_separator: DomainSeparator) -> Self {
        Self {
            domain_separator,
            orders: RwLock::new(Vec::new()),
        }
    }

    pub async fn add_order(&self, order: OrderCreation) -> Result<OrderUid, AddOrderError> {
        if !has_future_valid_to(now_in_epoch_seconds(), &order) {
            return Err(AddOrderError::PastValidTo);
        }
        let mut orders = self.orders.write().await;
        if orders.iter().any(|x| x.order_creation == order) {
            return Err(AddOrderError::DuplicatedOrder);
        }
        let order = self.order_creation_to_order(order)?;
        let uid = order.order_meta_data.uid;
        tracing::debug!(?order, "adding order");
        orders.push(order);
        Ok(uid)
    }

    pub async fn get_orders(&self) -> Vec<Order> {
        self.orders.read().await.clone()
    }

    #[allow(dead_code)]
    pub async fn remove_order(&self, order: &OrderCreation) -> Result<(), RemoveOrderError> {
        let mut orders = self.orders.write().await;
        if let Some(index) = orders.iter().position(|x| x.order_creation == *order) {
            orders.swap_remove(index);
            Ok(())
        } else {
            Err(RemoveOrderError::DoesNotExist)
        }
    }

    // Run maintenance tasks like removing expired orders.
    pub async fn run_maintenance(&self) {
        self.remove_expired_orders(now_in_epoch_seconds()).await;
    }

    async fn remove_expired_orders(&self, now_in_epoch_seconds: u64) {
        // TODO: use the timestamp from the most recent block instead?
        let mut orders = self.orders.write().await;
        orders.retain(|order| has_future_valid_to(now_in_epoch_seconds, &order.order_creation));
    }

    fn order_creation_to_order(&self, user_order: OrderCreation) -> Result<Order, AddOrderError> {
        let owner = user_order
            .validate_signature(&self.domain_separator)
            .ok_or(AddOrderError::InvalidSignature)?;
        println!("owner: {}", owner);
        Ok(Order {
            order_meta_data: OrderMetaData {
                creation_date: chrono::offset::Utc::now(),
                owner,
                uid: user_order.uid(&owner),
            },
            order_creation: user_order,
        })
    }
}

fn now_in_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("now earlier than epoch")
        .as_secs()
}

fn has_future_valid_to(now_in_epoch_seconds: u64, order: &OrderCreation) -> bool {
    order.valid_to as u64 > now_in_epoch_seconds
}

#[cfg(test)]
pub mod test_util {
    use super::*;

    #[tokio::test]
    async fn cannot_add_order_twice() {
        let orderbook = OrderBook::default();
        let mut order = OrderCreation::default();
        order.valid_to = u32::MAX;
        order.sign_self();
        orderbook.add_order(order).await.unwrap();
        assert_eq!(orderbook.get_orders().await.len(), 1);
        assert_eq!(
            orderbook.add_order(order).await,
            Err(AddOrderError::DuplicatedOrder)
        );
    }

    #[tokio::test]
    async fn test_simple_removing_order() {
        let orderbook = OrderBook::default();
        let mut order = OrderCreation::default();
        order.valid_to = u32::MAX;
        order.sign_self();
        orderbook.add_order(order).await.unwrap();
        assert_eq!(orderbook.get_orders().await.len(), 1);
        orderbook.remove_order(&order).await.unwrap();
        assert_eq!(orderbook.get_orders().await.len(), 0);
    }

    #[tokio::test]
    async fn removes_expired_orders() {
        let orderbook = OrderBook::default();
        let mut order = OrderCreation::default();
        order.valid_to = u32::MAX - 10;
        order.sign_self();
        orderbook.add_order(order).await.unwrap();
        assert_eq!(orderbook.get_orders().await.len(), 1);
        orderbook
            .remove_expired_orders((u32::MAX - 11) as u64)
            .await;
        assert_eq!(orderbook.get_orders().await.len(), 1);
        orderbook.remove_expired_orders((u32::MAX - 9) as u64).await;
        assert_eq!(orderbook.get_orders().await.len(), 0);
    }
}

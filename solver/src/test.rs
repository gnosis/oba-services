///! Helper functions for unit tests.
use ethcontract::{Account, Address, PrivateKey};

/// Create a dummy account.
pub fn account() -> Account {
    Account::Offline(
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
            .parse()
            .unwrap(),
        None,
    )
}

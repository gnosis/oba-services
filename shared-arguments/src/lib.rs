//! Contains command line arguments and related helpers that are shared between the binaries.

use model::DomainSeparator;
use std::{num::ParseFloatError, time::Duration};

#[derive(Debug, structopt::StructOpt)]
pub struct Arguments {
    #[structopt(
        long,
        env = "LOG_FILTER",
        default_value = "warn,orderbook=debug,solver=debug"
    )]
    pub log_filter: String,

    #[structopt(
        long,
        env = "DOMAIN_SEPARATOR",
        default_value = "0000000000000000000000000000000000000000000000000000000000000000"
    )]
    pub domain_separator: DomainSeparator,
}

pub fn duration_from_seconds(s: &str) -> Result<Duration, ParseFloatError> {
    Ok(Duration::from_secs_f32(s.parse()?))
}

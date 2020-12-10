//! Contains command line arguments and related helpers that are shared between the binaries.

use std::{num::ParseFloatError, time::Duration};

#[derive(Debug, structopt::StructOpt)]
pub struct Arguments {
    #[structopt(
        long,
        env = "LOG_FILTER",
        default_value = "warn,orderbook=debug,solver=debug"
    )]
    pub log_filter: String,

    /// Control colors in log output. Useful for example in Kibana where ansi colors are not
    /// supported.
    #[structopt(
        long,
        env = "COLORED_OUTPUT",
        parse(try_from_str),
        default_value = "true"
    )]
    pub colored_output: bool,
}

pub fn duration_from_seconds(s: &str) -> Result<Duration, ParseFloatError> {
    Ok(Duration::from_secs_f32(s.parse()?))
}

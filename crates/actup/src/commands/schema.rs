//! Schema command - output JSON schema for configuration

use actup_core::Config;
use anstream::{eprintln, println};
use schemars::schema_for;

use crate::cli::CliExitCode;

pub fn run() -> CliExitCode {
    let schema = schema_for!(Config);
    match crate::structured::json_pretty(&schema) {
        Ok(json) => {
            println!("{json}");
            CliExitCode::Success
        }
        Err(e) => {
            eprintln!("Error serializing schema: {e}");
            CliExitCode::EnvironmentError
        }
    }
}

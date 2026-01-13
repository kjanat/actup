//! Schema command - output JSON schema for configuration

use actup_core::Config;
use schemars::schema_for;

pub fn run() -> i32 {
    let schema = schema_for!(Config);
    println!(
        "{}",
        serde_json::to_string_pretty(&schema).expect("failed to serialize schema")
    );
    0
}

//! Configuration file generator for zkboost-server Ethereum Execution Layer
//! stateless validator.
//!
//! This binary generates configuration files that enable zkboost-server to
//! serve Ethereum Execution Layer stateless validator guest programs.

use std::path::PathBuf;

use clap::Parser;
use ere_common::zkVMKind;
use ere_zkvm_interface::ProverResourceType;
use tokio::fs;
use toml_edit::{ArrayOfTables, Item, Table, Value, ser::to_document};
use zkboost_ethereum_el_config::program::download_program;
use zkboost_ethereum_el_types::ElKind;
use zkboost_server_config::{Config, zkVMConfig};

#[derive(Parser)]
struct Args {
    /// Execution layer client implementation (reth or ethrex)
    #[arg(long)]
    el: ElKind,
    /// zkVM to use
    #[arg(long)]
    zkvm: zkVMKind,
    /// Resource type for proving (cpu or gpu)
    #[arg(long, ignore_case = true, default_value = "cpu", value_parser = ["cpu", "gpu"])]
    resource: String,
    /// Output path to save the `config.toml` and the program.
    #[arg(long)]
    output_dir: PathBuf,
    /// Always download the artifact even a release url is available
    #[arg(long)]
    always_download: bool,
    /// GitHub token for downloading artifacts from GitHub Actions.
    /// Required when `ere-guests` dependency uses a git revision instead of a released tag.
    #[arg(env = "GITHUB_TOKEN")]
    github_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    let program = download_program(
        args.el,
        args.zkvm,
        args.github_token.as_deref(),
        &args.output_dir,
        args.always_download,
    )
    .await?;

    let config = Config {
        zkvm: vec![zkVMConfig {
            kind: args.zkvm,
            resource: match args.resource.to_lowercase().as_str() {
                "cpu" => ProverResourceType::Cpu,
                "gpu" => ProverResourceType::Gpu,
                _ => unreachable!(),
            },
            program_id: format!("{}-{}", args.el, args.zkvm).into(),
            program,
        }],
    };

    // Format array into array of tables.
    let mut config_toml = to_document(&config)?;
    if let Some(item) = config_toml.get_mut("zkvm")
        && let Item::Value(Value::Array(array)) = item
    {
        *item = array
            .iter()
            .map(|v| Table::from_iter(v.as_inline_table().unwrap()))
            .collect::<ArrayOfTables>()
            .into();
    }

    fs::write(args.output_dir.join("config.toml"), config_toml.to_string()).await?;

    Ok(())
}

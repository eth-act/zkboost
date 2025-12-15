use std::path::Path;

use benchmark_runner::{
    guest_programs::GuestFixture,
    stateless_validator::{ethrex, reth},
};
use tokio::fs;
use tracing::info;
use witness_generator::{
    FixtureGenerator,
    eest_generator::EESTFixtureGeneratorBuilder,
    rpc_generator::{RpcBlocksAndWitnessesBuilder, RpcFlatHeaderKeyValues},
};

use crate::{
    Args,
    util::{WORKLOAD_PKG_VERSION, WORKLOAD_REPO},
};

pub(crate) async fn generate_stateless_validator_fixture(
    args: &Args,
    workspace: &Path,
) -> anyhow::Result<Box<dyn GuestFixture>> {
    let input_dir = workspace.join("input");
    fs::create_dir_all(&input_dir).await?;

    if let Some(rpc_url) = &args.rpc_url {
        // If RPC url is set, download the latest block fixture

        info!("Generating statless validator guest input from latest mainnet block...");

        let headers = args
            .rpc_headers
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        RpcBlocksAndWitnessesBuilder::new(rpc_url.to_string())
            .with_headers(RpcFlatHeaderKeyValues::new(headers).try_into()?)
            .last_n_blocks(1)
            .build()
            .await?
            .generate_to_path(&input_dir)
            .await?;
    } else {
        // Otherwise use EEST empty block fixture

        info!("Generating statless validator guest input from EEST empty block...");

        let eest_fixtures_dir = workspace.join("fixtures").join("blockchain_tests");
        fs::create_dir_all(&eest_fixtures_dir).await?;
        let fixture_url = format!(
            "https://raw.githubusercontent.com/{}/{}/tests/assets/eest-empty-block/fixtures/blockchain_tests/empty_block.json",
            &*WORKLOAD_REPO,
            WORKLOAD_PKG_VERSION.as_str(),
        );
        let response = reqwest::get(fixture_url).await?;
        let content = response.bytes().await?;
        fs::write(eest_fixtures_dir.join("empty_block.json"), content).await?;
        EESTFixtureGeneratorBuilder::default()
            .with_input_folder(workspace.into())?
            .build()?
            .generate_to_path(&input_dir)
            .await?;
    }

    let inputs = match args.el.as_str() {
        "reth" => reth::stateless_validator_inputs(&input_dir)?,
        "ethrex" => ethrex::stateless_validator_inputs(&input_dir)?,
        _ => unreachable!(),
    };
    assert_eq!(inputs.len(), 1);

    info!("Successfully generated statless validator guest input");

    Ok(inputs.into_iter().next().unwrap())
}

//! Simple rust code to test the server

use reth::rpc::types::Block as RpcBlock;
use reth_stateless::{ClientInput, ExecutionWitness};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

#[derive(Serialize)]
struct VerifyRequest<'a> {
    program_id: &'a str,
    proof: Vec<u8>,
}

#[derive(Deserialize)]
struct VerifyResponse {
    program_id: String,
    verified: bool,
    failure_reason: String,
}

#[derive(Deserialize)]
struct InfoResponse {
    architecture: String,
    gpu: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(transparent)]
pub struct ProgramInput {
    pub input: ClientInput,
}

#[derive(Serialize)]
struct ExecuteRequest<'a> {
    program_id: &'a str,
    input: ProgramInput,
}

#[derive(Deserialize, Debug)]
struct ExecuteResponse {
    total_num_cycles: u64,
    pub execution_time_duration: Duration,
}

#[derive(Serialize)]
struct ProveRequest<'a> {
    program_id: &'a str,
    input: ProgramInput,
}

#[derive(Deserialize, Debug)]
struct ProveResponse {
    program_id: String,
    proof: Vec<u8>,
    proving_time: Duration,
}

// SERVER_URL is the url of the zkboost
const SERVER_URL: &str = "http://localhost:3000";
// PROGRAM_ID is the program identifier.
// Currently, we have one program per zkvm and use the zkvm
// name as an ID.
const PROGRAM_ID: &str = "sp1";
// Note: it needs to be a reth node specifically because
// we want `debug_executionWitness`
const RETH_URL: &str = "https://reth.mainnet.ethpandaops.io";

// Simple helper to test that the Cloudflare Access credentials and networking
// are configured correctly.
fn web3_client_version(reth_url: &str) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct RpcReq<'a> {
        jsonrpc: &'static str,
        method: &'a str,
        params: [(); 0],
        id: u32,
    }

    let req_body = RpcReq {
        jsonrpc: "2.0",
        method: "web3_clientVersion",
        params: [],
        id: 1,
    };

    let client = reqwest::blocking::Client::new();
    let mut req = client.post(reth_url);

    if let Ok(id) = std::env::var("CF_ACCESS_CLIENT_ID") {
        req = req.header("CF-Access-Client-Id", id);
    }
    if let Ok(secret) = std::env::var("CF_ACCESS_CLIENT_SECRET") {
        req = req.header("CF-Access-Client-Secret", secret);
    }

    let resp = req.json(&req_body).send()?;

    if !resp.status().is_success() {
        anyhow::bail!("web3_clientVersion error: {}", resp.status());
    }

    let v: serde_json::Value = resp.json()?;
    println!("web3_clientVersion → {}", v);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // Load environment variables from an optional `.env` file
    // that is not checked into git
    dotenvy::dotenv().ok();

    let server = SERVER_URL;

    // Call info for a quick health check
    info(server)?;

    let reth_url = RETH_URL;

    // Sanity-check credentials.
    if let Err(e) = web3_client_version(reth_url) {
        eprintln!("web3_clientVersion failed: {e}");
        std::process::exit(1);
    }

    // Fetch block + execution witness and call /execute.
    let program_input = fetch_client_input(reth_url, "latest")?;

    // Call execute endpoint
    println!("started execution");
    let program_id = PROGRAM_ID;
    execute(server, program_id, &program_input)?;

    // Call prove endpoint
    println!("started proving");
    let proof = prove(server, program_id, &program_input)?;

    Ok(())
}

fn verify(server: &str, program_id: &str, proof_path: &Path) -> anyhow::Result<()> {
    let proof_bytes = std::fs::read(proof_path)?;

    let req_body = VerifyRequest {
        program_id,
        proof: proof_bytes,
    };

    let url = format!("{}/verify", server.trim_end_matches('/'));
    let client = reqwest::blocking::Client::new();
    let resp = client.post(url).json(&req_body).send()?;

    if !resp.status().is_success() {
        anyhow::bail!("Request failed: {}", resp.status());
    }

    let resp_body: VerifyResponse = resp.json()?;
    if resp_body.verified {
        println!(
            "Verification succeeded for program {}",
            resp_body.program_id
        );
    } else {
        println!("Verification FAILED: {}", resp_body.failure_reason);
        std::process::exit(1);
    }
    Ok(())
}

fn info(server: &str) -> anyhow::Result<()> {
    let url = format!("{}/info", server.trim_end_matches('/'));
    let resp = reqwest::blocking::get(url)?;

    if !resp.status().is_success() {
        anyhow::bail!("Info request failed: {}", resp.status());
    }

    let info: InfoResponse = resp.json()?;
    println!(
        "Server architecture: {} | GPU: {}",
        info.architecture, info.gpu
    );
    Ok(())
}

fn execute(server: &str, program_id: &str, input: &ProgramInput) -> anyhow::Result<()> {
    let req_body = ExecuteRequest {
        program_id,
        input: input.clone(),
    };

    let url = format!("{}/execute", server.trim_end_matches('/'));

    let client = reqwest::blocking::Client::builder()
        // Increase timeout since execute could take a long time
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap();
    let resp = client.post(url).json(&req_body).send()?;

    if !resp.status().is_success() {
        anyhow::bail!("Request failed: {}", resp.status());
    }

    let resp_body: ExecuteResponse = resp.json()?;
    println!("Execution response {:?}", resp_body);

    Ok(())
}

fn prove(server: &str, program_id: &str, input: &ProgramInput) -> anyhow::Result<Vec<u8>> {
    let req_body = ProveRequest {
        program_id,
        input: input.clone(),
    };

    let url = format!("{}/prove", server.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        // Increase timeout since prove could take a long time
        .timeout(Duration::from_secs(3600))
        .build()
        .unwrap();
    let resp = client.post(url).json(&req_body).send()?;

    if !resp.status().is_success() {
        anyhow::bail!("Request failed: {}", resp.status());
    }

    let resp_body: ProveResponse = resp.json()?;
    println!("Proving duration {:?}", resp_body);
    Ok(resp_body.proof)
}

fn verify_bytes(server: &str, program_id: &str, proof: &[u8]) -> anyhow::Result<()> {
    let req_body = VerifyRequest {
        program_id,
        proof: proof.to_vec(),
    };

    let url = format!("{}/verify", server.trim_end_matches('/'));
    let client = reqwest::blocking::Client::new();
    let resp = client.post(url).json(&req_body).send()?;

    if !resp.status().is_success() {
        anyhow::bail!("Request failed: {}", resp.status());
    }

    let resp_body: VerifyResponse = resp.json()?;
    if resp_body.verified {
        println!(
            "Verification succeeded for program {}",
            resp_body.program_id
        );
    } else {
        println!("Verification FAILED: {}", resp_body.failure_reason);
        std::process::exit(1);
    }
    Ok(())
}

// Fetch latest block (full tx bodies) and execution witness
fn fetch_client_input(reth_url: &str, block_param: &str) -> anyhow::Result<ProgramInput> {
    // Helper closure to perform RPC POST with headers
    let do_rpc = |method: &str, params: serde_json::Value| -> anyhow::Result<serde_json::Value> {
        #[derive(Serialize)]
        struct RpcReq<'a> {
            jsonrpc: &'static str,
            method: &'a str,
            params: serde_json::Value,
            id: u32,
        }

        let req_body = RpcReq {
            jsonrpc: "2.0",
            method,
            params,
            id: 1,
        };

        let client = reqwest::blocking::Client::new();
        let mut req = client.post(reth_url).timeout(Duration::from_secs(300));
        if let Ok(id) = std::env::var("CF_ACCESS_CLIENT_ID") {
            req = req.header("CF-Access-Client-Id", id);
        }
        if let Ok(secret) = std::env::var("CF_ACCESS_CLIENT_SECRET") {
            req = req.header("CF-Access-Client-Secret", secret);
        }
        let resp = req.json(&req_body).send()?;
        if !resp.status().is_success() {
            anyhow::bail!(
                "{method} RPC error: {{status_code: {}, body: {}}}",
                resp.status(),
                resp.text()
                    .unwrap_or_else(|_| String::from("[response body read error]"))
            );
        }
        let v: serde_json::Value = resp.json()?;
        Ok(v["result"].clone())
    };

    // 1. Fetch block JSON
    let block_json = do_rpc(
        "eth_getBlockByNumber",
        serde_json::json!([block_param, true]),
    )?;

    // 2. Fetch execution witness JSON
    let witness_json = do_rpc("debug_executionWitness", serde_json::json!([block_param]))?;

    // 3. Parse into RpcBlock and convert to reth::primitives::Block.
    let rpc_block: RpcBlock = serde_json::from_value(block_json)?;
    let block = rpc_block
        .map_transactions(|tx| alloy_consensus::TxEnvelope::from(tx).into())
        .into_consensus();
    let witness: ExecutionWitness = serde_json::from_value(witness_json)?;

    Ok(ProgramInput {
        input: ClientInput { block, witness },
    })
}

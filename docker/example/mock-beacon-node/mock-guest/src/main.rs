#![cfg_attr(any(feature = "sp1", feature = "zisk"), no_main)]

use stateless_validator_common::{
    guest::{StatelessInput, StatelessValidationResult},
    HashTreeRoot, Sha2Hasher, SszEncode,
};

#[cfg(feature = "openvm")]
use ere_platform_openvm::{OpenVMPlatform as P, Platform};

#[cfg(feature = "sp1")]
use ere_platform_sp1::{sp1_zkvm, Platform, SP1Platform as P};

#[cfg(feature = "sp1")]
sp1_zkvm::entrypoint!(main);

#[cfg(feature = "zisk")]
use ere_platform_zisk::{ziskos, Platform, ZiskPlatform as P};

#[cfg(feature = "zisk")]
ziskos::entrypoint!(main);

fn main() {
    let (fork, input) = StatelessInput::from_schema_prefixed_ssz(&P::read_input())
        .expect("stateless input decodes");
    let result = StatelessValidationResult {
        new_payload_request_root: input.new_payload_request.hash_tree_root(&Sha2Hasher),
        successful_validation: true,
        chain_id: input.chain_id,
        schema_id: fork.schema_id(),
    };
    P::write_output(&result.to_ssz());
}

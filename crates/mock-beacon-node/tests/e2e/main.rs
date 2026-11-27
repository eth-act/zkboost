//! End-to-end tests, one for each entry of the `cases!` table. The test is the CL. Its Engine API
//! requests go through the mock beacon node to zkboost and on to a fake EL. zkboost submits its
//! proofs to the mock beacon node, which verifies them against a fake CL.
//!
//! A case gives the behavior of the fakes, the steps of the CL, and the calls that the EL gets.
//! A `Wait` step gives the whole `Outcome` that zkboost counts at that time. Every case also
//! checks the invariants of `Harness::check_invariants`.

use crate::harness::{
    Case, NEW_PAYLOAD, NEW_PAYLOAD_WITH_WITNESS, Outcome, Scenario,
    Step::{
        Call, CallLarge, CallWithoutJwt, Get, NewPayload, NewPayloadTwice, Post, SlowCall, Wait,
    },
};

mod fakes;
mod harness;

/// Generates one test for each case, so the cases run at the same time and by name.
macro_rules! cases {
    ($($(#[$doc:meta])* $name:ident: $case:expr,)*) => {
        $(
            $(#[$doc])*
            #[tokio::test]
            async fn $name() {
                $case.run().await
            }
        )*
    };
}

const NONE: Outcome = Outcome::NONE;

/// One payload with its witness, proven and accepted once by every prover.
const PROVEN: Outcome = Outcome {
    valid_payloads: 1,
    witnesses: 1,
    started: 1,
    proved: 1,
    submitted: 1,
    accepted: 1,
    ..NONE
};

/// One payload with its witness, whose proof generation started and did not end with a proof.
const STARTED: Outcome = Outcome {
    valid_payloads: 1,
    witnesses: 1,
    started: 1,
    ..NONE
};

cases! {
    /// The same payload sent again, to which the EL responds without a witness, gets the proofs
    /// from the cache. A reused proof is no second accepted proof.
    test_payload_proven_and_reused: Case {
        scenario: Scenario { provers: &["reth-openvm", "reth-zisk"], ..Scenario::DEFAULT },
        steps: &[
            NewPayload,
            Wait(PROVEN),
            NewPayload,
            Wait(Outcome { valid_payloads: 2, missing_witnesses: 1, reused: 1, submitted: 2, ..PROVEN }),
        ],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS, NEW_PAYLOAD_WITH_WITNESS],
    },

    /// A payload that arrives again while its proof is in progress requests the proof once.
    test_payload_again_while_proving: Case {
        scenario: Scenario { proving_ms: 3000, ..Scenario::DEFAULT },
        steps: &[
            NewPayload,
            Wait(STARTED),
            NewPayload,
            Wait(Outcome { valid_payloads: 2, missing_witnesses: 1, ..PROVEN }),
        ],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS, NEW_PAYLOAD_WITH_WITNESS],
    },

    /// Two payloads at the same time, both with a witness, request the proof once.
    test_payloads_at_once: Case {
        scenario: Scenario { witness_every_call: true, ..Scenario::DEFAULT },
        steps: &[NewPayloadTwice, Wait(Outcome { valid_payloads: 2, witnesses: 2, ..PROVEN })],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS, NEW_PAYLOAD_WITH_WITNESS],
    },

    /// zkboost responds to every request as the EL responds to it, byte for byte. The duration of a
    /// request starts before the upload of its body.
    test_requests_forwarded: Case {
        scenario: Scenario::DEFAULT,
        steps: &[
            Call("engine_forkchoiceUpdatedV4"),
            Call("engine_getPayloadV6"),
            SlowCall("engine_getBlobsV3"),
            Post(r#"[{"jsonrpc":"2.0","id":1,"method":"engine_exchangeCapabilities","params":[]}]"#),
            Post("not json"),
            CallLarge("engine_large"),
            CallWithoutJwt("engine_forkchoiceUpdatedV4"),
            Get("health", 200),
            Get("dashboard", 200),
            Get("unknown", 404),
        ],
        el_calls: &[
            "engine_forkchoiceUpdatedV4",
            "engine_getPayloadV6",
            "engine_getBlobsV3",
            "",
            "engine_large",
        ],
    },

    /// For an unreachable EL, zkboost responds with 502, not with a JSON-RPC error forged for the
    /// EL.
    test_el_unreachable: Case {
        scenario: Scenario { el_unreachable: true, provers: &[], ..Scenario::DEFAULT },
        steps: &[Call("engine_forkchoiceUpdatedV4")],
        el_calls: &[],
    },

    /// An EL without the witness method responds to the plain method, and nothing is proven. The
    /// witness method, to which the EL responds with a JSON-RPC error, is forwarded like any other.
    test_witness_method_unsupported: Case {
        scenario: Scenario { witness_unsupported: true, ..Scenario::DEFAULT },
        steps: &[
            NewPayload,
            Wait(Outcome { valid_payloads: 1, missing_witnesses: 1, ..NONE }),
            Call(NEW_PAYLOAD_WITH_WITNESS),
        ],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS, NEW_PAYLOAD, NEW_PAYLOAD_WITH_WITNESS],
    },

    test_invalid_payload_not_proven: Case {
        scenario: Scenario { payload_status: "INVALID", ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(Outcome { invalid_payloads: 1, not_valid_witnesses: 1, ..NONE })],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },

    /// Before the validator is ready, zkboost is a plain proxy.
    test_validator_not_ready: Case {
        scenario: Scenario { cl_unreachable: true, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(Outcome { valid_payloads: 1, ..NONE })],
        el_calls: &[NEW_PAYLOAD],
    },

    /// A panic of the submitter, here for a spec that Lighthouse rejects, leaves the proxy in
    /// service, and the submitter starts again.
    test_submitter_panic_recovered: Case {
        scenario: Scenario { spec_invalid_once: true, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(PROVEN)],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },

    /// An ere-server that does not respond is not ready, and the other prover proves.
    test_ere_server_unreachable: Case {
        scenario: Scenario { ere_unreachable: true, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(PROVEN)],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },

    /// Without a zkVM, zkboost is a plain proxy.
    test_no_zkvm: Case {
        scenario: Scenario { provers: &[], ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(Outcome { valid_payloads: 1, ..NONE })],
        el_calls: &[NEW_PAYLOAD],
    },

    test_proof_failure_not_submitted: Case {
        scenario: Scenario { proof_failure: true, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(Outcome { prove_errors: 1, ..STARTED })],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },

    test_proof_timeout_not_submitted: Case {
        scenario: Scenario { proving_ms: 3000, proof_timeout_secs: 1, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(Outcome { prove_timeouts: 1, ..STARTED })],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },

    /// After an equivocation of the proposer, the proof has no single beacon block.
    test_ambiguous_beacon_block: Case {
        scenario: Scenario { ambiguous_block: true, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(Outcome { proved: 1, submission_errors: 1, ..STARTED })],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },

    /// zkboost looks for the beacon block at the slot of the payload only.
    test_bid_of_another_slot_ignored: Case {
        scenario: Scenario { other_slot_bid: true, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(PROVEN)],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },

    /// A submission is tried again after an error of the beacon node.
    test_beacon_node_error_once: Case {
        scenario: Scenario { headers_error_once: true, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(PROVEN)],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },

    /// Of two events for one payload, the first beacon block is kept.
    test_first_block_from_event_kept: Case {
        scenario: Scenario { block_from_event: true, ambiguous_block: true, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(PROVEN)],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },

    /// The `execution_payload` event carries the beacon block root. zkboost subscribes to the
    /// events again after the beacon node rejects a subscription.
    test_events_error_once: Case {
        scenario: Scenario { block_from_event: true, events_error_once: true, ..Scenario::DEFAULT },
        steps: &[NewPayload, Wait(PROVEN)],
        el_calls: &[NEW_PAYLOAD_WITH_WITNESS],
    },
}

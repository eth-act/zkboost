//! The proof types, each a stateless validator guest on a zkVM.

use ere_catalog::zkVMKind;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, FromRepr, IntoStaticStr};

/// The discriminant is the EIP-8025 proof type of the beacon chain.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Display,
    EnumIter,
    FromRepr,
    IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
#[repr(u8)]
pub enum ProofType {
    EthrexOpenvm = 1,
    EthrexSp1 = 2,
    EthrexZisk = 3,
    RethOpenvm = 4,
    RethSp1 = 5,
    RethZisk = 6,
    ZesuZisk = 7,
    NimbusSp1 = 8,
    NimbusZisk = 9,
    NethermindSp1 = 10,
    NethermindZisk = 11,
    ZilkwormSp1 = 12,
    ZesuOpenvm = 13,
    ZesuSp1 = 14,
    RethPico = 15,
}

impl ProofType {
    /// Returns the name, which is also the metric label.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn zkvm_kind(self) -> zkVMKind {
        use ProofType::*;
        match self {
            EthrexOpenvm | RethOpenvm | ZesuOpenvm => zkVMKind::OpenVM,
            EthrexSp1 | RethSp1 | ZesuSp1 | NimbusSp1 | NethermindSp1 | ZilkwormSp1 => {
                zkVMKind::SP1
            }
            EthrexZisk | RethZisk | ZesuZisk | NimbusZisk | NethermindZisk => zkVMKind::Zisk,
            RethPico => zkVMKind::Pico,
        }
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator;

    use crate::proof::proof_type::ProofType;

    /// The ids and names are the ones of the beacon chain clients, which reject an unknown id.
    #[test]
    fn test_ids_and_names() {
        let expected = [
            (1, "ethrex-openvm"),
            (2, "ethrex-sp1"),
            (3, "ethrex-zisk"),
            (4, "reth-openvm"),
            (5, "reth-sp1"),
            (6, "reth-zisk"),
            (7, "zesu-zisk"),
            (8, "nimbus-sp1"),
            (9, "nimbus-zisk"),
            (10, "nethermind-sp1"),
            (11, "nethermind-zisk"),
            (12, "zilkworm-sp1"),
            (13, "zesu-openvm"),
            (14, "zesu-sp1"),
            (15, "reth-pico"),
        ];
        let proof_types = ProofType::iter()
            .map(|proof_type| (proof_type as u8, proof_type.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(proof_types, expected);
        for (id, name) in expected {
            let proof_type = ProofType::from_repr(id).unwrap();
            assert_eq!(serde_json::to_value(proof_type).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<ProofType>(name.into()).unwrap(),
                proof_type
            );
            assert!(name.ends_with(&proof_type.zkvm_kind().to_string()));
        }
    }
}

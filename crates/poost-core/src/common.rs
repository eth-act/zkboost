//! Common Types for zkVM Operations in Poost
use ere_zkvm_interface::zkVM;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Hash)]
#[serde(transparent)]
pub struct ProgramID(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum zkVMVendor {
    Airbender,
    Jolt,
    Miden,
    Nexus,
    Openvm,
    Pico,
    Risc0,
    SP1,
    Ziren,
    Zisk,
}

#[derive(Clone)]
#[allow(non_camel_case_types)]
pub struct zkVMInstance {
    pub vendor: zkVMVendor,
    pub vm: Arc<dyn zkVM + Send + Sync>,
}

impl std::fmt::Display for zkVMVendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            zkVMVendor::Airbender => write!(f, "airbender"),
            zkVMVendor::Jolt => write!(f, "jolt"),
            zkVMVendor::Miden => write!(f, "miden"),
            zkVMVendor::Nexus => write!(f, "nexus"),
            zkVMVendor::Openvm => write!(f, "openvm"),
            zkVMVendor::Pico => write!(f, "pico"),
            zkVMVendor::Risc0 => write!(f, "risc0"),
            zkVMVendor::SP1 => write!(f, "sp1"),
            zkVMVendor::Ziren => write!(f, "ziren"),
            zkVMVendor::Zisk => write!(f, "zisk"),
        }
    }
}

impl std::str::FromStr for zkVMVendor {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "airbender" => Ok(zkVMVendor::Airbender),
            "jolt" => Ok(zkVMVendor::Jolt),
            "miden" => Ok(zkVMVendor::Miden),
            "nexus" => Ok(zkVMVendor::Nexus),
            "openvm" => Ok(zkVMVendor::Openvm),
            "pico" => Ok(zkVMVendor::Pico),
            "risc0" => Ok(zkVMVendor::Risc0),
            "sp1" => Ok(zkVMVendor::SP1),
            "ziren" => Ok(zkVMVendor::Ziren),
            "zisk" => Ok(zkVMVendor::Zisk),
            _ => Err(format!(
                "Unsupported zkVM type: {}. Supported types are: risc0, sp1",
                s
            )),
        }
    }
}

// TODO: We may use a hash of the elf binary or program
// TODO: in which case, we would remove this From impl
impl From<zkVMVendor> for ProgramID {
    fn from(value: zkVMVendor) -> Self {
        ProgramID(format!("{}", value))
    }
}

impl zkVMInstance {
    pub fn new(vendor: zkVMVendor, vm: Arc<dyn zkVM + Send + Sync>) -> Self {
        Self { vendor, vm }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zkvm_type_parsing() {
        // Test all variants in enum definition order
        assert_eq!(
            "airbender".parse::<zkVMVendor>().unwrap(),
            zkVMVendor::Airbender
        );
        assert_eq!("jolt".parse::<zkVMVendor>().unwrap(), zkVMVendor::Jolt);
        assert_eq!("miden".parse::<zkVMVendor>().unwrap(), zkVMVendor::Miden);
        assert_eq!("nexus".parse::<zkVMVendor>().unwrap(), zkVMVendor::Nexus);
        assert_eq!("openvm".parse::<zkVMVendor>().unwrap(), zkVMVendor::Openvm);
        assert_eq!("pico".parse::<zkVMVendor>().unwrap(), zkVMVendor::Pico);
        assert_eq!("risc0".parse::<zkVMVendor>().unwrap(), zkVMVendor::Risc0);
        assert_eq!("sp1".parse::<zkVMVendor>().unwrap(), zkVMVendor::SP1);
        assert_eq!("ziren".parse::<zkVMVendor>().unwrap(), zkVMVendor::Ziren);
        assert_eq!("zisk".parse::<zkVMVendor>().unwrap(), zkVMVendor::Zisk);

        // Test case insensitivity
        assert_eq!("RISC0".parse::<zkVMVendor>().unwrap(), zkVMVendor::Risc0);
        assert_eq!("SP1".parse::<zkVMVendor>().unwrap(), zkVMVendor::SP1);
        assert_eq!(
            "Airbender".parse::<zkVMVendor>().unwrap(),
            zkVMVendor::Airbender
        );

        // Test invalid inputs
        assert!("invalid".parse::<zkVMVendor>().is_err());
        assert!("".parse::<zkVMVendor>().is_err());
    }
}

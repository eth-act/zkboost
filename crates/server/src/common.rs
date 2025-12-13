use std::{collections::HashMap, sync::Arc};

use ere_zkvm_interface::zkVM;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Hash)]
#[serde(transparent)]
pub struct ProgramID(pub String);

#[derive(Clone)]
#[allow(non_camel_case_types)]
pub struct zkVMInstance {
    pub vm: Arc<dyn zkVM + Send + Sync>,
}

impl zkVMInstance {
    pub fn new(vm: impl 'static + zkVM + Send + Sync) -> Self {
        Self { vm: Arc::new(vm) }
    }
}

#[derive(Clone, Default)]
pub struct AppState {
    pub programs: Arc<RwLock<HashMap<ProgramID, zkVMInstance>>>,
}

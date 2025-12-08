//! Module holds Axum app state and related functionalities.

use std::{collections::HashMap, sync::Arc};
use poost_core::common::{ProgramID, zkVMInstance};
use tokio::sync::RwLock;



#[derive(Clone)]
pub struct AppState {
    pub programs: Arc<RwLock<HashMap<ProgramID, zkVMInstance>>>,
}
//! Primitives for the Info route handler in the poost server
use serde::Serialize;


#[derive(Debug, Serialize)]
pub struct ServerInfoResponse {
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub os: OsInfo,
    pub architecture: String,
    pub gpu: String,
}

#[derive(Debug, Serialize)]
pub struct CpuInfo {
    pub model: String,
    pub cores: usize,
    pub frequency: u64,
    pub vendor: String,
}

#[derive(Debug, Serialize)]
pub struct MemoryInfo {
    pub total: String,
    pub available: String,
    pub used: String,
}

#[derive(Debug, Serialize)]
pub struct OsInfo {
    pub name: String,
    pub version: String,
    pub kernel: String,
}
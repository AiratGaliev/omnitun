mod config;
pub mod geoip;
pub mod installer;
mod process;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VpnError {
    #[error("Config error: {0}")]
    Config(String),
    #[error("Process error: {0}")]
    Process(String),
    #[error("Install error: {0}")]
    Install(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerProfile {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub server: String,
    pub port: u16,
    pub uuid: Option<String>,
    pub password: Option<String>,
    pub method: Option<String>,
    pub sni: Option<String>,
    pub flow: Option<String>,
    pub public_key: Option<String>,
    pub private_key: Option<String>,
}

impl Default for ServerProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            protocol: String::new(),
            server: String::new(),
            port: 0,
            uuid: None,
            password: None,
            method: None,
            sni: None,
            flow: None,
            public_key: None,
            private_key: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VpnStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

#![allow(dead_code)]

pub mod parser;

use crate::vpn::ServerProfile;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionProfile {
    pub id: String,
    pub name: String,
    pub url: String,
    pub servers: Vec<ServerProfile>,
    pub updated_at: String,
}

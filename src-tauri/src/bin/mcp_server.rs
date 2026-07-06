//! MCP server binary for OmniTun.
//!
//! Runs as a standalone process (no Tauri GUI).
//! Communicates via stdio using the Model Context Protocol.
//!
//! Build: cargo build --features mcp-server --bin mcp_server
//! Run:   cargo run --features mcp-server --bin mcp_server

fn main() {
    let data_dir = dirs_data_dir();
    if let Err(e) = omnitun_lib::mcp_server::run(data_dir) {
        eprintln!("MCP server error: {e}");
        std::process::exit(1);
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        base.join("com.omnitun.desktop")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let base = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        base.join("omnitun")
    }
}

//! spike1-mcp: stdio MCP server (rmcp) exposing spike1 test tools to pi.
//!
//! Tools:
//!   - spike1_ping: returns a pong payload (proves the MCP tool seam end to end)
//!   - spike1_list_spool: lists the spike's spool/ directory
//!
//! On startup it records the environment pi gave it (process markers and any
//! PI_* session variables) into `.pi-run/mcp-env.json` — evidence of what a
//! child process spawned by pi actually sees. Stdout is reserved for the MCP
//! protocol; diagnostics go to stderr.

use std::collections::BTreeMap;

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    service::serve_server,
};
use serde_json::json;

#[derive(Debug, Clone)]
struct SpikeTools {
    #[allow(dead_code)] // wired in by the #[tool_handler] macro
    tool_router: ToolRouter<Self>,
    started_at: std::time::SystemTime,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PingRequest {
    /// Optional short note the server echoes back in the pong.
    #[serde(default)]
    note: Option<String>,
}

#[tool_router]
impl SpikeTools {
    #[tool(description = "Spike1 test tool: replies with a pong payload proving the MCP seam works. Optionally echoes a note.")]
    async fn spike1_ping(&self, Parameters(PingRequest { note }): Parameters<PingRequest>) -> String {
        let elapsed_ms = self
            .started_at
            .elapsed()
            .map(|d| d.as_millis())
            .unwrap_or(0);
        json!({
            "pong": true,
            "spike": "spike1-host",
            "note": note,
            "serverUptimeMs": elapsed_ms,
        })
        .to_string()
    }

    #[tool(description = "Spike1 test tool: lists files in the spike's spool/ directory with sizes.")]
    async fn spike1_list_spool(&self) -> String {
        let dir = std::path::Path::new("spool");
        let mut items = vec![];
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                for e in rd.flatten() {
                    let meta = match e.metadata() {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    items.push(json!({
                        "name": e.file_name().to_string_lossy(),
                        "bytes": meta.len(),
                        "isDir": meta.is_dir(),
                    }));
                }
            }
            Err(e) => {
                return json!({ "error": format!("cannot read spool/: {e}") }).to_string();
            }
        }
        items.sort_by_key(|v| v["name"].as_str().unwrap_or("").to_string());
        json!({ "spool": items }).to_string()
    }
}

#[tool_handler]
impl ServerHandler for SpikeTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Spike1 exploratory MCP server for the terminal-first hosting seam.")
    }
}

/// Record the environment this server received from pi (selected keys only —
/// never secret values).
fn dump_env() {
    let interesting = [
        "AI_AGENT",
        "PI_CODING_AGENT",
        "PI_SESSION_ID",
        "PI_SESSION_FILE",
        "PI_PROVIDER",
        "PI_MODEL",
        "PI_REASONING_LEVEL",
    ];
    let mut seen: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for k in interesting {
        seen.insert(
            k.to_string(),
            std::env::var(k).map(serde_json::Value::String).unwrap_or(serde_json::Value::Null),
        );
    }
    let pi_keys: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("PI_") || k == "AI_AGENT")
        .collect();
    let record = json!({
        "ts": chrono_less_now(),
        "argv": std::env::args().collect::<Vec<_>>(),
        "cwd": std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        "env": seen,
        "allPiLikeKeysPresent": pi_keys,
    });
    let path = std::path::Path::new(".pi-run").join("mcp-env.json");
    if let Err(e) = std::fs::create_dir_all(".pi-run")
        .and_then(|_| {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
            writeln!(f, "{record}")
        })
    {
        eprintln!("[spike1-mcp] env dump failed: {e}");
    } else {
        eprintln!("[spike1-mcp] env dumped to {}", path.display());
    }
}

/// ISO-8601 UTC timestamp without extra crates (same algorithm as the host).
fn chrono_less_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let millis = d.subsec_millis();
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dump_env();
    let service = serve_server(
        SpikeTools { tool_router: SpikeTools::tool_router(), started_at: std::time::SystemTime::now() },
        (tokio::io::stdin(), tokio::io::stdout()),
    )
    .await?;
    let reason = service.waiting().await?;
    eprintln!("[spike1-mcp] stopped: {reason:?}");
    Ok(())
}

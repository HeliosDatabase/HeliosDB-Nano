//! HeliosDB Proxy - Main Entry Point
//!
//! Standalone proxy binary for HeliosDB-Lite connection routing.

use clap::Parser;
use heliosdb_proxy::{config::ProxyConfig, server::ProxyServer, Result, VERSION};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// HeliosDB Proxy - Connection Router and Failover Manager
#[derive(Parser, Debug)]
#[command(name = "heliosdb-proxy")]
#[command(version = VERSION)]
#[command(about = "HeliosDB Proxy - Intelligent connection router for HeliosDB-Lite")]
struct Args {
    /// Configuration file path
    #[arg(short, long)]
    config: Option<String>,

    /// Listen address
    #[arg(short, long, default_value = "0.0.0.0:5432")]
    listen: String,

    /// Admin API address
    #[arg(long, default_value = "0.0.0.0:9090")]
    admin: String,

    /// Primary node (host:port)
    #[arg(long)]
    primary: Option<String>,

    /// Standby nodes (can be specified multiple times)
    #[arg(long)]
    standby: Vec<String>,

    /// Enable TR (Transaction Replay)
    #[arg(long, default_value = "true")]
    tr: bool,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Enable JSON logging
    #[arg(long)]
    json_logs: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    init_logging(&args.log_level, args.json_logs);

    tracing::info!("HeliosDB Proxy v{} starting...", VERSION);

    // Load configuration
    let config = load_config(&args)?;

    // Create and run server
    let server = ProxyServer::new(config)?;

    tracing::info!("Starting proxy server on {}", args.listen);

    // Run until shutdown
    server.run().await?;

    tracing::info!("Proxy server stopped");
    Ok(())
}

fn init_logging(level: &str, json: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));

    let subscriber = tracing_subscriber::registry().with(filter);

    if json {
        subscriber
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        subscriber
            .with(tracing_subscriber::fmt::layer())
            .init();
    }
}

fn load_config(args: &Args) -> Result<ProxyConfig> {
    // If config file specified, load from file
    if let Some(ref path) = args.config {
        return ProxyConfig::from_file(path);
    }

    // Otherwise, build from command line args
    let mut config = ProxyConfig::default();
    config.listen_address = args.listen.clone();
    config.admin_address = args.admin.clone();
    config.tr_enabled = args.tr;

    // Add primary node
    if let Some(ref primary) = args.primary {
        config.add_node(primary, "primary")?;
    }

    // Add standby nodes
    for standby in &args.standby {
        config.add_node(standby, "standby")?;
    }

    // Validate configuration
    config.validate()?;

    Ok(config)
}

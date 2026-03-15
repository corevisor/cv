mod config;
mod credential_store;
mod engine;
mod handler;
mod hub_client;
mod types;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;

#[derive(Parser)]
#[command(name = "cv", about = "Corevisor CLI — local credential management + MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (stdio transport)
    Serve {
        /// Profile to use (overrides default)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Authenticate with a Corevisor Hub
    Login {
        /// Hub URL (default: https://api.corevisor.xyz)
        #[arg(long, default_value = "https://api.corevisor.xyz")]
        hub_url: String,
    },
    /// Sync profile config from the Hub
    Sync {
        /// Profile to sync (or all if omitted)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Manage local credentials
    Credential {
        #[command(subcommand)]
        action: CredentialAction,
    },
}

#[derive(Subcommand)]
enum CredentialAction {
    /// Store a credential for a domain
    Set {
        /// Domain (e.g. api.notion.com)
        domain: String,
        /// Header name (default: Authorization)
        #[arg(long, default_value = "Authorization")]
        header: String,
        /// Profile to use
        #[arg(long)]
        profile: Option<String>,
    },
    /// List stored credentials
    List {
        /// Profile to list
        #[arg(long)]
        profile: Option<String>,
    },
    /// Delete a stored credential
    Delete {
        /// Domain to delete credential for
        domain: String,
        /// Profile to use
        #[arg(long)]
        profile: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,corevisor_cli=debug".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let mut app_config = config::AppConfig::load()?;

    match cli.command {
        Commands::Serve { profile } => {
            let profile_id = resolve_profile(&app_config, profile.as_deref())?;
            cmd_serve(&app_config, &profile_id).await?;
        }
        Commands::Login { hub_url } => {
            cmd_login(&mut app_config, &hub_url).await?;
        }
        Commands::Sync { profile } => {
            let profile_id = resolve_profile(&app_config, profile.as_deref())?;
            cmd_sync(&mut app_config, &profile_id).await?;
        }
        Commands::Credential { action } => match action {
            CredentialAction::Set {
                domain,
                header,
                profile,
            } => {
                let profile_id = resolve_profile(&app_config, profile.as_deref())?;
                cmd_credential_set(&profile_id, &domain, &header)?;
            }
            CredentialAction::List { profile } => {
                let profile_id = resolve_profile(&app_config, profile.as_deref())?;
                cmd_credential_list(&profile_id)?;
            }
            CredentialAction::Delete { domain, profile } => {
                let profile_id = resolve_profile(&app_config, profile.as_deref())?;
                cmd_credential_delete(&profile_id, &domain)?;
            }
        },
    }

    Ok(())
}

fn resolve_profile(config: &config::AppConfig, name_or_id: Option<&str>) -> Result<String> {
    if let Some(val) = name_or_id {
        // Try matching by name first, then by ID
        for (id, p) in &config.profiles {
            if p.name == val || id == val {
                return Ok(id.clone());
            }
        }
        anyhow::bail!("profile '{}' not found. Run `cv sync` first.", val);
    }
    config
        .default_profile
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no default profile set. Use --profile or run `cv login`"))
}

async fn cmd_serve(
    config: &config::AppConfig,
    profile_id: &str,
) -> Result<()> {
    let profile = config
        .profiles
        .get(profile_id)
        .ok_or_else(|| anyhow::anyhow!("profile not found in local config"))?;

    let store = credential_store::CredentialStore::new()?;

    let js_engine = engine::JsEngine::new()?;
    tracing::info!("WASM JS engine initialized");

    let hub_url = config
        .hub_url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("not logged in. Run `cv login` first."))?;
    let token = config
        .oauth_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("not logged in. Run `cv login` first."))?;
    let hub = hub_client::HubClient::new(hub_url.clone(), token.clone());

    let executor = handler::JsExecutor::new(
        js_engine,
        profile_id.to_string(),
        profile.services.clone(),
        store,
        hub,
    );

    tracing::info!(profile = %profile.name, "starting MCP server (stdio)");

    let service = executor
        .serve(rmcp::transport::stdio())
        .await
        .inspect_err(|e| {
            tracing::error!("serving error: {:?}", e);
        })?;

    service.waiting().await?;

    Ok(())
}

async fn cmd_login(
    config: &mut config::AppConfig,
    hub_url: &str,
) -> Result<()> {
    let hub_url = hub_url.trim_end_matches('/').to_string();

    eprintln!("Opening browser for authentication...");
    let client = hub_client::HubClient::new(hub_url.clone(), String::new());
    let token = client.oauth_login().await?;

    // Fetch profiles with the new token
    let client = hub_client::HubClient::new(hub_url.clone(), token.clone());
    let profiles = client.get_profiles().await?;

    config.hub_url = Some(hub_url);
    config.oauth_token = Some(token);

    if let Some(first) = profiles.first() {
        config.default_profile = Some(first.id.clone());
    }

    for p in &profiles {
        let services = client.get_services(&p.id).await?;
        config.profiles.insert(
            p.id.clone(),
            config::ProfileConfig {
                name: p.name.clone(),
                services,
                synced_at: Some(chrono_now()),
            },
        );
    }

    config.save()?;
    eprintln!("Logged in. {} profile(s) synced.", profiles.len());
    for p in &profiles {
        eprintln!("  - {} ({})", p.name, p.id);
    }

    Ok(())
}

async fn cmd_sync(config: &mut config::AppConfig, profile_id: &str) -> Result<()> {
    let hub_url = config
        .hub_url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("not logged in. Run `cv login` first."))?;
    let token = config
        .oauth_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no OAuth token. Run `cv login` first."))?;

    let client = hub_client::HubClient::new(hub_url.clone(), token.clone());
    let services = client.get_services(profile_id).await?;

    // Look up the profile name from existing config, or fetch all profiles to find it
    let name = if let Some(existing) = config.profiles.get(profile_id) {
        existing.name.clone()
    } else {
        let profiles = client.get_profiles().await?;
        profiles
            .iter()
            .find(|p| p.id == profile_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| profile_id.to_string())
    };

    let service_count = services.len();
    config.profiles.insert(
        profile_id.to_string(),
        config::ProfileConfig {
            name: name.clone(),
            services,
            synced_at: Some(chrono_now()),
        },
    );
    config.save()?;

    eprintln!("Synced profile '{}' ({} services)", name, service_count);

    Ok(())
}

fn cmd_credential_set(profile_id: &str, domain: &str, header_name: &str) -> Result<()> {
    let store = credential_store::CredentialStore::new()?;

    eprintln!("Enter credential value for {domain}:");
    let value = rpassword::read_password()?;
    if value.trim().is_empty() {
        anyhow::bail!("credential value cannot be empty");
    }

    store.set(types::CredentialEntry {
        profile_id: profile_id.to_string(),
        domain: domain.to_string(),
        header_name: header_name.to_string(),
        header_value: value.trim().to_string(),
    })?;

    eprintln!("Credential stored for {domain} (header: {header_name})");
    Ok(())
}

fn cmd_credential_list(profile_id: &str) -> Result<()> {
    let store = credential_store::CredentialStore::new()?;
    let entries = store.list(profile_id)?;

    if entries.is_empty() {
        eprintln!("No credentials stored for this profile.");
        return Ok(());
    }

    for e in &entries {
        eprintln!("  {} — header: {}", e.domain, e.header_name);
    }

    Ok(())
}

fn cmd_credential_delete(profile_id: &str, domain: &str) -> Result<()> {
    let store = credential_store::CredentialStore::new()?;
    store.delete(profile_id, domain)?;
    eprintln!("Credential deleted for {domain}");
    Ok(())
}

fn chrono_now() -> String {
    // Simple ISO timestamp without chrono dependency
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let secs = dur.as_secs();
    // Basic formatting — good enough for a timestamp
    format!("{}Z", secs)
}

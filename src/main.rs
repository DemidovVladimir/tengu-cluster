use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use std::path::PathBuf;
use tracing::{error, info};

use tengu_backends::OllamaEngine;
use tengu_channels::CliPipe;
use tengu_core::config::{Config, RuntimeProfile};
use tengu_core::types::{DeliveryOptions, Message, Recipient, Role};
use tengu_core::{Engine, EngineContext, Lens, Pipe, PipeContext, Refiner};
use tengu_optimizer::{NoopRefiner, RuleRefiner};

#[derive(Parser)]
#[command(name = "tengu")]
#[command(about = "Model-agnostic AI agent hub. Single binary, zero dependencies.")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start interactive CLI chat (default)
    Chat,
    /// Start the hub daemon
    Serve,
    /// Show system status
    Status,
    /// Diagnose configuration issues
    Doctor,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tengu=info".parse().unwrap()),
        )
        .compact()
        .init();

    let cli = Cli::parse();

    // Load config
    let config_path = cli.config.unwrap_or_else(|| {
        let home = std::env::var("TENGU_HOME")
            .unwrap_or_else(|_| {
                dirs_next::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".tengu")
                    .to_string_lossy()
                    .to_string()
            });
        PathBuf::from(home).join("config.toml")
    });

    let config = Config::load_or_default(&config_path);

    // Detect runtime profile
    let profile = RuntimeProfile::resolve(Some(&config.runtime_profile));

    match cli.command.unwrap_or(Commands::Chat) {
        Commands::Chat => run_chat(config, profile).await,
        Commands::Serve => {
            info!("Hub daemon not yet implemented (Phase 8)");
            Ok(())
        }
        Commands::Status => {
            print_status(&config, profile);
            Ok(())
        }
        Commands::Doctor => {
            run_doctor(&config).await;
            Ok(())
        }
    }
}

async fn run_chat(config: Config, profile: RuntimeProfile) -> Result<()> {
    // Resolve default agent
    let (agent_id, agent_config) = config
        .agents
        .iter()
        .find(|(_, ac)| ac.default)
        .or_else(|| config.agents.iter().next())
        .map(|(id, ac)| (id.clone(), ac.clone()))
        .expect("No agents configured");

    // Create refiner based on config
    let refiner: Box<dyn Refiner> = match config.refiner.mode.as_str() {
        "rules" => Box::new(RuleRefiner::new()),
        "off" => Box::new(NoopRefiner),
        _ => Box::new(NoopRefiner),
    };

    // Create engine
    let engine: Box<dyn Engine> = match agent_config.engine.as_str() {
        "ollama" => {
            let base_url = std::env::var("OLLAMA_HOST")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            Box::new(OllamaEngine::new(&base_url, &agent_config.model))
        }
        other => {
            error!(engine = %other, "Engine not yet implemented");
            return Err(anyhow::anyhow!("Engine '{}' not yet implemented", other));
        }
    };

    // Print startup banner
    print_banner(&agent_id, &agent_config, profile, &config.refiner.mode, engine.as_ref());

    // Create CLI pipe
    let pipe = CliPipe::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    pipe.connect(PipeContext { inbound_tx: tx }).await?;

    // Conversation state
    let mut messages: Vec<Message> = Vec::new();
    let mut lens = Lens::from_str(&agent_config.default_lens);
    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;
    let mut tokens_saved: u32 = 0;

    // Build system prompt from workspace files
    let system_prompt = build_system_prompt(&agent_config);

    println!("Type your message (Ctrl+D to quit):\n");

    while let Some(inbound) = rx.recv().await {
        let original_len = inbound.content.len();

        // Handle slash commands
        if inbound.content.starts_with('/') {
            match inbound.content.as_str() {
                "/eco" => {
                    lens = Lens::Eco;
                    println!("Switched to eco lens (summaries only)\n");
                    continue;
                }
                "/standard" => {
                    lens = Lens::Standard;
                    println!("Switched to standard lens (auto-expand)\n");
                    continue;
                }
                "/precise" => {
                    lens = Lens::Precise;
                    println!("Switched to precise lens (full content)\n");
                    continue;
                }
                "/cost" => {
                    println!("Session Stats");
                    println!("─────────────────────────────");
                    println!(" Input tokens:  {}", total_input_tokens);
                    println!(" Output tokens: {}", total_output_tokens);
                    println!(" Total:         {}", total_input_tokens + total_output_tokens);
                    if tokens_saved > 0 {
                        println!();
                        println!(" Saved by refiner:");
                        println!("   Prompt compression: -{} tokens", tokens_saved);
                    }
                    println!();
                    continue;
                }
                "/context" => {
                    let used: usize = messages.iter().map(|m| m.content.len() / 4).sum();
                    let window = engine.context_window();
                    println!("Context: ~{} / {} tokens ({}%)\n",
                        used, window, (used * 100) / window.max(1));
                    continue;
                }
                "/reset" => {
                    messages.clear();
                    total_input_tokens = 0;
                    total_output_tokens = 0;
                    tokens_saved = 0;
                    println!("Flow reset.\n");
                    continue;
                }
                "/engine" => {
                    println!("Current: {}/{}", agent_config.engine, agent_config.model);
                    println!("Context window: {}\n", engine.context_window());
                    continue;
                }
                "/help" => {
                    println!("Commands:");
                    println!("  /eco       — Eco lens (summaries)");
                    println!("  /standard  — Standard lens (auto-expand)");
                    println!("  /precise   — Precise lens (full content)");
                    println!("  /engine    — Show current engine");
                    println!("  /cost      — Token usage stats");
                    println!("  /context   — Context window usage");
                    println!("  /reset     — Clear conversation");
                    println!("  /help      — This help\n");
                    continue;
                }
                _ => {
                    println!("Unknown command. Type /help for available commands.\n");
                    continue;
                }
            }
        }

        // Apply refiner
        let compressed = refiner.compress(&inbound.content).await?;
        let compressed_len = compressed.len();
        if compressed_len < original_len {
            let saved = ((original_len - compressed_len) / 4) as u32;
            tokens_saved += saved;
        }

        messages.push(Message {
            role: Role::User,
            content: compressed,
            tool_call_id: None,
            tool_calls: None,
        });

        // Run engine
        let context = EngineContext {
            workspace: agent_config.workspace.clone(),
            system_prompt: system_prompt.clone(),
        };

        match engine.run(&messages, &[], &context).await {
            Ok(mut stream) => {
                let mut response_text = String::new();

                while let Some(event) = stream.next().await {
                    match event {
                        tengu_core::types::StreamEvent::TextDelta { text } => {
                            response_text.push_str(&text);
                        }
                        tengu_core::types::StreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                        } => {
                            total_input_tokens += input_tokens;
                            total_output_tokens += output_tokens;
                        }
                        tengu_core::types::StreamEvent::Error { message } => {
                            eprintln!("Engine error: {}", message);
                        }
                        tengu_core::types::StreamEvent::Done => {}
                        _ => {}
                    }
                }

                if !response_text.is_empty() {
                    // Send through pipe
                    pipe.send_text(
                        &inbound.sender,
                        &response_text,
                        &DeliveryOptions::default(),
                    )
                    .await?;

                    messages.push(Message {
                        role: Role::Assistant,
                        content: response_text,
                        tool_call_id: None,
                        tool_calls: None,
                    });
                }
            }
            Err(e) => {
                eprintln!("Engine error: {}\n", e);
            }
        }
    }

    pipe.disconnect().await?;
    Ok(())
}

fn build_system_prompt(agent_config: &tengu_core::config::AgentConfig) -> Option<String> {
    let workspace = agent_config.workspace.as_ref()?;
    let mut parts = Vec::new();

    // Load workspace files in order: IDENTITY.md, PROFILE.md, CONTEXT.md
    for filename in &["IDENTITY.md", "PROFILE.md", "CONTEXT.md"] {
        let path = workspace.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if !content.trim().is_empty() {
                parts.push(format!("# {}\n\n{}", filename, content));
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n---\n\n"))
    }
}

fn print_banner(
    agent_id: &str,
    agent_config: &tengu_core::config::AgentConfig,
    profile: RuntimeProfile,
    refiner_mode: &str,
    engine: &dyn Engine,
) {
    let identity = agent_config.identity.name.as_deref().unwrap_or("Tengu");
    println!();
    println!("  TENGU CLUSTER");
    println!("  ─────────────────────────────────────");
    println!("  Agent:    {} ({})", identity, agent_id);
    println!("  Engine:   {}/{}", agent_config.engine, agent_config.model);
    println!("  Context:  {} tokens", engine.context_window());
    println!("  Refiner:  {}", refiner_mode);
    println!("  Lens:     {}", agent_config.default_lens);
    println!("  Profile:  {:?}", profile);
    println!("  ─────────────────────────────────────");
    println!();
}

fn print_status(config: &Config, profile: RuntimeProfile) {
    println!();
    println!("  TENGU CLUSTER — Status");
    println!("  ─────────────────────────────────────");
    println!("  Profile:  {:?}", profile);
    println!("  Refiner:  {}", config.refiner.mode);
    println!("  Agents:   {}", config.agents.len());
    for (id, ac) in &config.agents {
        println!("    - {} ({}/{}){}", id, ac.engine, ac.model,
            if ac.default { " [default]" } else { "" });
    }
    println!("  Hub:      {}:{}", config.hub.bind, config.hub.port);
    println!("  ─────────────────────────────────────");
    println!();
}

async fn run_doctor(config: &Config) {
    println!();
    println!("  TENGU CLUSTER — Doctor");
    println!("  ─────────────────────────────────────");

    // Check Ollama connectivity
    for (id, ac) in &config.agents {
        if ac.engine == "ollama" {
            let base_url = std::env::var("OLLAMA_HOST")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            print!("  Ollama ({})... ", id);
            match reqwest::get(format!("{}/api/tags", base_url)).await {
                Ok(resp) if resp.status().is_success() => println!("OK"),
                Ok(resp) => println!("Error: HTTP {}", resp.status()),
                Err(e) => println!("Unreachable: {}", e),
            }
        }
    }

    println!("  ─────────────────────────────────────");
    println!();
}

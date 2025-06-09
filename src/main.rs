mod cargo_remote;
mod config_watcher;
mod context;
mod lsp;
mod mcp;
mod project;

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use context::{Context as ContextType, ContextNotification};
use lsp::LspNotification;
use mcp::run_server;
use tokio::signal;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, Layer, fmt::format::PrettyFields, layer::SubscriberExt, util::SubscriberInitExt};

/// Beautify path display by converting long paths to a more concise format
#[must_use]
pub fn beautify_path3(path: &Path) -> String {
    // ---------- 1. Fast-path: relative to CWD ---------------------------------
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = path.strip_prefix(&cwd) {
            if rel.components().next().is_some() {
                let mut rel_cow: Cow<'_, str> = rel.to_string_lossy();
                if rel_cow.contains('\\') {
                    rel_cow = Cow::Owned(rel_cow.replace('\\', "/"));
                }
                return format!("📁 ./{}", rel_cow);
            }
        }
    }

    // ---------- 2. One UTF-8 conversion of the full path ----------------------
    let mut whole: Cow<'_, str> = {
        // temporary value so we don’t assign to a still-borrowed `whole`
        let tmp: Cow<'_, str> = path.to_string_lossy();

        // Remove the Windows “\\?\” prefix
        match tmp {
            // Already borrowed from the original `Path` → we can keep borrowing
            Cow::Borrowed(s) if s.starts_with(r"\\?\") => Cow::Borrowed(&s[r"\\?\".len()..]),

            // Owned String → allocate a new (shorter) String
            Cow::Owned(ref s) if s.starts_with(r"\\?\") => Cow::Owned(s[r"\\?\".len()..].to_owned()),

            // Nothing to strip
            _ => tmp,
        }
    };

    // ---------- 3. Compact representation for very long paths -----------------
    if whole.len() > 50 {
        if let Some(name) = path.file_name() {
            if let Some(parent_name) = path.parent().and_then(|p| p.file_name()) {
                return format!("📁 {}/{}", parent_name.to_string_lossy(), name.to_string_lossy());
            }
            return format!("📁 {}", name.to_string_lossy());
        }
    }

    // ---------- 4. Normal case: final normalisation ---------------------------
    if whole.contains('\\') {
        whole = Cow::Owned(whole.replace('\\', "/"));
    }

    format!("📁 {}", whole)
}

#[must_use]
pub fn beautify_path2(path: &std::path::Path) -> String {
    let path_str = path.to_string_lossy();

    // Remove Windows \\?\\ prefix
    let cleaned = path_str.strip_prefix(r"\\?\\").unwrap_or(&path_str);

    // If path is too long, only show project name and simplified parent path.
    // This check is first to avoid the expensive current_dir() call for long paths.
    if cleaned.len() > 50 {
        if let Some(project_name) = path.file_name() {
            let name = project_name.to_string_lossy();
            if let Some(parent) = path.parent() {
                if let Some(grandparent) = parent.file_name() {
                    return format!("📁 {}/{}", grandparent.to_string_lossy(), name);
                }
            }
            return format!("📁 {}", name);
        }
    }

    // If it's a subdirectory of current working directory, use relative path.
    // This involves a syscall and is therefore potentially slow.
    if let Ok(current_dir) = std::env::current_dir() {
        if let Ok(relative) = path.strip_prefix(&current_dir) {
            let rel_str = relative.to_string_lossy();
            if !rel_str.is_empty() {
                // OPTIMIZATION: Avoid allocation from `replace` if no backslashes are present.
                // Use Cow<str> to hold either a borrowed slice or a new owned String.
                let normalized_rel = if rel_str.contains('\\') {
                    Cow::Owned(rel_str.replace('\\', "/"))
                } else {
                    rel_str // This is already a Cow<'_, str>
                };
                return format!("📁 ./{}", normalized_rel);
            }
        }
    }

    // Fallback: Replace backslashes with forward slashes for aesthetics.
    // OPTIMIZATION: Apply the same Cow trick to avoid allocation if not needed.
    let normalized = if cleaned.contains('\\') {
        Cow::Owned(cleaned.replace('\\', "/"))
    } else {
        Cow::Borrowed(cleaned)
    };

    format!("📁 {}", normalized)
}

/// Beautify path display by converting long paths to a more concise format
#[must_use]
pub fn beautify_path(path: &std::path::Path) -> String {
    let path_str = path.to_string_lossy();

    // Remove Windows \\?\\ prefix
    let cleaned = path_str.strip_prefix(r"\\?\").unwrap_or(&path_str);

    // Get project name (last directory name)
    if let Some(project_name) = path.file_name() {
        let name = project_name.to_string_lossy();
        // If path is too long, only show project name and simplified parent path
        if cleaned.len() > 50 {
            if let Some(parent) = path.parent() {
                if let Some(grandparent) = parent.file_name() {
                    return format!("📁 {}/{}", grandparent.to_string_lossy(), name);
                }
            }
            return format!("📁 {}", name);
        }
    }

    // Replace backslashes with forward slashes (more aesthetic)
    let normalized = cleaned.replace('\\', "/");

    // If it's a subdirectory of current working directory, use relative path
    if let Ok(current_dir) = std::env::current_dir() {
        if let Ok(relative) = path.strip_prefix(&current_dir) {
            let rel_str = relative.to_string_lossy().replace('\\', "/");
            if !rel_str.is_empty() {
                return format!("📁 ./{}", rel_str);
            }
        }
    }

    format!("📁 {}", normalized)
}

/// A powerful suite of Rust development tools for the Model-Context Protocol (MCP)
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to the configuration file
    #[arg(long, global = true, default_value = "~/.rust-devtools-mcp.toml")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the MCP server and listen for requests
    Serve(ServerConfig),
    /// Manage projects in the workspace
    #[command(subcommand)]
    Projects(ProjectCommands),
    /// Show configuration information
    Config(ServerConfig),
}

#[derive(Parser, Debug)]
struct ServerConfig {
    /// Port to run the server on
    #[arg(short, long, default_value_t = 4000)]
    port: u16,

    /// Transport mode to use
    #[arg(short, long, default_value = "sse")]
    transport: String,

    /// Host to bind to
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
}

#[derive(Subcommand, Debug)]
enum ProjectCommands {
    /// Add a new project to the workspace
    Add {
        /// The root path of the project to add
        #[arg(required = true)]
        path: PathBuf,
    },
    /// Remove a project from the workspace
    #[command(alias = "rm")]
    Remove {
        /// The root path or project name to remove
        #[arg(required = true)]
        path_or_name: String,
    },
    /// List all projects currently in the workspace
    #[command(alias = "ls")]
    List,
    /// Clear all projects from the workspace
    Clear,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Expand tilde in the config path
    let config_path = PathBuf::from(shellexpand::tilde(&cli.config).to_string());

    let log_layer = tracing_subscriber::fmt::layer()
        .event_format(tracing_subscriber::fmt::format().compact().without_time())
        .fmt_fields(PrettyFields::new())
        .boxed();

    tracing_subscriber::registry()
        .with((EnvFilter::builder().try_from_env()).unwrap_or_else(|_| EnvFilter::new("rust_devtools_mcp=info")))
        .with(log_layer)
        .init();

    match cli.command {
        Commands::Serve(args) => run_serve(args, config_path).await,
        Commands::Projects(subcommand) => handle_projects(subcommand, config_path).await,
        Commands::Config(args) => handle_config(args, config_path).await,
    }
}

async fn run_serve(args: ServerConfig, config_path: PathBuf) -> Result<()> {
    info!("run_serve: Starting function");
    let (sender, receiver) = flume::unbounded();
    info!("run_serve: Created channels");

    // Parse transport type
    let transport = match args.transport.as_str() {
        "stdio" => crate::project::TransportType::Stdio,
        "sse" => crate::project::TransportType::Sse {
            host: args.host.clone(),
            port: args.port,
        },
        "streamable-http" => crate::project::TransportType::StreamableHttp {
            host: args.host.clone(),
            port: args.port,
        },
        _ => {
            error!("Invalid transport type: {}. Valid options: stdio, sse, streamable-http", args.transport);
            return Err(anyhow::anyhow!("Invalid transport type: {}", args.transport));
        }
    };

    let context = ContextType::new(transport, config_path, sender).await;
    info!("run_serve: Created context");
    context.load_config().await?;
    info!("run_serve: Loaded config");

    // Create config file watcher to support hot reloading
    let context_for_watcher = std::sync::Arc::new(tokio::sync::RwLock::new(context.clone()));
    let _config_watcher = config_watcher::ConfigWatcher::new(context_for_watcher)?;
    info!("Config file hot reloading enabled");

    let final_context = context.clone();

    // Run the MCP Server
    info!("run_serve: About to spawn MCP server task");
    let cloned_context = context.clone();
    let server_handle = tokio::spawn(async move {
        info!("Starting MCP server task...");
        if let Err(e) = run_server(cloned_context).await {
            error!("MCP Server exited with an error: {}", e);
        } else {
            info!("MCP Server task completed successfully");
        }
    });
    info!("run_serve: Spawned MCP server task");

    // Give the server task a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let main_loop_fut = async {
        info!("Starting server on {}:{}", context.address_information().0, context.address_information().1);
        info!("Using configuration file: {}", context.config_path().display());
        if context.project_descriptions().await.is_empty() {
            warn!(
                "No projects found. Once connected, add one using the 'manage_projects' tool with add_project_path parameter or the CLI: `rust-devtools-mcp projects add <path>`"
            );
        }
        info!("Cursor MCP JSON (for .cursor/mcp.json):\n---\n{}\n---", context.mcp_configuration());

        // Immediately show connection information
        println!();
        info!("-------------------------------------------------------------------");
        info!("🚀 Connection Ready! Your MCP server is now fully initialized.");
        info!("📋 To connect your MCP client (e.g., Cursor):");
        info!("1. Copy the JSON configuration from above (between the '---' lines).");
        info!("2. Paste it into your project's .cursor/mcp.json file.");
        info!("3. Start using Rust development tools in your editor!");
        info!("-------------------------------------------------------------------");
        println!();

        info!("⏳ Initializing LSP and indexing project... Please wait for completion.");

        context.request_project_descriptions();

        let mut is_indexing = false;
        let mut indexing_finished_sent = false;
        let mut last_indexing_activity = std::time::Instant::now();
        let mut any_stage_completed = false;

        info!("Initial state - indexing_finished_sent: {}", indexing_finished_sent);

        loop {
            while let Ok(notification) = receiver.try_recv() {
                let notification_path = notification.notification_path();

                // Clear the current line before processing any notification
                print!("\r\x1B[2K");
                use std::io::{self, Write};
                io::stdout().flush().unwrap();

                match &notification {
                    ContextNotification::Lsp(LspNotification::Indexing {
                        is_indexing: indexing,
                        progress,
                        project,
                    }) => {
                        if *indexing {
                            is_indexing = true;
                            last_indexing_activity = std::time::Instant::now();

                            let is_cache_priming = progress
                                .as_ref()
                                .map(|p| matches!(p.stage, crate::lsp::IndexingStage::CachePriming))
                                .unwrap_or(false);

                            if is_cache_priming {
                                print!("[{}] {}", beautify_path(&notification_path), notification.description());
                                io::stdout().flush().unwrap();
                            } else {
                                info!("[{}] {}", beautify_path(&notification_path), notification.description());
                            }
                        } else {
                            // This is a WorkDoneProgress::End event for a specific stage
                            let stage_name = progress
                                .as_ref()
                                .map(|p| format!("{:?}", p.stage))
                                .unwrap_or_else(|| "Unknown".to_string());

                            // Check if this is a known indexing stage completion
                            let is_indexing_stage = progress
                                .as_ref()
                                .map(|p| {
                                    matches!(
                                        p.stage,
                                        crate::lsp::IndexingStage::CachePriming
                                            | crate::lsp::IndexingStage::Indexing
                                            | crate::lsp::IndexingStage::Building
                                    )
                                })
                                .unwrap_or(false);

                            // Always clear the current line for stage completion
                            print!("\r\x1B[2K");
                            io::stdout().flush().unwrap();

                            // Show stage completion message
                            info!("[{}] ✅ {} Stage: Completed", beautify_path(project), stage_name);

                            // Mark indexing as finished for any known indexing stage completion
                            if is_indexing_stage {
                                is_indexing = false;
                                any_stage_completed = true;
                                last_indexing_activity = std::time::Instant::now();
                            }
                        }
                    }
                    other_notification => {
                        info!("[{}] {}", beautify_path(&notification_path), other_notification.description());
                    }
                }
            }

            // Check if indexing has been idle for a while and show final completion message
            if any_stage_completed && !indexing_finished_sent && !is_indexing {
                let idle_duration = last_indexing_activity.elapsed();
                if idle_duration >= std::time::Duration::from_secs(2) {
                    info!("✅ LSP Indexing: Finished");
                    indexing_finished_sent = true;
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    };

    tokio::select! {
        _ = main_loop_fut => {
            info!("Main loop finished unexpectedly.");
        },
        _ = signal::ctrl_c() => {
            info!("Ctrl+C received, shutting down...");
        }
        _ = server_handle => {
             info!("Server task finished unexpectedly.");
        }
    }

    final_context.shutdown_all().await;
    Ok(())
}

#[allow(clippy::unused_async)]
async fn handle_projects(command: ProjectCommands, config_path: PathBuf) -> Result<()> {
    use std::{collections::HashMap, fs};

    use crate::context::{SerConfig, SerProject};

    // For CLI commands, we work directly with the config file instead of starting services
    let mut config = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        toml::from_str::<SerConfig>(&content)?
    } else {
        SerConfig {
            projects: HashMap::new(),
        }
    };

    match command {
        ProjectCommands::Add {
            path,
        } => {
            let absolute_path = path.canonicalize()?;
            println!("✅ Adding project: {}", beautify_path(&absolute_path));

            let ser_project = SerProject {
                root: absolute_path.clone(),
                ignore_crates: vec![],
                rust_analyzer: None,
            };

            config.projects.insert(absolute_path.clone(), ser_project);

            // Save config
            if let Some(parent) = config_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let content = toml::to_string_pretty(&config)?;
            fs::write(&config_path, content)?;

            println!("🎉 Project successfully added to workspace!");
        }
        ProjectCommands::Remove {
            path_or_name,
        } => {
            // Try to find project by name first, then by path
            let mut found_project = None;
            let mut project_to_remove = None;

            // First, try to interpret as a project name
            for (root, project) in &config.projects {
                let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == path_or_name {
                    found_project = Some((root.clone(), project));
                    project_to_remove = Some(root.clone());
                    break;
                }
            }

            // If not found by name, try to interpret as a path
            if found_project.is_none() {
                if let Ok(path) = PathBuf::from(&path_or_name).canonicalize() {
                    if config.projects.contains_key(&path) {
                        found_project = Some((path.clone(), &config.projects[&path]));
                        project_to_remove = Some(path);
                    }
                }
            }

            if let Some((root, _)) = found_project {
                println!("🗑️  Removing project: {}", beautify_path(&root));

                if let Some(path_to_remove) = project_to_remove {
                    config.projects.remove(&path_to_remove);
                    // Save config
                    let content = toml::to_string_pretty(&config)?;
                    fs::write(&config_path, content)?;
                    println!("✅ Project successfully removed from workspace!");
                }
            } else {
                println!("⚠️  Project not found: '{}'", path_or_name);
                println!("💡 Use 'rust-devtools-mcp projects list' to see available projects");
            }
        }
        ProjectCommands::List => {
            if config.projects.is_empty() {
                println!("📭 No projects found in the workspace.");
                println!("💡 Add a project using: rust-devtools-mcp projects add <path>");
            } else {
                println!("📋 Projects in workspace:");
                for (root, project) in &config.projects {
                    let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("<unknown>");
                    println!("  • {} {}", name, beautify_path(&project.root));
                }
            }
        }
        ProjectCommands::Clear => {
            if config.projects.is_empty() {
                println!("📭 No projects found in the workspace. Nothing to clear.");
            } else {
                let project_count = config.projects.len();
                println!("🧹 Clearing {} project(s) from workspace...", project_count);

                config.projects.clear();

                // Save config
                let content = toml::to_string_pretty(&config)?;
                fs::write(&config_path, content)?;

                println!("✅ All projects successfully cleared from workspace!");
            }
        }
    }

    Ok(())
}

async fn handle_config(args: ServerConfig, config_path: PathBuf) -> Result<()> {
    // We don't need a real notifier for config display
    let (sender, _) = flume::unbounded();

    // Parse transport type
    let transport = match args.transport.as_str() {
        "stdio" => crate::project::TransportType::Stdio,
        "sse" => crate::project::TransportType::Sse {
            host: args.host.clone(),
            port: args.port,
        },
        "streamable-http" => crate::project::TransportType::StreamableHttp {
            host: args.host.clone(),
            port: args.port,
        },
        _ => {
            error!("Invalid transport type: {}. Valid options: stdio, sse, streamable-http", args.transport);
            return Err(anyhow::anyhow!("Invalid transport type: {}", args.transport));
        }
    };

    let context = ContextType::new(transport, config_path.clone(), sender).await;

    println!("⚙️  Configuration file: {}", beautify_path(&config_path));
    println!("📋 MCP Configuration for Cursor (.cursor/mcp.json):");
    println!("{}", context.mcp_configuration());

    Ok(())
}

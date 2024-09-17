mod consts;
mod logging;
mod protocol;
mod python;
mod server;
mod temporary_recipe;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use clap_verbosity_flag::{InfoLevel, Verbosity};
use miette::IntoDiagnostic;
use pixi_build_types::{
    procedures::{
        conda_build::{CondaBuildParams, CondaOutputIdentifier},
        conda_metadata::{CondaMetadataParams, CondaMetadataResult},
    },
    ChannelConfiguration,
};
use rattler_build::console_utils::{get_default_env_filter, LoggingOutputHandler};
use rattler_conda_types::ChannelConfig;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    protocol::{Protocol, ProtocolFactory},
    python::PythonBuildBackend,
    server::Server,
};

#[allow(missing_docs)]
#[derive(Parser)]
pub struct App {
    #[clap(subcommand)]
    command: Option<Commands>,

    /// The port to expose the json-rpc server on. If not specified will
    /// communicate with stdin/stdout.
    #[clap(long)]
    http_port: Option<u16>,

    /// Enable verbose logging.
    #[command(flatten)]
    verbose: Verbosity<InfoLevel>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// store data as key value pair
    GetCondaMetadata {
        #[clap(env, long, env = "PIXI_PROJECT_MANIFEST", default_value = consts::PROJECT_MANIFEST)]
        manifest_path: PathBuf,
    },
    Build {
        #[clap(env, long, env = "PIXI_PROJECT_MANIFEST", default_value = consts::PROJECT_MANIFEST)]
        manifest_path: PathBuf,
    },
}

#[tokio::main]
pub async fn main() {
    if let Err(err) = actual_main().await {
        eprintln!("{err:?}");
        std::process::exit(1);
    }
}

async fn run_server<T: ProtocolFactory>(port: Option<u16>, protocol: T) -> miette::Result<()> {
    let server = Server::new(protocol);
    if let Some(port) = port {
        server.run_over_http(port)
    } else {
        server.run().await
    }
}

async fn actual_main() -> miette::Result<()> {
    let args = App::parse();

    // Setup logging
    let log_handler = LoggingOutputHandler::default();
    let registry = tracing_subscriber::registry()
        .with(get_default_env_filter(args.verbose.log_level_filter()).into_diagnostic()?);
    registry.with(log_handler.clone()).init();

    match args.command {
        None => run_server(args.http_port, PythonBuildBackend::factory(log_handler)).await,
        Some(Commands::Build { manifest_path }) => build(log_handler, &manifest_path).await,
        Some(Commands::GetCondaMetadata { manifest_path }) => {
            let metadata = get_conda_metadata(log_handler, &manifest_path).await?;
            println!("{}", serde_yaml::to_string(&metadata).unwrap());
            Ok(())
        }
    }
}

async fn get_conda_metadata(
    logging_output_handler: LoggingOutputHandler,
    manifest_path: &Path,
) -> miette::Result<CondaMetadataResult> {
    let channel_config = ChannelConfig::default_with_root_dir(
        manifest_path
            .parent()
            .expect("manifest should always reside in a directory")
            .to_path_buf(),
    );

    let backend = PythonBuildBackend::new(manifest_path, logging_output_handler)?;
    backend
        .get_conda_metadata(CondaMetadataParams {
            target_platform: None,
            channel_base_urls: None,
            channel_configuration: ChannelConfiguration {
                base_url: channel_config.channel_alias,
            },
        })
        .await
}

async fn build(
    logging_output_handler: LoggingOutputHandler,
    manifest_path: &Path,
) -> miette::Result<()> {
    let channel_config = ChannelConfig::default_with_root_dir(
        manifest_path
            .parent()
            .expect("manifest should always reside in a directory")
            .to_path_buf(),
    );

    let backend = PythonBuildBackend::new(manifest_path, logging_output_handler)?;
    let result = backend
        .build_conda(CondaBuildParams {
            target_platform: None,
            channel_base_urls: None,
            channel_configuration: ChannelConfiguration {
                base_url: channel_config.channel_alias,
            },
            output: CondaOutputIdentifier::default(),
        })
        .await?;

    eprintln!("Successfully build '{}'", result.path.display());

    Ok(())
}

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use clap_verbosity_flag::{InfoLevel, Verbosity};
use miette::IntoDiagnostic;
use pixi_build_types::{
    procedures::{
        conda_build::{CondaBuildParams, CondaOutputIdentifier},
        conda_metadata::{CondaMetadataParams, CondaMetadataResult},
        initialize::InitializeParams,
    },
    ChannelConfiguration, FrontendCapabilities,
};
use rattler_build::console_utils::{get_default_env_filter, LoggingOutputHandler};
use rattler_conda_types::ChannelConfig;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    consts,
    protocol::{Protocol, ProtocolFactory},
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
    CondaBuild {
        #[clap(env, long, env = "PIXI_PROJECT_MANIFEST", default_value = consts::PROJECT_MANIFEST)]
        manifest_path: PathBuf,
    },
}

async fn run_server<T: ProtocolFactory>(port: Option<u16>, protocol: T) -> miette::Result<()> {
    let server = Server::new(protocol);
    if let Some(port) = port {
        server.run_over_http(port)
    } else {
        server.run().await
    }
}

pub async fn main<T: ProtocolFactory, F: FnOnce(LoggingOutputHandler) -> T>(
    factory: F,
) -> miette::Result<()> {
    let args = App::parse();

    // Setup logging
    let log_handler = LoggingOutputHandler::default();
    let registry = tracing_subscriber::registry()
        .with(get_default_env_filter(args.verbose.log_level_filter()).into_diagnostic()?);
    registry.with(log_handler.clone()).init();

    let factory = factory(log_handler);

    match args.command {
        None => run_server(args.http_port, factory).await,
        Some(Commands::CondaBuild { manifest_path }) => build(factory, &manifest_path).await,
        Some(Commands::GetCondaMetadata { manifest_path }) => {
            let metadata = get_conda_metadata(factory, &manifest_path).await?;
            println!("{}", serde_yaml::to_string(&metadata).unwrap());
            Ok(())
        }
    }
}

async fn get_conda_metadata(
    factory: impl ProtocolFactory,
    manifest_path: &Path,
) -> miette::Result<CondaMetadataResult> {
    let channel_config = ChannelConfig::default_with_root_dir(
        manifest_path
            .parent()
            .expect("manifest should always reside in a directory")
            .to_path_buf(),
    );

    let (protocol, _initialize_result) = factory
        .initialize(InitializeParams {
            manifest_path: manifest_path.to_path_buf(),
            capabilities: FrontendCapabilities {},
        })
        .await?;

    protocol
        .get_conda_metadata(CondaMetadataParams {
            target_platform: None,
            channel_base_urls: None,
            channel_configuration: ChannelConfiguration {
                base_url: channel_config.channel_alias,
            },
        })
        .await
}

async fn build(factory: impl ProtocolFactory, manifest_path: &Path) -> miette::Result<()> {
    let channel_config = ChannelConfig::default_with_root_dir(
        manifest_path
            .parent()
            .expect("manifest should always reside in a directory")
            .to_path_buf(),
    );

    let (protocol, _initialize_result) = factory
        .initialize(InitializeParams {
            manifest_path: manifest_path.to_path_buf(),
            capabilities: FrontendCapabilities {},
        })
        .await?;

    let result = protocol
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

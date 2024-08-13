mod consts;

use std::{
    collections::BTreeMap,
    io::BufWriter,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use chrono::Utc;
use clap::{Parser, Subcommand};
use clap_verbosity_flag::{InfoLevel, Verbosity};
use jsonrpc_core::{to_value, Error, IoHandler};
use jsonrpc_http_server::jsonrpc_core::Params;
use miette::{Context, IntoDiagnostic};
use parking_lot::Mutex;
use pixi_build_types::{
    procedures::initialize::{InitializeParams, InitializeResult},
    BackendCapabilities,
};
use pixi_manifest::{Dependencies, Manifest, SpecType};
use pixi_spec::PixiSpec;
use rattler_build::{
    build::run_build,
    console_utils::{get_default_env_filter, LoggingOutputHandler},
    hash::HashInfo,
    metadata::{BuildConfiguration, Directories, Output, PackagingSettings},
    recipe::{
        parser::{Build, Dependency, Package, PathSource, Requirements, ScriptContent, Source},
        Recipe,
    },
    tool_configuration::{Configuration, SkipExisting},
};
use rattler_conda_types::{
    package::ArchiveType, ChannelConfig, MatchSpec, NoArchType, PackageName, Platform,
    VersionWithSource,
};
use rattler_package_streaming::write::CompressionLevel;
use reqwest::{Client, Url};
use tempfile::tempdir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[allow(missing_docs)]
#[derive(Parser)]
#[clap(version)]
pub struct App {
    #[clap(subcommand)]
    command: Option<Commands>,

    /// The port to expose the json-rpc server on. If not specified will
    /// communicate with stdin/stdout.
    #[clap(long, conflicts_with = "command")]
    http_port: Option<u16>,

    /// Enable verbose logging.
    #[command(flatten)]
    verbose: Verbosity<InfoLevel>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// store data as key value pair
    GetMetadata {
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

struct BuildServer {
    manifest: Manifest,
    manifest_root: PathBuf,
}

impl BuildServer {
    pub fn new(source_dir: &Path) -> miette::Result<Self> {
        // Load the manifest from the source directory
        let manifest = Manifest::from_path(&source_dir)
            .with_context(|| format!("failed to parse manifest from {}", source_dir.display()))?;

        Ok(Self {
            manifest,
            manifest_root: source_dir.to_path_buf(),
        })
    }
}

async fn run_server(port: Option<u16>) -> miette::Result<()> {
    let build_server = Arc::new(Mutex::new(None));

    // Construct a server
    let mut io = IoHandler::new();
    io.add_sync_method("initialize", move |params: Params| {
        let params: InitializeParams = params.parse()?;

        let mut build_server = build_server.lock();
        if build_server.is_some() {
            return Err(Error::invalid_request());
        }

        *build_server = Some(
            BuildServer::new(&params.source_dir)
                .map_err(|e| Error::invalid_params(e.to_string()))?,
        );

        Ok(to_value(InitializeResult {
            capabilities: BackendCapabilities {
                provides_conda_metadata: Some(true),
            },
        })
        .expect("failed to convert to json"))
    });

    if let Some(port) = port {
        jsonrpc_http_server::ServerBuilder::new(io)
            .start_http(&SocketAddr::from(([127, 0, 0, 1], port)))
            .into_diagnostic()?
            .wait()
    } else {
        jsonrpc_stdio_server::ServerBuilder::new(io).build().await;
    }

    Ok(())
}

async fn actual_main() -> miette::Result<()> {
    let args = App::parse();

    // Setup logging
    let log_handler = LoggingOutputHandler::default();
    let registry = tracing_subscriber::registry()
        .with(get_default_env_filter(args.verbose.log_level_filter()).into_diagnostic()?);
    registry.with(log_handler.clone()).init();

    match args.command {
        None => run_server(args.http_port).await,
        Some(Commands::Build { manifest_path }) => build(log_handler, &manifest_path).await,
        Some(Commands::GetMetadata { .. }) => unimplemented!(),
    }
}

async fn build(
    logging_output_handler: LoggingOutputHandler,
    manifest_path: &Path,
) -> miette::Result<()> {
    // Load the manifest
    let manifest = Manifest::from_path(&manifest_path)
        .with_context(|| format!("failed to parse manifest from {}", manifest_path.display()))?;
    let manifest_root = manifest_path
        .parent()
        .expect("the project manifest must reside in a directory");

    // Parse the package name from the manifest
    let Some(name) = manifest.parsed.project.name.clone() else {
        miette::bail!("a 'name' field is required in the project manifest");
    };
    let name = PackageName::from_str(&name).into_diagnostic()?;

    // Parse the package version from the manifest
    let Some(version) = manifest.parsed.project.version.clone() else {
        miette::bail!("a 'version' field is required in the project manifest");
    };

    // TODO: Variants???
    let variants = BTreeMap::default();

    // TODO: NoArchType???
    let noarch_type = NoArchType::python();

    // TODO: Setup defaults
    let output_dir = tempdir()
        .into_diagnostic()
        .context("failed to create temporary directory")?;
    std::fs::create_dir_all(&output_dir)
        .into_diagnostic()
        .context("failed to create output directory")?;
    let directories = Directories::setup(
        name.as_normalized(),
        manifest_path,
        output_dir.path(),
        false,
        &Utc::now(),
    )
    .into_diagnostic()
    .context("failed to setup build directories")?;

    // TODO: Read from config / project.
    let channel_config = ChannelConfig::default_with_root_dir(manifest_root.to_path_buf());

    let requirements = requirements_from_manifest(&manifest, &channel_config);
    let channels = channels_from_manifest(&manifest, &channel_config);

    let host_platform = Platform::current();
    let build_platform = Platform::current();

    let hash = HashInfo::from_variant(&variants, &noarch_type);
    let build_number = 0;
    let build_string = format!("{}_{}", &hash.hash, build_number);

    let output = Output {
        recipe: Recipe {
            schema_version: 1,
            package: Package {
                version: VersionWithSource::from(version),
                name,
            },
            cache: None,
            source: vec![Source::Path(PathSource {
                // TODO: How can we use a git source?
                path: manifest_root.to_path_buf(),
                sha256: None,
                md5: None,
                patches: vec![],
                target_directory: None,
                file_name: None,
                use_gitignore: true,
            })],
            build: Build {
                number: build_number,
                string: Some(build_string),

                // skip: Default::default(),
                script: ScriptContent::Commands(
                    if build_platform.is_windows() {
                        vec![
                            "%PYTHON% -m pip install --ignore-installed --no-deps --no-build-isolation . -vv".to_string(),
                            "if errorlevel 1 exit 1".to_string()]
                    } else {
                        vec!["$PYTHON -m pip install --ignore-installed --no-deps --no-build-isolation . -vv".to_string()]
                    })
                    .into(),
                noarch: noarch_type,

                // TODO: Python is not exposed properly
                //python: Default::default(),
                // dynamic_linking: Default::default(),
                // always_copy_files: Default::default(),
                // always_include_files: Default::default(),
                // merge_build_and_host_envs: false,
                // variant: Default::default(),
                // prefix_detection: Default::default(),
                // post_process: vec![],
                // files: Default::default(),
                ..Build::default()
            },
            // TODO read from manifest
            requirements,
            tests: vec![],
            about: Default::default(),
            extra: Default::default(),
        },
        build_configuration: BuildConfiguration {
            // TODO: NoArch??
            target_platform: Platform::NoArch,
            host_platform,
            build_platform,
            hash,
            variant: Default::default(),
            directories,
            channels,
            channel_priority: Default::default(),
            solve_strategy: Default::default(),
            timestamp: chrono::Utc::now(),
            subpackages: Default::default(), // TODO: ???
            packaging_settings: PackagingSettings::from_args(
                ArchiveType::Conda,
                CompressionLevel::default(),
            ),
            store_recipe: true,
            force_colors: true,
        },
        finalized_dependencies: None,
        finalized_cache_dependencies: None,
        finalized_sources: None,
        build_summary: Arc::default(),
        system_tools: Default::default(),
    };

    let tool_config = Configuration {
        fancy_log_handler: logging_output_handler,
        client: reqwest_middleware::ClientWithMiddleware::from(Client::default()),
        no_clean: false,
        no_test: false,
        use_zstd: true,
        use_bz2: true,
        render_only: false,
        skip_existing: SkipExisting::None,
        channel_config,
        compression_threads: None,
    };

    let (recipe_file, recipe_path) = tempfile::Builder::new()
        .prefix(".rendered-recipe")
        .suffix(".yaml")
        .tempfile_in(output_dir)
        .into_diagnostic()
        .context("failed to create temporary file for recipe")?
        .into_parts();

    // Write the recipe back to a file
    serde_yaml::to_writer(BufWriter::new(recipe_file), &output.recipe)
        .into_diagnostic()
        .context("failed to write recipe to temporary file")?;

    let (_output, package) = run_build(output, &tool_config).await?;
    eprintln!("Successfully build '{}'", package.display());

    // Remove the temporary recipe file.
    std::fs::remove_file(recipe_path)
        .into_diagnostic()
        .context("failed to remove temporary recipe file")?;

    Ok(())
}

/// Get the requirements for a default feature
fn requirements_from_manifest(manifest: &Manifest, channel_config: &ChannelConfig) -> Requirements {
    let mut requirements = Requirements::default();
    let default_features = vec![manifest.default_feature()];

    // Get all different feature types
    let run_dependencies = Dependencies::from(
        default_features
            .iter()
            .filter_map(|f| f.dependencies(Some(SpecType::Run), None)),
    );
    let host_dependencies = Dependencies::from(
        default_features
            .iter()
            .filter_map(|f| f.dependencies(Some(SpecType::Host), None)),
    );
    let build_dependencies = Dependencies::from(
        default_features
            .iter()
            .filter_map(|f| f.dependencies(Some(SpecType::Build), None)),
    );

    requirements.build = dependencies_into_matchspecs(build_dependencies, channel_config)
        .into_iter()
        .map(Dependency::Spec)
        .collect();
    requirements.host = dependencies_into_matchspecs(host_dependencies, channel_config)
        .into_iter()
        .map(Dependency::Spec)
        .collect();
    requirements.run = dependencies_into_matchspecs(run_dependencies, channel_config)
        .into_iter()
        .map(Dependency::Spec)
        .collect();

    // TODO: If the host requirements don't contain pip or python add those

    requirements
}

fn channels_from_manifest(manifest: &Manifest, channel_config: &ChannelConfig) -> Vec<Url> {
    // TODO: Improve
    manifest
        .parsed
        .project
        .channels
        .iter()
        .map(|c| c.channel.clone().into_base_url(channel_config))
        .collect()
}

fn dependencies_into_matchspecs(
    deps: Dependencies<PackageName, PixiSpec>,
    channel_config: &ChannelConfig,
) -> Vec<MatchSpec> {
    deps.into_specs()
        .filter_map(|(name, spec)| {
            spec.try_into_nameless_match_spec(channel_config)
                .expect("failed to convert spec into match spec")
                .map(|spec| MatchSpec::from_nameless(spec, Some(name)))
        })
        .collect()
}

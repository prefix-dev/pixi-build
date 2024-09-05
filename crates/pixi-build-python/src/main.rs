mod consts;

use std::{
    collections::BTreeMap,
    future::Future,
    io::BufWriter,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use chrono::Utc;
use clap::{Parser, Subcommand};
use clap_verbosity_flag::{InfoLevel, Verbosity};
use jsonrpc_core::{serde_json, to_value, Error, IoHandler};
use jsonrpc_http_server::jsonrpc_core::Params;
use miette::{Context, IntoDiagnostic, JSONReportHandler};
use parking_lot::Mutex;
use pixi_build_types::{
    procedures,
    procedures::{
        conda_metadata::{CondaMetadataParams, CondaMetadataResult},
        initialize::{InitializeParams, InitializeResult},
    },
    BackendCapabilities, CondaPackageMetadata,
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
    render::resolved_dependencies::DependencyInfo,
    tool_configuration::Configuration,
};
use rattler_conda_types::{
    package::ArchiveType, ChannelConfig, MatchSpec, NoArchType, PackageName, Platform, Version,
    VersionWithSource,
};
use rattler_package_streaming::write::CompressionLevel;
use reqwest::Url;
use tempfile::tempdir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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

fn convert_error(err: miette::Report) -> jsonrpc_core::Error {
    let rendered = JSONReportHandler::new();
    let mut json_str = String::new();
    rendered
        .render_report(&mut json_str, err.as_ref())
        .expect("failed to convert error to json");
    let data = serde_json::from_str(&json_str).expect("failed to parse json error");
    jsonrpc_core::Error {
        code: jsonrpc_core::ErrorCode::ServerError(-32000),
        message: err.to_string(),
        data: Some(data),
    }
}

struct BuildServer {
    manifest: Manifest,
    logging_output_handler: LoggingOutputHandler,
}

impl BuildServer {
    pub fn new(
        manifest_path: &Path,
        logging_output_handler: LoggingOutputHandler,
    ) -> miette::Result<Self> {
        // Load the manifest from the source directory
        let manifest = Manifest::from_path(&manifest_path).with_context(|| {
            format!("failed to parse manifest from {}", manifest_path.display())
        })?;

        Ok(Self {
            manifest,
            logging_output_handler,
        })
    }

    pub fn manifest_root(&self) -> &Path {
        self.manifest
            .path
            .parent()
            .expect("manifest should always reside in a directory")
    }

    pub async fn get_conda_metadata(
        &self,
        params: CondaMetadataParams,
    ) -> miette::Result<CondaMetadataResult> {
        let channel_config = ChannelConfig {
            channel_alias: params.channel_configuration.base_url,
            root_dir: self.manifest_root().to_path_buf(),
        };
        let channels = params
            .channel_base_urls
            .unwrap_or_else(|| channels_from_manifest(&self.manifest, &channel_config));

        get_conda_metadata_from_manifest(
            &self.manifest,
            &channel_config,
            channels,
            self.logging_output_handler.clone(),
        )
        .await
    }
}

async fn run_server(
    port: Option<u16>,
    logging_output_handler: LoggingOutputHandler,
) -> miette::Result<()> {
    let build_server = Arc::new(Mutex::new(None));

    // Construct a server
    let mut io = IoHandler::new();

    let initialize_build_server = build_server.clone();
    io.add_sync_method(
        procedures::initialize::METHOD_NAME,
        move |params: Params| {
            let params: InitializeParams = params.parse()?;

            let mut build_server = initialize_build_server.lock();
            if build_server.is_some() {
                return Err(Error::invalid_request());
            }

            *build_server = Some(Arc::new(
                BuildServer::new(&params.manifest_path, logging_output_handler.clone())
                    .map_err(convert_error)?,
            ));

            Ok(to_value(InitializeResult {
                capabilities: BackendCapabilities {
                    provides_conda_metadata: Some(true),
                },
            })
            .expect("failed to convert to json"))
        },
    );

    io.add_method(
        procedures::conda_metadata::METHOD_NAME,
        move |params: Params| {
            let build_server = build_server.clone();
            let build_server = build_server.lock().as_ref().cloned();
            async move {
                let params: CondaMetadataParams = params.parse()?;

                build_server
                    .ok_or_else(|| Error::invalid_request())?
                    .get_conda_metadata(params)
                    .await
                    .map(|value| to_value(value).expect("failed to convert to json"))
                    .map_err(convert_error)
            }
        },
    );

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
        None => run_server(args.http_port, log_handler).await,
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
    let manifest = Manifest::from_path(&manifest_path)
        .with_context(|| format!("failed to parse manifest from {}", manifest_path.display()))?;
    let channel_config = ChannelConfig::default_with_root_dir(
        manifest_path
            .parent()
            .expect("manifest should always reside in a directory")
            .to_path_buf(),
    );
    let channels = channels_from_manifest(&manifest, &channel_config);

    get_conda_metadata_from_manifest(&manifest, &channel_config, channels, logging_output_handler)
        .await
}

async fn get_conda_metadata_from_manifest(
    manifest: &Manifest,
    channel_config: &ChannelConfig,
    channels: Vec<Url>,
    logging_output_handler: LoggingOutputHandler,
) -> miette::Result<CondaMetadataResult> {
    // TODO: Determine how and if we can determine this from the manifest.
    let recipe = manifest_to_recipe(&manifest, &channel_config)?;
    let output = Output {
        build_configuration: manifest_to_build_configuration(&manifest, &recipe, channels).await?,
        recipe,
        finalized_dependencies: None,
        finalized_cache_dependencies: None,
        finalized_sources: None,
        build_summary: Arc::default(),
        system_tools: Default::default(),
        extra_meta: None,
    };
    let tool_config = get_tool_configuration(logging_output_handler, &channel_config)?;

    let temp_recipe = TemporaryRenderedRecipe::from_output(&output)?;
    let output = temp_recipe
        .within_context_async(move || async move {
            output
                .resolve_dependencies(&tool_config)
                .await
                .into_diagnostic()
        })
        .await?;

    let finalized_deps = &output
        .finalized_dependencies
        .as_ref()
        .expect("dependencies should be resolved at this point")
        .run;

    Ok(CondaMetadataResult {
        packages: vec![CondaPackageMetadata {
            name: output.name().clone(),
            version: output.version().clone().into(),
            build: output.build_string().into_owned(),
            build_number: output.recipe.build.number,
            subdir: output.build_configuration.target_platform,
            depends: finalized_deps
                .depends
                .iter()
                .map(DependencyInfo::spec)
                .cloned()
                .collect(),
            constraints: finalized_deps
                .constraints
                .iter()
                .map(DependencyInfo::spec)
                .cloned()
                .collect(),
            license: output.recipe.about.license.map(|l| l.to_string()),
            license_family: output.recipe.about.license_family,
            noarch: output.recipe.build.noarch,
        }],
    })
}

async fn build(
    logging_output_handler: LoggingOutputHandler,
    manifest_path: &Path,
) -> miette::Result<()> {
    let manifest = Manifest::from_path(&manifest_path)
        .with_context(|| format!("failed to parse manifest from {}", manifest_path.display()))?;
    let channel_config = ChannelConfig::default_with_root_dir(
        manifest_path
            .parent()
            .expect("manifest should always reside in a directory")
            .to_path_buf(),
    );
    let channels = channels_from_manifest(&manifest, &channel_config);

    build_manifest(&manifest, &channel_config, channels, logging_output_handler).await
}

async fn build_manifest(
    manifest: &Manifest,
    channel_config: &ChannelConfig,
    channels: Vec<Url>,
    logging_output_handler: LoggingOutputHandler,
) -> miette::Result<()> {
    let recipe = manifest_to_recipe(&manifest, &channel_config)?;
    let output = Output {
        build_configuration: manifest_to_build_configuration(&manifest, &recipe, channels).await?,
        recipe,
        finalized_dependencies: None,
        finalized_cache_dependencies: None,
        finalized_sources: None,
        build_summary: Arc::default(),
        system_tools: Default::default(),
        extra_meta: None,
    };
    let tool_config = get_tool_configuration(logging_output_handler, &channel_config)?;

    let temp_recipe = TemporaryRenderedRecipe::from_output(&output)?;
    let (_output, package) = temp_recipe
        .within_context_async(move || async move { run_build(output, &tool_config).await })
        .await?;
    eprintln!("Successfully build '{}'", package.display());

    Ok(())
}

fn get_tool_configuration(
    logging_output_handler: LoggingOutputHandler,
    channel_config: &ChannelConfig,
) -> miette::Result<Configuration> {
    Ok(Configuration::builder()
        .with_logging_output_handler(logging_output_handler)
        .with_channel_config(channel_config.clone())
        .finish())
}

async fn manifest_to_build_configuration(
    manifest: &Manifest,
    recipe: &Recipe,
    channels: Vec<Url>,
) -> miette::Result<BuildConfiguration> {
    // Parse the package name from the manifest
    let Some(name) = manifest.parsed.project.name.clone() else {
        miette::bail!("a 'name' field is required in the project manifest");
    };
    let name = PackageName::from_str(&name).into_diagnostic()?;

    // TODO: Setup defaults
    let output_dir = tempdir()
        .into_diagnostic()
        .context("failed to create temporary directory")?;
    std::fs::create_dir_all(&output_dir)
        .into_diagnostic()
        .context("failed to create output directory")?;
    let directories = Directories::setup(
        name.as_normalized(),
        manifest.path.as_path(),
        output_dir.path(),
        false,
        &Utc::now(),
    )
    .into_diagnostic()
    .context("failed to setup build directories")?;

    let host_platform = Platform::current();
    let build_platform = Platform::current();

    let variant = BTreeMap::new();

    Ok(BuildConfiguration {
        // TODO: NoArch??
        target_platform: Platform::NoArch,
        host_platform,
        build_platform,
        hash: HashInfo::from_variant(&variant, &recipe.build.noarch),
        variant,
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
    })
}

fn manifest_to_recipe(
    manifest: &Manifest,
    channel_config: &ChannelConfig,
) -> miette::Result<Recipe> {
    let manifest_root = manifest
        .path
        .parent()
        .expect("the project manifest must reside in a directory");

    // Parse the package name from the manifest
    let Some(name) = manifest.parsed.project.name.clone() else {
        miette::bail!("a 'name' field is required in the project manifest");
    };
    let name = PackageName::from_str(&name).into_diagnostic()?;

    // Parse the package version from the manifest. The version is optional, so we
    // default to "0dev0" if it is not present.
    let version = manifest
        .parsed
        .project
        .version
        .clone()
        .unwrap_or_else(|| Version::from_str("0dev0").unwrap());

    // TODO: NoArchType???
    let noarch_type = NoArchType::python();

    // TODO: Read from config / project.
    let requirements = requirements_from_manifest(&manifest, &channel_config);
    let build_platform = Platform::current();
    let build_number = 0;

    Ok(Recipe {
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
            string: Default::default(),

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
    })
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
    let mut host_dependencies = Dependencies::from(
        default_features
            .iter()
            .filter_map(|f| f.dependencies(Some(SpecType::Host), None)),
    );
    let build_dependencies = Dependencies::from(
        default_features
            .iter()
            .filter_map(|f| f.dependencies(Some(SpecType::Build), None)),
    );

    // Ensure python and pip are available in the host dependencies section.
    for pkg_name in ["pip", "python"] {
        if host_dependencies.contains_key(pkg_name) {
            // If the host dependencies already contain the package, we don't need to add it
            // again.
            continue;
        }

        if let Some(run_requirements) = run_dependencies.get(pkg_name) {
            // Copy the run requirements to the host requirements.
            for req in run_requirements {
                host_dependencies.insert(PackageName::from_str(pkg_name).unwrap(), req.clone());
            }
        } else {
            host_dependencies.insert(
                PackageName::from_str(pkg_name).unwrap(),
                PixiSpec::default(),
            );
        }
    }

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

/// A helper struct that owns a temporary file containing a rendered recipe.
/// If `finish` is not called, the temporary file will stay on disk for
/// debugging purposes.
struct TemporaryRenderedRecipe {
    file: PathBuf,
}

impl TemporaryRenderedRecipe {
    pub fn from_output(output: &Output) -> miette::Result<Self> {
        // Ensure that the output directory exists
        std::fs::create_dir_all(&output.build_configuration.directories.output_dir)
            .into_diagnostic()
            .context("failed to create output directory")?;

        let (recipe_file, recipe_path) = tempfile::Builder::new()
            .prefix(".rendered-recipe")
            .suffix(".yaml")
            .tempfile_in(&output.build_configuration.directories.output_dir)
            .into_diagnostic()
            .context("failed to create temporary file for recipe")?
            .into_parts();

        // Write the recipe back to a file
        serde_yaml::to_writer(BufWriter::new(recipe_file), &output.recipe)
            .into_diagnostic()
            .context("failed to write recipe to temporary file")?;

        Ok(Self {
            file: recipe_path.keep().unwrap(),
        })
    }

    pub fn within_context<R, F: FnOnce() -> miette::Result<R>>(
        self,
        operation: F,
    ) -> miette::Result<R> {
        let result = operation()?;
        std::fs::remove_file(self.file)
            .into_diagnostic()
            .context("failed to remove temporary recipe file")?;
        Ok(result)
    }

    pub async fn within_context_async<
        R,
        Fut: Future<Output = miette::Result<R>>,
        F: FnOnce() -> Fut,
    >(
        self,
        operation: F,
    ) -> miette::Result<R> {
        let result = operation().await?;
        std::fs::remove_file(self.file)
            .into_diagnostic()
            .context("failed to remove temporary recipe file")?;
        Ok(result)
    }
}

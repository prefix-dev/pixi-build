mod consts;

use std::{collections::BTreeMap, io::BufWriter, path::PathBuf, str::FromStr, sync::Arc};

use chrono::Utc;
use clap::Parser;
use clap_verbosity_flag::{InfoLevel, Verbosity};
use miette::{Context, IntoDiagnostic};
use pixi_manifest::{Manifest, SpecType};
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
};
use rattler_package_streaming::write::CompressionLevel;
use reqwest::{Client, Url};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[allow(missing_docs)]
#[derive(Parser)]
#[clap(version)]
pub struct App {
    /// The path to the manifest file
    #[clap(env, long, env = "PIXI_PROJECT_MANIFEST", default_value = consts::PROJECT_MANIFEST)]
    manifest_path: PathBuf,

    /// Enable verbose logging.
    #[command(flatten)]
    verbose: Verbosity<InfoLevel>,
}

/// Get the requirements for a default feature
fn requirements_from_manifest(manifest: &Manifest) -> Requirements {
    let mut requirements = Requirements::default();
    let default_features = vec![manifest.default_feature()];

    // Get all different feature types
    let run_dependencies = default_features
        .iter()
        .filter_map(|f| f.dependencies(Some(SpecType::Run), None))
        .collect::<Vec<_>>();
    let host_dependencies = default_features
        .iter()
        .filter_map(|f| f.dependencies(Some(SpecType::Host), None))
        .collect::<Vec<_>>();
    let build_dependencies = default_features
        .iter()
        .filter_map(|f| f.dependencies(Some(SpecType::Build), None))
        .collect::<Vec<_>>();

    requirements.build = build_dependencies
        .into_iter()
        .flat_map(|d| d.into_owned().into_iter())
        .map(|(name, spec)| MatchSpec::from_nameless(spec, Some(name)))
        .map(Dependency::Spec)
        .collect();

    requirements.host = host_dependencies
        .into_iter()
        .flat_map(|d| d.into_owned().into_iter())
        .map(|(name, spec)| MatchSpec::from_nameless(spec, Some(name)))
        .map(Dependency::Spec)
        .collect();

    requirements.run = run_dependencies
        .into_iter()
        .flat_map(|d| d.into_owned().into_iter())
        .map(|(name, spec)| MatchSpec::from_nameless(spec, Some(name)))
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

#[tokio::main]
pub async fn main() {
    if let Err(err) = actual_main().await {
        eprintln!("{err:?}");
        std::process::exit(1);
    }
}

async fn actual_main() -> miette::Result<()> {
    let args = App::parse();

    // Setup logging
    let log_handler = LoggingOutputHandler::default();
    let registry = tracing_subscriber::registry()
        .with(get_default_env_filter(args.verbose.log_level_filter()).into_diagnostic()?);
    registry.with(log_handler.clone()).init();

    // Load the manifest
    let manifest = Manifest::from_path(&args.manifest_path).with_context(|| {
        format!(
            "failed to parse manifest from {}",
            args.manifest_path.display()
        )
    })?;
    let manifest_root = args
        .manifest_path
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
    let output_dir = std::env::current_dir()
        .expect("failed to get current directory")
        .join("pixi-build-python-output");
    std::fs::create_dir_all(&output_dir)
        .into_diagnostic()
        .context("failed to create output directory")?;
    let directories = Directories::setup(
        name.as_normalized(),
        args.manifest_path.as_path(),
        &output_dir,
        false,
        &Utc::now(),
    )
    .into_diagnostic()
    .context("failed to setup build directories")?;

    // TODO: Read from config / project.
    let channel_config = ChannelConfig::default_with_root_dir(manifest_root.to_path_buf());

    let requirements = requirements_from_manifest(&manifest);
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
                version: version.into(),
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
        fancy_log_handler: log_handler,
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

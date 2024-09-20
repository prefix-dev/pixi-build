use std::{collections::BTreeMap, path::Path, str::FromStr, sync::Arc};

use chrono::Utc;
use miette::{Context, IntoDiagnostic};
use pixi_build_backend::{
    protocol::{Protocol, ProtocolFactory},
    utils::TemporaryRenderedRecipe,
};
use pixi_build_types::{
    procedures::{
        conda_build::{CondaBuildParams, CondaBuildResult},
        conda_metadata::{CondaMetadataParams, CondaMetadataResult},
        initialize::{InitializeParams, InitializeResult},
    },
    BackendCapabilities, CondaPackageMetadata, FrontendCapabilities,
};
use pixi_manifest::{Dependencies, Manifest, SpecType};
use pixi_spec::PixiSpec;
use rattler_build::{
    build::run_build,
    console_utils::LoggingOutputHandler,
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
};
use rattler_package_streaming::write::CompressionLevel;
use reqwest::Url;
use tempfile::tempdir;

use crate::build_script::{BuildPlatform, BuildScriptContext, Installer};

pub struct PythonBuildBackend {
    logging_output_handler: LoggingOutputHandler,
    manifest: Manifest,
}

impl PythonBuildBackend {
    /// Returns a new instance of [`PythonBuildBackendFactory`].
    ///
    /// This type implements [`ProtocolFactory`] and can be used to initialize a
    /// new [`PythonBuildBackend`].
    pub fn factory(logging_output_handler: LoggingOutputHandler) -> PythonBuildBackendFactory {
        PythonBuildBackendFactory {
            logging_output_handler,
        }
    }

    /// Returns a new instance of [`PythonBuildBackend`] by reading the manifest
    /// at the given path.
    pub fn new(
        manifest_path: &Path,
        logging_output_handler: LoggingOutputHandler,
    ) -> miette::Result<Self> {
        // Load the manifest from the source directory
        let manifest = Manifest::from_path(manifest_path).with_context(|| {
            format!("failed to parse manifest from {}", manifest_path.display())
        })?;

        Ok(Self {
            manifest,
            logging_output_handler,
        })
    }

    /// Returns the capabilities of this backend based on the capabilities of
    /// the frontend.
    pub fn capabilites(
        &self,
        _frontend_capabilities: &FrontendCapabilities,
    ) -> BackendCapabilities {
        BackendCapabilities {
            provides_conda_metadata: Some(true),
            provides_conda_build: Some(true),
        }
    }

    /// Returns the root directory of the pixi project.
    pub fn manifest_root(&self) -> &Path {
        self.manifest
            .path
            .parent()
            .expect("manifest should always reside in a directory")
    }

    /// Returns the channels from the manifest.
    fn channels(&self, channel_config: &ChannelConfig) -> Vec<Url> {
        self.manifest
            .parsed
            .project
            .channels
            .iter()
            .map(|c| c.channel.clone().into_base_url(channel_config))
            .collect()
    }

    /// Returns the requirements of the project that should be used for a
    /// recipe.
    fn requirements(&self, channel_config: &ChannelConfig) -> (Requirements, Installer) {
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

        let mut requirements = Requirements::default();
        let default_features = [self.manifest.default_feature()];

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

        // Determine the installer to use
        let installer = if host_dependencies.contains_key("uv")
            || run_dependencies.contains_key("uv")
            || build_dependencies.contains_key("uv")
        {
            Installer::Uv
        } else {
            Installer::Pip
        };

        // Ensure python and pip are available in the host dependencies section.
        for pkg_name in [installer.package_name(), "python"] {
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

        (requirements, installer)
    }

    /// Constructs a [`Recipe`] from the current manifest.
    fn recipe(&self, channel_config: &ChannelConfig) -> miette::Result<Recipe> {
        let manifest_root = self
            .manifest
            .path
            .parent()
            .expect("the project manifest must reside in a directory");

        // Parse the package name from the manifest
        let Some(name) = self.manifest.parsed.project.name.clone() else {
            miette::bail!("a 'name' field is required in the project manifest");
        };
        let name = PackageName::from_str(&name).into_diagnostic()?;

        // Parse the package version from the manifest. The version is optional, so we
        // default to "0dev0" if it is not present.
        let version = self
            .manifest
            .parsed
            .project
            .version
            .clone()
            .unwrap_or_else(|| Version::from_str("0dev0").unwrap());

        // TODO: NoArchType???
        let noarch_type = NoArchType::python();

        // TODO: Read from config / project.
        let (requirements, installer) = self.requirements(channel_config);
        let build_platform = Platform::current();
        let build_number = 0;

        let build_script = BuildScriptContext {
            installer,
            build_platform: if build_platform.is_windows() {
                BuildPlatform::Windows
            } else {
                BuildPlatform::Unix
            },
        }
        .render();

        Ok(Recipe {
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
                string: Default::default(),

                // skip: Default::default(),
                script: ScriptContent::Commands(build_script).into(),
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

    /// Returns the build configuration for a recipe
    pub async fn build_configuration(
        &self,
        recipe: &Recipe,
        channels: Vec<Url>,
    ) -> miette::Result<BuildConfiguration> {
        // Parse the package name from the manifest
        let Some(name) = self.manifest.parsed.project.name.clone() else {
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
            self.manifest.path.as_path(),
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
            store_recipe: false,
            force_colors: true,
        })
    }
}

/// Determines the build input globs for given python package
/// even this will be probably backend specific, e.g setuptools
/// has a different way of determining the input globs than hatch etc.
///
/// However, lets take everything in the directory as input for now
fn input_globs() -> Vec<String> {
    vec![
        // Source files
        "**/*.py",
        "**/*.pyx",
        "**/*.c",
        "**/*.cpp",
        "**/*.sh",
        // Common data files
        "**/*.json",
        "**/*.yaml",
        "**/*.yml",
        "**/*.txt",
        // Project configuration
        "setup.py",
        "setup.cfg",
        "pyproject.toml",
        "requirements*.txt",
        "Pipfile",
        "Pipfile.lock",
        "poetry.lock",
        "tox.ini",
        // Build configuration
        "Makefile",
        "MANIFEST.in",
        "tests/**/*.py",
        "docs/**/*.rst",
        "docs/**/*.md",
        // Versioning
        "VERSION",
        "version.py",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[async_trait::async_trait]
impl Protocol for PythonBuildBackend {
    async fn get_conda_metadata(
        &self,
        params: CondaMetadataParams,
    ) -> miette::Result<CondaMetadataResult> {
        let channel_config = ChannelConfig {
            channel_alias: params.channel_configuration.base_url,
            root_dir: self.manifest_root().to_path_buf(),
        };
        let channels = params
            .channel_base_urls
            .unwrap_or_else(|| self.channels(&channel_config));

        // TODO: Determine how and if we can determine this from the manifest.
        let recipe = self.recipe(&channel_config)?;
        let output = Output {
            build_configuration: self.build_configuration(&recipe, channels).await?,
            recipe,
            finalized_dependencies: None,
            finalized_cache_dependencies: None,
            finalized_sources: None,
            build_summary: Arc::default(),
            system_tools: Default::default(),
            extra_meta: None,
        };
        let tool_config = Configuration::builder()
            .with_logging_output_handler(self.logging_output_handler.clone())
            .with_channel_config(channel_config.clone())
            .finish();

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

    async fn build_conda(&self, params: CondaBuildParams) -> miette::Result<CondaBuildResult> {
        let channel_config = ChannelConfig {
            channel_alias: params.channel_configuration.base_url,
            root_dir: self.manifest_root().to_path_buf(),
        };
        let channels = params
            .channel_base_urls
            .unwrap_or_else(|| self.channels(&channel_config));

        let recipe = self.recipe(&channel_config)?;
        let output = Output {
            build_configuration: self.build_configuration(&recipe, channels).await?,
            recipe,
            finalized_dependencies: None,
            finalized_cache_dependencies: None,
            finalized_sources: None,
            build_summary: Arc::default(),
            system_tools: Default::default(),
            extra_meta: None,
        };
        let tool_config = Configuration::builder()
            .with_logging_output_handler(self.logging_output_handler.clone())
            .with_channel_config(channel_config.clone())
            .finish();

        let temp_recipe = TemporaryRenderedRecipe::from_output(&output)?;
        let (_output, package) = temp_recipe
            .within_context_async(move || async move { run_build(output, &tool_config).await })
            .await?;

        Ok(CondaBuildResult {
            output_file: package,
            input_globs: input_globs(),
        })
    }
}

pub struct PythonBuildBackendFactory {
    logging_output_handler: LoggingOutputHandler,
}

#[async_trait::async_trait]
impl ProtocolFactory for PythonBuildBackendFactory {
    type Protocol = PythonBuildBackend;

    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> miette::Result<(Self::Protocol, InitializeResult)> {
        let instance = PythonBuildBackend::new(
            params.manifest_path.as_path(),
            self.logging_output_handler.clone(),
        )?;

        let capabilities = instance.capabilites(&params.capabilities);
        Ok((instance, InitializeResult { capabilities }))
    }
}

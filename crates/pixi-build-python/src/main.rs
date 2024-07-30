mod consts;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use clap::Parser;
use pixi_manifest::TargetSelector::Platform;
use pixi_manifest::{FeatureName, Manifest, SpecType};
use rattler_build::recipe::parser::{Build, Requirements};
use rattler_build::{
    hash::HashInfo,
    metadata::{BuildConfiguration, Directories, Output, PackagingSettings},
    recipe::{
        parser::{Package, PathSource, Source},
        Recipe,
    },
};
use rattler_conda_types::{package::ArchiveType, MatchSpec, NoArchType};
use rattler_package_streaming::write::CompressionLevel;

#[allow(missing_docs)]
#[derive(Parser)]
#[clap(version)]
pub struct App {
    /// The path to the manifest file
    #[clap(env, long, env = "PIXI_PROJECT_MANIFEST", default_value = consts::PROJECT_MANIFEST)]
    manifest_path: PathBuf,
}

/// Get the requirements for a default feature
fn requirements_from_manifest(manifest: &Manifest) -> Requirements {
    let mut requirements = Requirements::default();
    let default_features = manifest
        .default_environment()
        .features
        .iter()
        .filter_map(|e| manifest.feature(&FeatureName::Named(e.clone())))
        .collect::<Vec<_>>();

    // Get all different feature types
    let run_dependencies = default_features
        .iter()
        .filter_map(|f| f.dependencies(Some(SpecType::Run), None))
        .collect::<Vec<_>>();
    let host_dependencies = default_features
        .iter()
        .filter_map(|f| f.dependencies(Some(SpecType::Run), None))
        .collect::<Vec<_>>();
    let build_dependencies = default_features
        .iter()
        .filter_map(|f| f.dependencies(Some(SpecType::Run), None))
        .collect::<Vec<_>>();

    requirements
}

fn main() {
    let args = App::parse();

    let manifest = Manifest::from_path(args.manifest_path).expect("could not load manifest");
    requirements_from_manifest(&manifest);

    // Load the manifest
    eprintln!("Looking for manifest at {:?}", args.manifest_path);

    // TODO: Variants???
    let variants = BTreeMap::default();

    // TODO: NoArchType???
    let noarch_type = NoArchType::None;

    // TODO: Setup defaults
    let directories = Directories::default();

    let output = Output {
        recipe: Recipe {
            schema_version: 1,
            package: Package {
                // TODO:
            },
            cache: None,
            source: vec![Source::Path(PathSource {
                path: args
                    .manifest_path
                    .parent()
                    .expect("the project manifest must reside in a directory")
                    .to_path_buf(),
                sha256: None,
                md5: None,
                patches: vec![],
                target_directory: None,
                file_name: None,
                use_gitignore: true,
            })],
            build: Build {
                // TODO:
            },
            // TODO read from manifest
            requirements: Requirements {},
            tests: vec![],
            about: Default::default(),
            extra: Default::default(),
        },
        build_configuration: BuildConfiguration {
            // TODO: NoArch??
            target_platform: Platform::NoArch,
            host_platform: Platform::current(),
            build_platform: Platform::current(),
            hash: HashInfo::from_variant(&variants, &noarch_type),
            variant,
            directories,
            channels: vec![], // TODO: read from manifest
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
}

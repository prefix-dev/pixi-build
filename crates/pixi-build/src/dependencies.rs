use itertools::Either;
use miette::IntoDiagnostic;
use pixi_manifest::CondaDependencies;
use pixi_spec::SourceSpec;
use rattler_conda_types::{ChannelConfig, MatchSpec};

/// A helper struct to extract match specs from a manifest.
pub struct MatchspecExtractor {
    channel_config: ChannelConfig,
    ignore_self: bool,
}

impl MatchspecExtractor {
    pub fn new(channel_config: ChannelConfig) -> Self {
        Self {
            channel_config,
            ignore_self: false,
        }
    }

    /// If `ignore_self` is `true`, the conversion will skip dependencies that
    /// point to root directory itself.
    pub fn with_ignore_self(self, ignore_self: bool) -> Self {
        Self {
            ignore_self,
            ..self
        }
    }

    /// Extracts match specs from the given set of dependencies.
    pub fn extract(&self, dependencies: CondaDependencies) -> miette::Result<Vec<MatchSpec>> {
        let root_dir = &self.channel_config.root_dir;
        let mut specs = Vec::new();
        for (name, spec) in dependencies.into_specs() {
            let source_or_binary = spec
                .into_source_or_binary(&self.channel_config)
                .into_diagnostic()?;
            let match_spec = match source_or_binary {
                Either::Left(SourceSpec::Path(path))
                    if self.ignore_self
                        && path
                            .resolve(root_dir)
                            .map_or(false, |path| path.as_path() == root_dir) =>
                {
                    // Skip source dependencies that point to the root directory. That would
                    // be a self reference.
                    continue;
                }
                Either::Left(_) => {
                    // All other source dependencies are not yet supported.
                    return Err(miette::miette!(
                        "recursive source dependencies are not yet supported"
                    ));
                }
                Either::Right(binary) => MatchSpec::from_nameless(binary, Some(name)),
            };

            specs.push(match_spec);
        }

        Ok(specs)
    }
}

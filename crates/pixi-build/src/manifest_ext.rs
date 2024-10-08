use std::{path::Path, str::FromStr, sync::OnceLock};

use pixi_manifest::Manifest;
use rattler_conda_types::{ChannelConfig, ParseChannelError, Platform, Version};
use reqwest::Url;

pub trait ManifestExt {
    fn manifest(&self) -> &Manifest;

    /// Returns the path to the root directory that contains the manifest.
    fn manifest_root(&self) -> &Path {
        self.manifest()
            .path
            .parent()
            .expect("manifest path should have a parent")
    }

    /// Returns the resolved channels that are specified in the manifest
    /// `project` section.
    ///
    /// This function might return an error if the channel URL is invalid.
    fn resolved_project_channels(
        &self,
        channel_config: &ChannelConfig,
    ) -> Result<Vec<Url>, ParseChannelError> {
        self.manifest()
            .parsed
            .project
            .channels
            .iter()
            .map(|c| c.channel.clone().into_base_url(channel_config))
            .collect()
    }

    /// Returns `true` if the manifest is configured to use the specified
    /// platform.
    fn supports_target_platform(&self, platform: Platform) -> bool {
        self.manifest()
            .parsed
            .project
            .platforms
            .value
            .contains(&platform)
    }

    /// Returns the version as specified in the manifest.
    ///
    /// Note that this may be `None` because having a version is not required.
    /// Use [`Self::version_or_default`] to get a default version in that case.
    fn version(&self) -> Option<&Version> {
        self.manifest().parsed.project.version.as_ref()
    }

    /// Returns the version of the project or a default version if no version is
    /// specified in the manifest.
    fn version_or_default(&self) -> &Version {
        static DEFAULT_VERSION: OnceLock<Version> = OnceLock::new();
        self.version()
            .unwrap_or_else(|| DEFAULT_VERSION.get_or_init(|| Version::from_str("0.1.0").unwrap()))
    }
}

impl ManifestExt for Manifest {
    fn manifest(&self) -> &Manifest {
        self
    }
}

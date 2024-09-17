use pixi_build_types::procedures::{
    conda_build::{CondaBuildParams, CondaBuildResult},
    conda_metadata::{CondaMetadataParams, CondaMetadataResult},
    initialize::{InitializeParams, InitializeResult},
};

/// A trait that is used to initialize a new protocol connection.
#[async_trait::async_trait]
pub trait ProtocolFactory: Send + Sync + 'static {
    type Protocol: Protocol + Send + Sync + 'static;

    /// Called when the client requests initialization.
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> miette::Result<(Self::Protocol, InitializeResult)>;
}

/// A trait that defines the protocol for a pixi build backend.
#[async_trait::async_trait]
pub trait Protocol {
    /// Called when the client requests metadata for a Conda package.
    async fn get_conda_metadata(
        &self,
        _params: CondaMetadataParams,
    ) -> miette::Result<CondaMetadataResult> {
        unimplemented!("get_conda_metadata not implemented");
    }

    /// Called when the client requests to build a Conda package.
    async fn build_conda(&self, _params: CondaBuildParams) -> miette::Result<CondaBuildResult> {
        unimplemented!("build_conda not implemented");
    }
}

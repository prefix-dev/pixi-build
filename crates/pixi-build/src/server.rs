use std::{net::SocketAddr, sync::Arc};

use jsonrpc_core::{serde_json, to_value, Error, IoHandler, Params};
use miette::{IntoDiagnostic, JSONReportHandler};
use pixi_build_types::{
    procedures,
    procedures::{
        conda_build::CondaBuildParams, conda_metadata::CondaMetadataParams,
        initialize::InitializeParams,
    },
};
use tokio::sync::RwLock;

use crate::protocol::{Protocol, ProtocolFactory};

/// A JSONRPC server that can be used to communicate with a client.
pub struct Server<T: ProtocolFactory> {
    factory: T,
}

enum ServerState<T: ProtocolFactory> {
    Uninitialized(T),
    Initialized(T::Protocol),
}

impl<T: ProtocolFactory> ServerState<T> {
    pub fn as_protocol(&self) -> Result<&T::Protocol, jsonrpc_core::Error> {
        match self {
            Self::Initialized(protocol) => Ok(protocol),
            _ => Err(Error::invalid_request()),
        }
    }
}

impl<T: ProtocolFactory> Server<T> {
    pub fn new(factory: T) -> Self {
        Self { factory }
    }

    pub async fn run(self) -> miette::Result<()> {
        let io = self.setup_io();
        jsonrpc_stdio_server::ServerBuilder::new(io).build().await;
        Ok(())
    }

    pub fn run_over_http(self, port: u16) -> miette::Result<()> {
        let io = self.setup_io();
        jsonrpc_http_server::ServerBuilder::new(io)
            .start_http(&SocketAddr::from(([127, 0, 0, 1], port)))
            .into_diagnostic()?
            .wait();
        Ok(())
    }

    fn setup_io(self) -> IoHandler {
        // Construct a server
        let mut io = IoHandler::new();
        let state = Arc::new(RwLock::new(ServerState::Uninitialized(self.factory)));

        let initialize_state = state.clone();
        io.add_method(
            procedures::initialize::METHOD_NAME,
            move |params: Params| {
                let state = initialize_state.clone();

                async move {
                    let params: InitializeParams = params.parse()?;
                    let mut state = state.write().await;
                    let ServerState::Uninitialized(factory) = &mut *state else {
                        return Err(Error::invalid_request());
                    };

                    let (protocol, result) =
                        factory.initialize(params).await.map_err(convert_error)?;
                    *state = ServerState::Initialized(protocol);

                    Ok(to_value(result).expect("failed to convert to json"))
                }
            },
        );

        let conda_get_metadata = state.clone();
        io.add_method(
            procedures::conda_metadata::METHOD_NAME,
            move |params: Params| {
                let state = conda_get_metadata.clone();

                async move {
                    let params: CondaMetadataParams = params.parse()?;
                    let state = state.read().await;
                    state
                        .as_protocol()?
                        .get_conda_metadata(params)
                        .await
                        .map(|value| to_value(value).expect("failed to convert to json"))
                        .map_err(convert_error)
                }
            },
        );

        let conda_build = state.clone();
        io.add_method(
            procedures::conda_build::METHOD_NAME,
            move |params: Params| {
                let state = conda_build.clone();

                async move {
                    let params: CondaBuildParams = params.parse()?;
                    let state = state.read().await;
                    state
                        .as_protocol()?
                        .build_conda(params)
                        .await
                        .map(|value| to_value(value).expect("failed to convert to json"))
                        .map_err(convert_error)
                }
            },
        );

        io
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

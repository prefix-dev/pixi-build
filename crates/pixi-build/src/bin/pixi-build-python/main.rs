mod build_script;
mod python;

use python::PythonBuildBackend;

#[tokio::main]
pub async fn main() {
    if let Err(err) = pixi_build_backend::cli::main(PythonBuildBackend::factory).await {
        eprintln!("{err:?}");
        std::process::exit(1);
    }
}

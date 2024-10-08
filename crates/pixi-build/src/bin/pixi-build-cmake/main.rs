mod build_script;
mod cmake;
mod stub;

use cmake::CMakeBuildBackend;

#[tokio::main]
pub async fn main() {
    if let Err(err) = pixi_build_backend::cli::main(CMakeBuildBackend::factory).await {
        eprintln!("{err:?}");
        std::process::exit(1);
    }
}

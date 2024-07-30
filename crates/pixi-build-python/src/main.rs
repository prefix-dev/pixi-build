mod consts;

use std::path::PathBuf;

use clap::Parser;

#[allow(missing_docs)]
#[derive(Parser)]
#[clap(version)]
pub struct App {
    /// The path to the manifest file
    #[clap(env, long, env = "PIXI_PROJECT_MANIFEST", default_value = consts::PROJECT_MANIFEST)]
    manifest_path: PathBuf,
}

fn main() {
    let args = App::parse();

    // Load the manifest
    eprintln!("Looking for manifest at {:?}", args.manifest_path);
}

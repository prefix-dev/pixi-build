//! This module contains some functions that I copied from rattler-build. We
//! should refactor these.

use rattler_conda_types::Platform;

pub(crate) fn default_compiler(platform: Platform, language: &str) -> Option<String> {
    Some(
        match language {
            // Platform agnostic compilers
            "fortran" => "gfortran",
            lang if !["c", "cxx"].contains(&lang) => lang,
            // Platform specific compilers
            _ => {
                if platform.is_windows() {
                    match language {
                        "c" => "vs2017",
                        "cxx" => "vs2017",
                        _ => unreachable!(),
                    }
                } else if platform.is_osx() {
                    match language {
                        "c" => "clang",
                        "cxx" => "clangxx",
                        _ => unreachable!(),
                    }
                } else if matches!(platform, Platform::EmscriptenWasm32) {
                    match language {
                        "c" => "emscripten",
                        "cxx" => "emscripten",
                        _ => unreachable!(),
                    }
                } else {
                    match language {
                        "c" => "gcc",
                        "cxx" => "gxx",
                        _ => unreachable!(),
                    }
                }
            }
        }
        .to_string(),
    )
}

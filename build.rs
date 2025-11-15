use std::{env, fs};

fn main() {
    cdylib_plugin::buildflags();

    if env::consts::OS == "macos" {
        let dylib_path = cdylib_plugin::cdylib_path();
        let dylib_dir = dylib_path.parent().unwrap();

        // Set LD_LIBRARY_PATH for vtc tests
        println!(
            "cargo:rustc-env=LD_LIBRARY_PATH=/opt/homebrew/lib:{}",
            dylib_dir.display()
        );

        // Add varnish sbin to PATH
        if let Ok(path) = env::var("PATH") {
            println!(
                "cargo:rustc-env=PATH=/opt/homebrew/opt/varnish/sbin:{}",
                path
            );
        }

        // After the build, copy .dylib to .so if it exists
        if dylib_path.exists() {
            let so_path = dylib_path.with_extension("so");
            fs::copy(&dylib_path, &so_path).unwrap();
        }
    }
}

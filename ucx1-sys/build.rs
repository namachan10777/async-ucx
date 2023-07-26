use std::env;
use std::path::PathBuf;

fn main() {
    let ucx = pkg_config::Config::new().probe("ucx").unwrap();

    for lib_dir in &ucx.link_paths {
        // Tell cargo to tell rustc to link the library.
        println!("cargo:rustc-link-search=native={}", lib_dir.to_str().unwrap());
    }
    println!("cargo:rustc-link-lib=ucp");
    println!("cargo:rustc-link-lib=uct");
    println!("cargo:rustc-link-lib=ucs");
    println!("cargo:rustc-link-lib=ucm");

    // Tell cargo to invalidate the built crate whenever the wrapper changes
    println!("cargo:rerun-if-changed=wrapper.h");

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    let bindings = bindgen::Builder::default()
        .clang_args(ucx.include_paths.iter().map(|inc_dir| format!("-I{}", inc_dir.to_string_lossy())))
        // The input header we would like to generate bindings for.
        .header("wrapper.h")
        // Tell cargo to invalidate the built crate whenever any of the
        // included header files changed.
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        // .parse_callbacks(Box::new(ignored_macros))
        .allowlist_function("uc[tsmp]_.*")
        .allowlist_var("uc[tsmp]_.*")
        .allowlist_var("UC[TSMP]_.*")
        .allowlist_type("uc[tsmp]_.*")
        .rustified_enum(".*")
        .bitfield_enum("ucp_feature")
        .bitfield_enum(".*_field")
        .bitfield_enum(".*_flags(_t)?")
        // Finish the builder and generate the bindings.
        .generate()
        // Unwrap the Result and panic on failure.
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

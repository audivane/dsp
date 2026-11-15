use std::{env, process::Command};
fn main() {
    let out = env::var("OUT_DIR").unwrap();
    assert!(
        Command::new("cc")
            .args([
                "-O3",
                "-c",
                "reference.c",
                "-o",
                &format!("{out}/reference.o")
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("ar")
            .args([
                "crs",
                &format!("{out}/libreference.a"),
                &format!("{out}/reference.o")
            ])
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=reference");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rerun-if-changed=reference.c");
}

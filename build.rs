use std::path::Path;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(msql_srv_warning_counts)");

    // The in-tree protocol dependency carries warning-count support that has
    // not yet been released by msql-srv. `cargo package` replaces this path
    // dependency with the published 0.11 crate, so keep that build compatible
    // until the extension is available upstream.
    if Path::new("vendor/msql-srv/src/resultset.rs").is_file() {
        println!("cargo:rustc-cfg=msql_srv_warning_counts");
    }
}

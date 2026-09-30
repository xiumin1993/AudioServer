// build.rs - make cargo notice that the locale files are part of the build.
//
// WHY THIS EXISTS
//   src/lib.rs embeds the translations at compile time with `i18n!("locales", ...)`.
//   Cargo has no idea those .toml files are inputs: it only tracks .rs files and
//   build scripts. So after adding a key to locales/zh.toml, `cargo build` happily
//   reuses the cached lib object and the program prints the raw key
//   ("settings.log_path") instead of the text - which is exactly what happened on
//   2026-10-01: the release exe was rebuilt, main.rs had changed, the lib had not,
//   and the settings page shipped a placeholder to a screenshot.
//
//   Listing every file under locales/ with `rerun-if-changed` fixes it. Per-file
//   (not the directory) matters: editing an existing .toml does not change the
//   directory's mtime, so a directory-level watch would miss precisely the common case.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    match std::fs::read_dir("locales") {
        Ok(entries) => {
            for entry in entries.flatten() {
                println!("cargo:rerun-if-changed={}", entry.path().display());
            }
        }
        // No locales dir (e.g. building the crate from a package that stripped it):
        // say nothing rather than fail - rust-i18n's own macro will report the problem.
        Err(_) => {}
    }
}

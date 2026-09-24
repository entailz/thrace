fn main() {
    if std::env::var_os("CARGO_FEATURE_SLINT_UI").is_some() {
        slint_build::compile_with_config(
            "ui/app.slint",
            slint_build::CompilerConfiguration::new().with_style("fluent-dark".into()),
        )
        .expect("compile Slint UI");
    }
}

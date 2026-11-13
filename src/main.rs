#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = everything_rs::cli::run(true) {
        eprintln!("Error: {error:#}");
        #[cfg(all(windows, feature = "gui"))]
        rfd::MessageDialog::new()
            .set_title("Everything Rust")
            .set_description(format!("{error:#}"))
            .set_level(rfd::MessageLevel::Error)
            .show();
        std::process::exit(1);
    }
}

//! Console companion; the Windows desktop executable intentionally has no console window.
fn main() {
    if let Err(error) = everything_rs::cli::run(false) {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

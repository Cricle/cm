fn main() {
    if let Err(error) = cm_memory::run() {
        eprintln!("cm: {error:#}");
        std::process::exit(1);
    }
}

fn main() {
    let exit_code = poros::runner::run(std::env::args().skip(1).collect());
    std::process::exit(exit_code);
}

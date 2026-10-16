#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = if args.first().map(String::as_str) == Some("run") {
        opctl::glue::run(&args[1..]).await
    } else {
        opctl::cli::run(&args, &mut std::io::stdout(), &mut std::io::stderr())
    };
    std::process::exit(code);
}

use clap::Parser;

fn main() {
    let cli = fbrain::cli::Cli::parse();
    if let Err(e) = fbrain::cli::run(cli) {
        eprintln!("{}", fbrain::review::escape_text(&format!("{e:#}")));
        let code = if e.is::<fbrain::cli::ModelNotDownloaded>()
            || e.is::<fbrain::cli::Refused>()
            || e.is::<fbrain::cli::PrivateRefused>()
        {
            3
        } else if e.is::<fbrain::proposals::Invalid>() || e.is::<fbrain::review::Protected>() {
            2
        } else {
            1
        };
        std::process::exit(code);
    }
}

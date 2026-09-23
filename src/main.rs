fn main() {
    // Logs go to stderr (the daemon's stderr is its log file). Default level
    // is `warn` for interactive use; set HERDR_ORCH_LOG=info|debug for more.
    let filter = tracing_subscriber::EnvFilter::try_from_env("HERDR_ORCH_LOG").unwrap_or_else(|_| {
        let daemon = std::env::args().nth(1).as_deref() == Some("daemon");
        tracing_subscriber::EnvFilter::new(if daemon { "info" } else { "warn" })
    });
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_target(false).init();
    match herdr_orchestrator::cli::main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

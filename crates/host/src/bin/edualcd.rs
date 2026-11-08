//! The daemon, `edualcd`, which the first window starts beside itself (or as the image
//! decoder, confined: see `decode`).

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "edualc=info,edualc_host=info,edualc_core=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Some(job) = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse::<edualc_host::decode::Job>().ok())
    {
        return edualc_host::decode::run(job);
    }
    edualc_host::daemon::run()
}

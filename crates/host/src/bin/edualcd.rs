//! The daemon, `edualcd`, which the first window starts beside itself (or as the image
//! decoder, confined: see `decode`; or as an agy session's adapter or its messaging tool: see
//! `agy`).

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "edualc=info,edualc_host=info,edualc_core=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    // An agy session's adapter, which the daemon starts in place of claude, and the messaging
    // tool it gives agy (see `agy`).
    match std::env::args().nth(1).as_deref() {
        Some(edualc_host::agy::ADAPTER_FLAG) => return edualc_host::agy::run_adapter(),
        Some(edualc_host::agy::TOOL_FLAG) => return edualc_host::agy::run_tool(),
        Some(_) | None => {}
    }
    if let Some(job) = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse::<edualc_host::decode::Job>().ok())
    {
        return edualc_host::decode::run(job);
    }
    edualc_host::daemon::run()
}

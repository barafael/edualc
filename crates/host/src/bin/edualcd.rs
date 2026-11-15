//! The daemon, `edualcd`, which the first window starts beside itself (or as the image
//! decoder, confined: see `decode`; or as an ACP session's adapter or its messaging tool: see
//! `acp`).

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "edualc=info,edualc_host=info,edualc_core=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    // An ACP session's adapter, which the daemon starts in place of claude, and the messaging
    // tool it gives the agent (see `acp`).
    match std::env::args().nth(1).as_deref() {
        Some(edualc_host::ADAPTER_FLAG) => return edualc_host::run_adapter(),
        Some(edualc_host::TOOL_FLAG) => return edualc_host::run_tool(),
        Some(_) | None => {}
    }
    if let Some(job) = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse::<edualc_core::decode::Job>().ok())
    {
        return edualc_core::decode::run(job);
    }
    edualc_host::run()
}

//! How the window starts, as the Slint window does (`crates/slint/src/start.rs`): its command
//! line read (see `command_line`), its D-Bus proxies started (see `buses`), its edualcd reached,
//! where it works found on edualcd's machine, and the window confined before GPUI makes a
//! thread or a window; then GPUI's application, the window, and the app run on GPUI's own
//! executor.

use gpui::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};

use crate::command_line::{Args, pair, paired, working_folder};
use crate::line;
use crate::{app, buses, confine, decode, desktop, ui};

pub(crate) fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "edualc=info,edualc_gpui=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // The image decoder the window starts, confined before it reads (see `decode`).
    if let Some(job) = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse::<decode::Job>().ok())
    {
        return decode::run(job);
    }

    // Help and the version are printed here, and wrong arguments said, before anything starts.
    let Args {
        model,
        mode,
        resume,
        server,
        pair: pairing,
        pair_as,
        dir,
        prompt,
    } = clap::Parser::parse();
    if let Some(address) = pairing {
        return pair(&address, pair_as);
    }
    // The first process starts the D-Bus proxies and the window as its child, and waits for it.
    if std::env::var_os(buses::PROXIED).is_none() {
        return buses::run_window().unwrap_or_else(|error| {
            eprintln!("edualc: couldn't start the window: {error}");
            std::process::ExitCode::FAILURE
        });
    }
    if !buses::end_with_first_process() {
        return std::process::ExitCode::FAILURE;
    }
    let chosen = paired().and_then(|(servers, _path)| servers.chosen(server.as_deref()));
    let (name, pairing) = match chosen {
        Ok(chosen) => chosen,
        Err(error) => {
            eprintln!("edualc: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // In this order: edualcd is reached and the desktop asked while the window is free; then it
    // is confined, and all it makes after keeps that.
    let mut connected = match line::connect(&pairing, line::Lookup::Once) {
        Ok(connected) => connected,
        Err(error) => {
            eprintln!("edualc: {name}: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let cwd = match working_folder(&mut connected, dir, resume.as_ref()) {
        Ok(folder) => folder,
        Err(error) => {
            eprintln!("edualc: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    // Asked before the window is confined, of programs it may not run then.
    let (fonts, text_size) = std::thread::scope(|scope| {
        let fonts = scope.spawn(crate::appearance::fonts);
        let text_size = crate::appearance::desktop_text_size();
        let fonts = fonts
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (fonts, text_size)
    });
    let desktop = app::Desktop::of(&connected.greeting, text_size);
    match confine::confine(&connected.addresses) {
        Ok(confinement) => tracing::info!(?confinement, "the window is confined"),
        Err(error) => {
            eprintln!("edualc: {error}");
            return std::process::ExitCode::FAILURE;
        }
    }
    let (events, received) = async_channel::unbounded();
    let (link, greeting) = line::run(connected, events.clone());

    // GPUI's threads and its Wayland connection are made here, confined as the rest.
    gpui_platform::application()
        .with_assets(ui::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            let bounds = Bounds::centered(None, size(px(1280.0), px(860.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // The compositor is told the id once, as the window is made.
                app_id: Some(desktop::APP_ID.to_owned()),
                window_min_size: Some(size(px(560.0), px(400.0))),
                ..WindowOptions::default()
            };
            let opened = cx.open_window(options, |window, cx| {
                let shell = cx.new(|cx| ui::Shell::new(events.clone(), fonts, window, cx));
                cx.new(|cx| gpui_component::Root::new(shell, window, cx))
            });
            let handle = match opened {
                Ok(handle) => handle,
                Err(error) => {
                    eprintln!("edualc: couldn't open the window: {error}");
                    cx.quit();
                    return;
                }
            };
            let Some(frontend) = ui::GpuiFrontend::new(handle.into(), cx) else {
                eprintln!("edualc: the window has no view");
                cx.quit();
                return;
            };
            // Made and begun on GPUI's executor, not here: GPUI holds its app while this
            // callback runs, and the frontend reaches the window through it.
            cx.spawn(async move |_| {
                let mut app = app::App::new(
                    frontend,
                    events,
                    link,
                    desktop,
                    greeting.settings,
                    cwd,
                    model,
                );
                app.begin(
                    greeting.hosted,
                    greeting.live,
                    resume,
                    mode,
                    prompt.as_deref(),
                );
                app.run(received).await;
            })
            .detach();
        });
    std::process::ExitCode::SUCCESS
}

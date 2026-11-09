//! How the window starts: its command line read (see `command_line`), its D-Bus proxies started
//! (see `buses`), its edualcd reached, where it works found on edualcd's machine, and the window
//! confined before it opens.

use slint::ComponentHandle;

use crate::command_line::{Args, pair, paired, working_folder};
use crate::line;
use crate::{app, buses, confine, decode, desktop, generated, ui};

pub(crate) fn main() -> Result<std::process::ExitCode, slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "edualc=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // The image decoder the window starts, confined before it reads (see `decode`).
    if let Some(job) = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse::<decode::Job>().ok())
    {
        return Ok(decode::run(job));
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
        return Ok(pair(&address, pair_as));
    }
    // The first process starts the D-Bus proxies and the window as its child, and waits for it.
    if std::env::var_os(buses::PROXIED).is_none() {
        return Ok(buses::run_window().unwrap_or_else(|error| {
            eprintln!("edualc: couldn't start the window: {error}");
            std::process::ExitCode::FAILURE
        }));
    }
    if !buses::end_with_first_process() {
        return Ok(std::process::ExitCode::FAILURE);
    }
    let chosen = paired().and_then(|(servers, _path)| servers.chosen(server.as_deref()));
    let (name, pairing) = match chosen {
        Ok(chosen) => chosen,
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    // In this order: edualcd is reached and the desktop asked while the window is free; then it
    // is confined, and all it makes after keeps that.
    let mut connected = match line::connect(&pairing, line::Lookup::Once) {
        Ok(connected) => connected,
        Err(error) => {
            eprintln!("edualc: {name}: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let cwd = match working_folder(&mut connected, dir, resume.as_ref()) {
        Ok(folder) => folder,
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::from(2));
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
            return Ok(std::process::ExitCode::FAILURE);
        }
    }
    let (events, received) = async_channel::unbounded();
    let (link, greeting) = line::run(connected, events.clone());
    let window = generated::AppWindow::new()?;
    // Before `show`: the compositor is told the id once, as the window is made.
    slint::set_xdg_app_id(desktop::APP_ID)?;
    ui::wire(&window, &events);
    let frontend = ui::SlintFrontend::new(window.clone_strong(), fonts, events.clone());
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
    slint::spawn_local(app.run(received)).expect("the event loop is set up");
    window.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(std::process::ExitCode::SUCCESS)
}

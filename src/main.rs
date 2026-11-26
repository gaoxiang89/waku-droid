// A GUI build must not open a console window behind the app. Debug builds
// keep the console subsystem so `cargo run` and the dev watcher still see
// stdout and panics.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

fn main() {
    // ssh launches this executable as SSH_ASKPASS for password-authenticated
    // connections. Answer and exit before any UI or daemon starts.
    if let Some(code) = waku_client::remote::ssh::handle_askpass() {
        std::process::exit(code);
    }

    let action = match waku::cli::parse_args(std::env::args().skip(1), true) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("narel: {error}");
            std::process::exit(1);
        }
    };
    let request = match action {
        waku::cli::CliAction::Help => {
            println!("{}", waku::cli::help_text());
            return;
        }
        waku::cli::CliAction::Version => {
            println!("{}", waku::cli::version_text());
            return;
        }
        waku::cli::CliAction::Open(request) => request,
    };

    let cwd = std::env::current_dir().ok();
    let mut request = waku::cli::prepare_request(request, cwd.as_deref());

    match waku::cli::try_deliver_to_running_desktop(&request) {
        Ok(waku::cli::DeliveryResult::Delivered(_)) => return,
        Ok(waku::cli::DeliveryResult::NotRunning) => {}
        Err(error) => {
            eprintln!("narel: {error}");
            std::process::exit(1);
        }
    }

    // Validate local startup paths before opening the GPUI window so invalid
    // paths exit cleanly without blocking the UI thread on filesystem I/O.
    if request.host.is_none()
        && let Some(raw_path) = request.path.as_deref()
    {
        match waku::cli::validate_local_workspace_path(raw_path) {
            Ok(resolved) => request.path = Some(resolved),
            Err(error) => {
                eprintln!("narel: {error}");
                std::process::exit(1);
            }
        }
    }

    // Import before GPUI, window preferences, or saved connections are read.
    if let Err(error) = waku_protocol::identity::import_legacy_user_data() {
        eprintln!("narel: could not import Waku data; originals are unchanged: {error}");
        std::process::exit(1);
    }
    let startup_open = (request.path.is_some() || request.host.is_some()).then_some(request);
    waku::run(startup_open);
}

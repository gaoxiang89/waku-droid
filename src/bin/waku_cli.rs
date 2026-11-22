//! Console-subsystem CLI wrapper for Waku (`waku_cli`, staged as `bin/waku.exe`
//! in packaged Windows distributions).
//!
//! Keeps terminal stdout/stderr and exit codes intact while handing workspace
//! open requests to an already-running desktop or cold-launching the GUI
//! executable detached.

#[allow(dead_code)]
#[path = "../cli.rs"]
mod cli;

fn main() {
    let action = match cli::parse_args(std::env::args().skip(1), false) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("waku: {error}");
            std::process::exit(1);
        }
    };

    let request = match action {
        cli::CliAction::Help => {
            println!("{}", cli::help_text());
            return;
        }
        cli::CliAction::Version => {
            println!("{}", cli::version_text());
            return;
        }
        cli::CliAction::Open(request) => request,
    };

    let cwd = std::env::current_dir().ok();
    let mut request = cli::prepare_request(request, cwd.as_deref());

    match cli::try_deliver_to_running_desktop(&request) {
        Ok(cli::DeliveryResult::Delivered(_)) => return,
        Ok(cli::DeliveryResult::NotRunning) => {}
        Err(error) => {
            eprintln!("waku: {error}");
            std::process::exit(1);
        }
    }

    // No desktop is currently attached: validate any local path before spawning
    // the GUI so typos/missing paths report immediately on stderr and exit 1.
    if request.host.is_none()
        && let Some(raw_path) = request.path.as_deref()
    {
        match cli::validate_local_workspace_path(raw_path) {
            Ok(resolved) => request.path = Some(resolved),
            Err(error) => {
                eprintln!("waku: {error}");
                std::process::exit(1);
            }
        }
    }

    let gui_exe = match cli::resolve_gui_executable() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("waku: {error:#}");
            std::process::exit(1);
        }
    };

    let forwarded = cli::forwarded_gui_args(&request);
    if let Err(error) = cli::spawn_gui_detached(&gui_exe, &forwarded) {
        eprintln!("waku: {error:#}");
        std::process::exit(1);
    }
}

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
    if waku_client::remote::ssh::handle_askpass() {
        return;
    }
    waku::run();
}

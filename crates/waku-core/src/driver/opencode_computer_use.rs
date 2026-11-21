//! One runtime MCP connection per OpenCode location, with session-scoped
//! kernels and native helper ownership behind it. The adopted service is
//! never restarted and no OpenCode configuration file is changed.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};
use uuid::Uuid;

use super::computer_use::{ComputerUseConfig, ComputerUseRuntime, create_process_directory};
use crate::driver::DriverEventSender;
use crate::opencode_api;
use crate::opencode_service::OpenCodeService;

pub(super) const INSTRUCTION_KEY: &str = "waku-computer-use";

pub(super) fn tool_identity(name: &str) -> Option<(&str, &str)> {
    let (server, tool) = name
        .strip_suffix("_js_reset")
        .map(|server| (server, "js_reset"))
        .or_else(|| name.strip_suffix("_js").map(|server| (server, "js")))?;
    let id = server.strip_prefix("waku_js_repl_")?;
    (id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some((server, tool))
}

type BridgeKey = (usize, String);
type Bridges = Mutex<HashMap<BridgeKey, Weak<McpBridge>>>;
static BRIDGES: OnceLock<Bridges> = OnceLock::new();
const MCP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

struct McpBridge {
    service: Arc<OpenCodeService>,
    directory: String,
    server: String,
    sessions_directory: PathBuf,
    config: Value,
    registration: Mutex<()>,
}

impl McpBridge {
    fn acquire(
        service: &Arc<OpenCodeService>,
        directory: &str,
        config: &ComputerUseConfig,
    ) -> anyhow::Result<Arc<Self>> {
        let key = (Arc::as_ptr(service) as usize, directory.to_owned());
        let bridge = {
            let mut bridges = BRIDGES.get_or_init(Default::default).lock();
            bridges.retain(|_, bridge| bridge.strong_count() > 0);
            if let Some(bridge) = bridges.get(&key).and_then(Weak::upgrade) {
                bridge
            } else {
                let sessions_directory = create_process_directory()?;
                let bridge = Arc::new(Self {
                    service: service.clone(),
                    directory: directory.to_owned(),
                    server: format!("waku_js_repl_{}", Uuid::new_v4().simple()),
                    config: json!({
                        "type": "local",
                        "command": [config.repl_path],
                        "cwd": directory,
                        // This is already a JavaScript tool. Keep OpenCode's extra
                        // execute/codemode wrapper out of the agent-facing API.
                        "codemode": false,
                        "environment": {
                            "WAKU_COMPUTER_USE_SESSIONS_DIRECTORY": sessions_directory,
                        },
                        "timeout": {"startup": 10000, "catalog": 10000, "execution": 300000},
                    }),
                    sessions_directory,
                    registration: Mutex::new(()),
                });
                bridges.insert(key, Arc::downgrade(&bridge));
                bridge
            }
        };
        // Connect outside the global `BRIDGES` lock so up to 12s of HTTP
        // polling for one workspace never blocks other workspaces. Same-key
        // callers share `bridge` and serialize on `bridge.registration`.
        bridge.ensure_connected()?;
        Ok(bridge)
    }

    fn ensure_connected(&self) -> anyhow::Result<()> {
        let _registration = self.registration.lock();
        let endpoint = self.service.endpoint();
        let status = opencode_api::list_mcp(&endpoint, &self.directory)?
            .into_iter()
            .find(|server| server["name"] == self.server);
        if status
            .as_ref()
            .is_some_and(|server| server["status"]["status"] == "connected")
        {
            return Ok(());
        }
        opencode_api::add_mcp(&endpoint, &self.directory, &self.server, &self.config)
            .context("could not connect OpenCode to Waku Computer Use")?;
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let status = opencode_api::list_mcp(&endpoint, &self.directory)?
                .into_iter()
                .find(|server| server["name"] == self.server);
            match status
                .as_ref()
                .and_then(|server| server["status"]["status"].as_str())
            {
                Some("connected") => return Ok(()),
                Some("failed" | "disabled" | "needs_auth") => bail!(
                    "OpenCode could not start Waku Computer Use: {}",
                    status
                        .as_ref()
                        .and_then(|server| server["status"]["error"].as_str())
                        .unwrap_or("MCP server unavailable")
                ),
                _ if Instant::now() >= deadline => {
                    bail!("OpenCode timed out connecting Waku Computer Use")
                }
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_directory.join(format!(
            "{}.json",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(session_id),
        ))
    }
}

impl Drop for McpBridge {
    fn drop(&mut self) {
        let endpoint = self.service.endpoint();
        let directory = self.directory.clone();
        let server = self.server.clone();
        let _ = fs::remove_dir_all(&self.sessions_directory);
        // Offload the best-effort DELETE so dropping a driver cannot block the
        // worker or caller for up to 10s against a stalled OpenCode service.
        if std::thread::Builder::new()
            .name("waku-opencode-mcp-cleanup".into())
            .spawn(move || {
                let _ = opencode_api::remove_mcp_with_timeout(
                    &endpoint,
                    &directory,
                    &server,
                    MCP_CLEANUP_TIMEOUT,
                );
            })
            .is_err()
        {
            let _ = opencode_api::remove_mcp_with_timeout(
                &self.service.endpoint(),
                &self.directory,
                &self.server,
                MCP_CLEANUP_TIMEOUT,
            );
        }
    }
}

pub(super) struct OpenCodeComputerUse {
    runtime: ComputerUseRuntime,
    bridge: Arc<McpBridge>,
    session_id: String,
}

impl OpenCodeComputerUse {
    pub(super) fn start(
        service: &Arc<OpenCodeService>,
        directory: &str,
        session_id: &str,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        let runtime = ComputerUseRuntime::start(events)?;
        let bridge = McpBridge::acquire(service, directory, &runtime.config)?;
        let this = Self {
            runtime,
            bridge,
            session_id: session_id.to_owned(),
        };
        // Serialize registration and cleanup for a resumed native session so
        // an older driver cannot revoke the driver replacing it.
        let registration = this.bridge.registration.lock();
        let path = this.bridge.session_path(session_id);
        let temporary = path.with_extension("tmp");
        fs::write(
            &temporary,
            serde_json::to_vec(&json!({
                "server_path": this.runtime.config.server_path,
                "process_directory": this.runtime.config.process_directory,
                "cwd": directory,
            }))?,
        )?;
        fs::rename(&temporary, &path)?;
        let instructions = format!(
            "Computer Use is enabled for this Waku session. Use the `js` and `js_reset` tools from MCP server `{}`. These are direct MCP tools; call them directly. The host routes calls to this session automatically. Before using Computer Use, read the bundled skill at {} for the complete native API and operating instructions. Initialize with `await setupComputerUseRuntime({{ globals: globalThis }})`, then call methods such as `cua.list_apps()` directly. Use `jsRepl.write(...)` for output and `await jsRepl.emitImage(...)` for images. Bindings persist until `js_reset`. The skill documents all available methods; there is no public tool-discovery or generic dispatch API.",
            this.bridge.server,
            this.runtime.config.skill_path.display(),
        );
        let attached = opencode_api::put_instruction_entry(
            &service.endpoint(),
            session_id,
            INSTRUCTION_KEY,
            &instructions,
        )
        .map_err(|error| anyhow!("could not attach OpenCode Computer Use instructions: {error}"));
        drop(registration);
        attached?;
        Ok(this)
    }

    pub(super) fn ensure_connected(&self) -> anyhow::Result<()> {
        self.bridge.ensure_connected()
    }

    pub(super) fn stop(&self) {
        self.runtime.stop();
    }
}

impl Drop for OpenCodeComputerUse {
    fn drop(&mut self) {
        // Worker ownership keeps these blocking cleanup calls off the UI
        // thread. Removing the registration revokes this session immediately;
        // the shared bridge stays up while any other Waku session uses it.
        let _registration = self.bridge.registration.lock();
        let path = self.bridge.session_path(&self.session_id);
        let owns_registration = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|config| {
                config["process_directory"]
                    == self
                        .runtime
                        .config
                        .process_directory
                        .to_string_lossy()
                        .as_ref()
            });
        if owns_registration {
            let _ = fs::remove_file(path);
            let _ = opencode_api::remove_instruction_entry(
                &self.bridge.service.endpoint(),
                &self.session_id,
                INSTRUCTION_KEY,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use crate::http_wire::Endpoint;

    use super::*;

    fn test_config() -> ComputerUseConfig {
        ComputerUseConfig {
            server_path: PathBuf::from("fake-server"),
            repl_path: PathBuf::from("fake-repl"),
            skill_path: PathBuf::from("fake-skill"),
            process_directory: PathBuf::from("fake-proc"),
        }
    }

    fn read_http_request(socket: &mut std::net::TcpStream) -> (String, String) {
        let mut reader = std::io::BufReader::new(socket);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut content_length = 0_usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0_u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).unwrap();
        }
        (
            request_line,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    fn write_json(socket: &mut std::net::TcpStream, status: &str, body: &Value) {
        let body = body.to_string();
        let _ = write!(
            socket,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.flush();
    }

    fn spawn_mcp_server(
        hold_first_list: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    ) -> (Endpoint, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let put_count = Arc::new(AtomicUsize::new(0));
        let registered = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_puts = Arc::clone(&put_count);
        let server_registered = Arc::clone(&registered);

        thread::spawn(move || {
            let mut hold = hold_first_list;
            while let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
                let (request_line, _) = read_http_request(&mut socket);
                if request_line.starts_with("GET /api/mcp") {
                    if let Some((entered_tx, release_rx)) = hold.take() {
                        let _ = entered_tx.send(());
                        let _ = release_rx.recv_timeout(Duration::from_secs(5));
                    }
                    let items: Vec<Value> = server_registered
                        .lock()
                        .iter()
                        .map(|name| json!({"name": name, "status": {"status": "connected"}}))
                        .collect();
                    write_json(&mut socket, "200 OK", &json!({"data": items}));
                } else if request_line.starts_with("PUT /api/experimental/mcp/") {
                    let name = request_line
                        .strip_prefix("PUT /api/experimental/mcp/")
                        .and_then(|rest| rest.split(['?', ' ']).next())
                        .unwrap_or_default()
                        .to_owned();
                    server_puts.fetch_add(1, Ordering::SeqCst);
                    server_registered.lock().push(name);
                    let _ = socket.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
                } else if request_line.starts_with("DELETE /api/experimental/mcp/") {
                    let _ = socket.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
                }
            }
        });
        (Endpoint::local(port), put_count, registered)
    }

    #[test]
    fn acquire_does_not_hold_global_bridges_lock_during_ensure_connected() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (endpoint_a, _, _) = spawn_mcp_server(Some((entered_tx, release_rx)));
        let (endpoint_b, _, _) = spawn_mcp_server(None);

        let service_a = OpenCodeService::for_test(endpoint_a);
        let service_b = OpenCodeService::for_test(endpoint_b);
        let config = test_config();

        let dir_a = format!("/work-a-{}", Uuid::new_v4());
        let dir_b = format!("/work-b-{}", Uuid::new_v4());

        let svc_a = Arc::clone(&service_a);
        let cfg_a = config.clone();
        let d_a = dir_a.clone();
        let slow_acquire = thread::spawn(move || McpBridge::acquire(&svc_a, &d_a, &cfg_a));

        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("dir_a acquire should reach ensure_connected");

        // Acquiring a bridge for another directory must complete immediately
        // even while dir_a is blocked inside `ensure_connected()`.
        let started = Instant::now();
        let bridge_b = McpBridge::acquire(&service_b, &dir_b, &config)
            .expect("dir_b acquire should not block behind dir_a");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "dir_b blocked on BRIDGES lock: {:?}",
            started.elapsed()
        );

        let _ = release_tx.send(());
        let bridge_a = slow_acquire.join().unwrap().unwrap();
        assert_ne!(bridge_a.server, bridge_b.server);
        drop(bridge_b);
        drop(bridge_a);
    }

    #[test]
    fn concurrent_acquire_for_same_directory_shares_one_bridge() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (endpoint, put_count, _) = spawn_mcp_server(Some((entered_tx, release_rx)));
        let service = OpenCodeService::for_test(endpoint);
        let config = test_config();
        let dir = format!("/work-shared-{}", Uuid::new_v4());

        let s1 = Arc::clone(&service);
        let d1 = dir.clone();
        let c1 = config.clone();
        let t1 = thread::spawn(move || McpBridge::acquire(&s1, &d1, &c1).unwrap());

        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let s2 = Arc::clone(&service);
        let d2 = dir.clone();
        let c2 = config.clone();
        let t2 = thread::spawn(move || McpBridge::acquire(&s2, &d2, &c2).unwrap());

        thread::sleep(Duration::from_millis(50));
        let _ = release_tx.send(());

        let b1 = t1.join().unwrap();
        let b2 = t2.join().unwrap();
        assert!(Arc::ptr_eq(&b1, &b2));
        assert_eq!(put_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mcp_bridge_drop_offloads_stalled_delete_request() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (delete_seen_tx, delete_seen_rx) = mpsc::channel();
        let (release_server_tx, release_server_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let (request_line, _) = read_http_request(&mut socket);
            let _ = delete_seen_tx.send(request_line);
            // Intentionally stall without replying so a synchronous drop
            // would block until the HTTP timeout expires.
            let _ = release_server_rx.recv_timeout(Duration::from_secs(5));
        });

        let service = OpenCodeService::for_test(Endpoint::local(port));
        let sessions_directory = create_process_directory().unwrap();
        let bridge = McpBridge {
            service,
            directory: "/work-drop".into(),
            server: "waku_js_repl_0123456789abcdef0123456789abcdef".into(),
            sessions_directory: sessions_directory.clone(),
            config: json!({}),
            registration: Mutex::new(()),
        };

        let started = Instant::now();
        drop(bridge);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "McpBridge::drop blocked synchronously for {:?}",
            started.elapsed()
        );
        assert!(!sessions_directory.exists());

        let request = delete_seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("background cleanup should still issue DELETE");
        assert!(
            request.starts_with("DELETE /api/experimental/mcp/waku_js_repl_0123456789abcdef0123456789abcdef"),
            "{request}"
        );

        let _ = release_server_tx.send(());
        server.join().unwrap();
    }
}

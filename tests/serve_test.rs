#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn signals_stop_listeners_even_with_an_incomplete_http_request() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let store = tempfile::tempdir().unwrap();
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        std::fs::write(store.path().join("config.yaml"), format!(
            "serve:\n  port: {}\n  shutdown_timeout: 1\nsync:\n  home_networks: [127.0.0.0/8]\n", address.port()
        )).unwrap();
        drop(reservation);
        let mut server = Server(
            Command::new(assert_cmd::cargo::cargo_bin!("kanban4ai"))
                .arg("serve")
                .env("KANBAN_HOME", store.path())
                .env_remove("KANBAN_PROJECT")
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut probe = loop {
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "server exited before readiness"
            );
            match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(error) => panic!("server did not listen: {error}"),
            }
        };
        probe
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        probe
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        probe.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("{\"ok\":true}"), "{response}");
        let mut incomplete = TcpStream::connect(address).unwrap();
        incomplete.write_all(b"GET /health HTTP/1.1\r\n").unwrap();
        // SAFETY: the PID belongs to the child kept alive by Server.
        assert_eq!(
            unsafe { libc::kill(server.0.id() as libc::pid_t, signal) },
            0
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = server.0.try_wait().unwrap() {
                assert!(status.success(), "signal {signal}: {status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "shutdown hung on an incomplete request"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
    }
}

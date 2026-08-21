//! Process-level coverage for the shipped testing UI binary.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(port: u16, method: &str, path: &str, content_type: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to testing UI");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set response timeout");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("write HTTP request");
    stream.flush().expect("flush HTTP request");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read HTTP response");
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP response has headers");
    let status = head
        .split_whitespace()
        .nth(1)
        .expect("HTTP response has a status")
        .parse()
        .expect("HTTP status is numeric");
    (status, body.to_string())
}

fn json(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    request(port, method, path, "application/json", body)
}

fn start_server() -> (Server, u16) {
    let reservation = TcpListener::bind(("127.0.0.1", 0)).expect("reserve local port");
    let port = reservation.local_addr().expect("reserved address").port();
    drop(reservation);

    let child = Command::new(env!("CARGO_BIN_EXE_tinymemory-testing-ui"))
        .env("TINYMEMORY_TESTING_UI_ADDR", format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start testing UI binary");
    let server = Server(child);

    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (server, port);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("testing UI did not start on its reserved local port");
}

#[test]
fn shipped_binary_drives_the_local_memory_and_document_workflows() {
    let (_server, port) = start_server();

    let (status, body) = json(port, "GET", "/api/status", "");
    assert_eq!(status, 200);
    assert!(body.contains("\"connected\":false"));

    let (status, body) = json(port, "POST", "/api/connect", r#"{"engine":"local"}"#);
    assert_eq!(status, 200);
    assert!(body.contains("\"driver_id\":\"tinycortex\""));

    let entry = r#"{"namespace":"e2e","key":"welcome","content":"hello from the binary","category":"core","session_id":"s1","taint":"external_sync"}"#;
    assert_eq!(json(port, "POST", "/api/store", entry).0, 204);

    for (path, needle) in [
        (
            "/api/get?namespace=e2e&key=welcome",
            "hello from the binary",
        ),
        (
            "/api/list?namespace=e2e&category=core&session_id=s1",
            "welcome",
        ),
        ("/api/namespaces", "e2e"),
        ("/api/export?limit=5", "welcome"),
        ("/api/documents/formats", "plain_text"),
    ] {
        let (status, body) = json(port, "GET", path, "");
        assert_eq!(status, 200, "unexpected status for {path}: {body}");
        assert!(body.contains(needle), "missing {needle:?} in {body}");
    }

    let recall = r#"{"query":"hello","namespace":"e2e","category":"core","session_id":"s1","limit":3,"min_score":0.0,"cross_session":false}"#;
    let (status, body) = json(port, "POST", "/api/recall", recall);
    assert_eq!(status, 200);
    assert!(body.contains("welcome"));

    let boundary = "tinymemory-process-boundary";
    let multipart = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"namespace\"\r\n\r\ne2e\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nuploaded\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"tags\"\r\n\r\nprocess, coverage\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"guide.txt\"\r\nContent-Type: text/plain\r\n\r\nuploaded through the shipped binary\r\n\
         --{boundary}--\r\n"
    );
    let (status, body) = request(
        port,
        "POST",
        "/api/documents/upload",
        &format!("multipart/form-data; boundary={boundary}"),
        &multipart,
    );
    assert_eq!(status, 200, "upload failed: {body}");
    assert!(body.contains("uploaded"));

    let (status, body) = json(port, "GET", "/api/get?namespace=e2e&key=uploaded", "");
    assert_eq!(status, 200);
    assert!(body.contains("uploaded through the shipped binary"));

    let (status, body) = json(
        port,
        "POST",
        "/api/forget",
        r#"{"namespace":"e2e","key":"welcome"}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(body, "true");
    assert_eq!(json(port, "POST", "/api/disconnect", "{}").0, 200);
    assert_eq!(json(port, "POST", "/api/store", entry).0, 409);
}

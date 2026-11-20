//! The built-in static file server.
//!
//! A `serve` service publishes a directory over HTTP, or HTTPS with a
//! certificate and key in PEM files, on one or more addresses; port 0
//! picks a free one, which steps then find as `{{services.NAME.port}}`.
//! It answers what a package manager asks for, and nothing more: `GET`
//! and `HEAD` of regular files, with `Content-Length`, on connections
//! that are kept alive unless the client says otherwise. A path that is
//! not a file is `404 Not Found`, as Python's `http.server` answers, and
//! a path can never leave the directory. It replaces the
//! `python3 -m http.server` processes joe Linux's tests start, which
//! nothing checked were listening before a guest tried them.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

/// How long an idle connection is kept.
const IDLE: Duration = Duration::from_secs(30);

/// A function that records one line of the service's log.
pub type LogFn = Arc<dyn Fn(String) + Send + Sync>;

/// A TLS acceptor for the certificate chain and key in PEM files.
pub fn tls_acceptor(cert: &Path, key: &Path) -> Result<TlsAcceptor, String> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("cannot read {}: {e}", cert.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("cannot read {}: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!("{} holds no certificate", cert.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| format!("cannot read the key {}: {e}", key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("the certificate and key do not make a server identity: {e}"))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Serve `root` on `listener` until `shutdown` fires.
pub async fn serve(
    listener: TcpListener,
    root: PathBuf,
    tls: Option<TlsAcceptor>,
    log: LogFn,
    shutdown: CancellationToken,
) {
    let root = Arc::new(root);
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    log(format!("accept failed: {e}"));
                    continue;
                }
            },
            _ = shutdown.cancelled() => return,
        };
        let root = Arc::clone(&root);
        let log = Arc::clone(&log);
        let tls = tls.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let served = async {
                match tls {
                    Some(acceptor) => match acceptor.accept(stream).await {
                        Ok(s) => connection(s, &root, &log).await,
                        Err(e) => log(format!("{peer}: TLS handshake failed: {e}")),
                    },
                    None => connection(stream, &root, &log).await,
                }
            };
            tokio::select! {
                _ = served => {}
                _ = shutdown.cancelled() => {}
            }
        });
    }
}

async fn connection<S: AsyncRead + AsyncWrite + Unpin>(stream: S, root: &Path, log: &LogFn) {
    let mut stream = BufReader::new(stream);
    loop {
        let mut request_line = String::new();
        match tokio::time::timeout(IDLE, stream.read_line(&mut request_line)).await {
            Ok(Ok(0)) | Err(_) => return,
            Ok(Err(_)) => return,
            Ok(Ok(_)) => {}
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let version = parts.next().unwrap_or("HTTP/1.0").to_string();
        let mut close = version != "HTTP/1.1";
        loop {
            let mut header = String::new();
            match stream.read_line(&mut header).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("connection") {
                    let value = value.trim();
                    if value.eq_ignore_ascii_case("close") {
                        close = true;
                    } else if value.eq_ignore_ascii_case("keep-alive") {
                        close = false;
                    }
                }
            }
        }

        let outcome = match method.as_str() {
            "GET" | "HEAD" => {
                respond(stream.get_mut(), root, &target, method == "HEAD", close).await
            }
            _ => status(stream.get_mut(), 405, "Method Not Allowed", close)
                .await
                .map(|_| 405),
        };
        match outcome {
            Ok(code) => log(format!("{method} {target} {code}")),
            Err(e) => {
                log(format!("{method} {target}: {e}"));
                return;
            }
        }
        if close {
            let _ = stream.get_mut().shutdown().await;
            return;
        }
    }
}

async fn respond<S: AsyncWrite + Unpin>(
    stream: &mut S,
    root: &Path,
    target: &str,
    head: bool,
    close: bool,
) -> std::io::Result<u16> {
    let Some(path) = resolve(root, target) else {
        return status(stream, 404, "Not Found", close).await.map(|_| 404);
    };
    let path = if path.is_dir() {
        path.join("index.html")
    } else {
        path
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return status(stream, 404, "Not Found", close).await.map(|_| 404),
    };
    let meta = file.metadata().await?;
    if !meta.is_file() {
        return status(stream, 404, "Not Found", close).await.map(|_| 404);
    }
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: {}\r\n\r\n",
        meta.len(),
        content_type(&path),
        if close { "close" } else { "keep-alive" }
    );
    stream.write_all(headers.as_bytes()).await?;
    if !head {
        let mut file = file;
        tokio::io::copy(&mut file, stream).await?;
    }
    stream.flush().await?;
    Ok(200)
}

async fn status<S: AsyncWrite + Unpin>(
    stream: &mut S,
    code: u16,
    reason: &str,
    close: bool,
) -> std::io::Result<()> {
    let body = format!("{code} {reason}\n");
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: {}\r\n\r\n{body}",
        body.len(),
        if close { "close" } else { "keep-alive" }
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// The file `target` names under `root`, or `None` if it would leave it.
fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(path)?;
    let mut out = root.to_path_buf();
    for component in Path::new(&decoded).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("txt" | "zon" | "conf") => "text/plain; charset=utf-8",
        Some("json") => "application/json",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn paths_stay_inside_the_root() {
        let root = Path::new("/srv");
        assert_eq!(
            resolve(root, "/repo.zon"),
            Some(PathBuf::from("/srv/repo.zon"))
        );
        assert_eq!(resolve(root, "/a%20b?x=1"), Some(PathBuf::from("/srv/a b")));
        assert_eq!(resolve(root, "/../etc/passwd"), None);
        assert_eq!(resolve(root, "/a/%2e%2e/%2e%2e/x"), None);
    }

    async fn get(addr: std::net::SocketAddr, request: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn serves_files_and_404s() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("repo.zon"), "hello\n").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let logged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let l = Arc::clone(&logged);
        let log: LogFn = Arc::new(move |line| l.lock().unwrap().push(line));
        tokio::spawn(serve(
            listener,
            tmp.path().to_path_buf(),
            None,
            log,
            shutdown.clone(),
        ));

        let ok = get(addr, "GET /repo.zon HTTP/1.0\r\n\r\n").await;
        assert!(ok.starts_with("HTTP/1.1 200 OK\r\n"), "{ok}");
        assert!(ok.contains("Content-Length: 6\r\n"), "{ok}");
        assert!(ok.ends_with("\r\n\r\nhello\n"), "{ok}");
        let missing = get(
            addr,
            "GET /nowhere/repo.zon HTTP/1.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            missing.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{missing}"
        );
        let head = get(addr, "HEAD /repo.zon HTTP/1.1\r\nConnection: close\r\n\r\n").await;
        assert!(!head.contains("hello"), "{head}");
        // Two requests on one kept-alive connection.
        let two = get(
            addr,
            "GET /repo.zon HTTP/1.1\r\n\r\nGET /repo.zon HTTP/1.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(two.matches("200 OK").count(), 2, "{two}");
        shutdown.cancel();
        assert!(logged
            .lock()
            .unwrap()
            .iter()
            .any(|l| l == "GET /nowhere/repo.zon 404"));
    }
}

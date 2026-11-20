//! QEMU's machine protocol, for screenshots.

use std::path::Path;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Save what the machine's display shows to `png`.
#[cfg(unix)]
pub async fn screenshot(socket: &Path, png: &Path) -> Result<(), String> {
    let talk = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(|e| format!("cannot reach QEMU's control socket: {e}"))?;
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        // The greeting.
        lines.next_line().await.map_err(|e| e.to_string())?;
        for command in [
            serde_json::json!({ "execute": "qmp_capabilities" }),
            serde_json::json!({
                "execute": "screendump",
                "arguments": { "filename": png.display().to_string(), "format": "png" },
            }),
        ] {
            write
                .write_all(format!("{command}\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            loop {
                let line = lines
                    .next_line()
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("QEMU closed its control socket")?;
                let reply: serde_json::Value =
                    serde_json::from_str(&line).map_err(|e| e.to_string())?;
                if let Some(error) = reply.get("error") {
                    return Err(format!("QEMU refused: {error}"));
                }
                if reply.get("return").is_some() {
                    break;
                }
            }
        }
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(30), talk)
        .await
        .map_err(|_| "QEMU did not answer within 30s".to_string())?
}

/// Screenshots need a Unix socket.
#[cfg(not(unix))]
pub async fn screenshot(_socket: &Path, _png: &Path) -> Result<(), String> {
    Err("screenshots need a Unix host".into())
}

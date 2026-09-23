//! Newline-delimited JSON client for the Herdr socket.
//! One short-lived connection per request; long waits are chunked by the
//! callers so cancellation is observed promptly.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use super::HerdrError;

#[derive(Debug, Clone)]
pub struct SocketClient {
    pub path: PathBuf,
    pub default_timeout: Duration,
}

/// Socket resolution, mirroring Herdr's documented order:
/// `HERDR_SOCKET_PATH` → `HERDR_SESSION` → default session socket.
pub fn resolve_socket_path(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(c) = configured {
        return Some(PathBuf::from(c));
    }
    if let Ok(p) = std::env::var("HERDR_SOCKET_PATH") {
        return Some(PathBuf::from(p));
    }
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(|d| PathBuf::from(d).join("herdr"))
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config/herdr")))?;
    if let Ok(s) = std::env::var("HERDR_SESSION") {
        return Some(config_dir.join("sessions").join(s).join("herdr.sock"));
    }
    Some(config_dir.join("herdr.sock"))
}

impl SocketClient {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), default_timeout: Duration::from_secs(30) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Send one request and return its `result` object.
    pub fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> Result<Value, HerdrError> {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;
            let timeout = timeout.unwrap_or(self.default_timeout);
            let stream = UnixStream::connect(&self.path).map_err(|e| HerdrError::Unavailable(format!("{}: {e}", self.path.display())))?;
            stream.set_read_timeout(Some(timeout)).map_err(HerdrError::io)?;
            stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(HerdrError::io)?;
            let id = format!("orch-{}", uuid::Uuid::new_v4().simple());
            let req = json!({"id": id, "method": method, "params": params});
            let mut line = serde_json::to_string(&req).map_err(|e| HerdrError::Protocol(e.to_string()))?;
            line.push('\n');
            (&stream).write_all(line.as_bytes()).map_err(HerdrError::io)?;
            let mut reader = BufReader::new(&stream);
            loop {
                let mut resp = String::new();
                let n = reader.read_line(&mut resp).map_err(|e| {
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) {
                        HerdrError::ClientTimeout
                    } else {
                        HerdrError::io(e)
                    }
                })?;
                if n == 0 {
                    return Err(HerdrError::Protocol(format!("connection closed without a response to {method}")));
                }
                let v: Value = serde_json::from_str(resp.trim()).map_err(|e| HerdrError::Protocol(format!("invalid JSON from Herdr: {e}")))?;
                if v.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                    continue; // not ours (should not happen on a private connection)
                }
                if let Some(err) = v.get("error") {
                    return Err(HerdrError::Api {
                        code: err.get("code").and_then(Value::as_str).unwrap_or("unknown").to_string(),
                        message: err.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                    });
                }
                return v.get("result").cloned().ok_or_else(|| HerdrError::Protocol("response without result".into()));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (method, params, timeout);
            Err(HerdrError::Unavailable("raw socket client is Unix-only".into()))
        }
    }

    /// Request and deserialize one field of the result.
    pub fn request_field<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        field: &str,
        timeout: Option<Duration>,
    ) -> Result<T, HerdrError> {
        let r = self.request(method, params, timeout)?;
        let f = r.get(field).cloned().ok_or_else(|| HerdrError::Protocol(format!("{method}: result has no `{field}`")))?;
        serde_json::from_value(f).map_err(|e| HerdrError::Protocol(format!("{method}: unexpected `{field}` shape: {e}")))
    }
}

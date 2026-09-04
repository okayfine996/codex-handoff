use crate::HandoffError;
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(crate) enum Operation {
    Preflight,
    Usage,
}

impl Operation {
    fn error(self, message: impl Into<String>) -> HandoffError {
        match self {
            Self::Preflight => HandoffError::Preflight(message.into()),
            Self::Usage => HandoffError::Usage(message.into()),
        }
    }

    fn timeout_message(self) -> &'static str {
        match self {
            Self::Preflight => "Codex app-server timed out while verifying authentication",
            Self::Usage => "Codex app-server timed out while reading usage",
        }
    }

    fn ended_message(self) -> &'static str {
        match self {
            Self::Preflight => "Codex app-server ended before authentication was verified",
            Self::Usage => "Codex app-server ended before returning usage",
        }
    }
}

pub(crate) struct AppServerSession {
    _home: tempfile::TempDir,
    auth_path: PathBuf,
    child: Child,
    stdin: ChildStdin,
    receiver: Receiver<Result<Value, String>>,
    deadline: Instant,
    operation: Operation,
}

impl AppServerSession {
    pub(crate) fn start(
        codex_binary: &Path,
        auth: &[u8],
        operation: Operation,
        timeout: Duration,
    ) -> Result<Self, HandoffError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| operation.error("app-server timeout is too large"))?;
        let home = tempfile::tempdir()?;
        let auth_path = home.path().join("auth.json");
        fs::write(&auth_path, auth)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600))?;
        }

        let mut child = Command::new(codex_binary)
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                operation.error(format!("could not start Codex app-server: {error}"))
            })?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(operation.error("could not open app-server stdin"));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(operation.error("could not open app-server stdout"));
            }
        };
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let response = line.map_err(|error| error.to_string()).and_then(|line| {
                    serde_json::from_str(&line).map_err(|error| error.to_string())
                });
                if sender.send(response).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            _home: home,
            auth_path,
            child,
            stdin,
            receiver,
            deadline,
            operation,
        })
    }

    pub(crate) fn initialize(&mut self) -> Result<(), HandoffError> {
        let response = self.request(
            1,
            serde_json::json!({
                "method":"initialize",
                "id":1,
                "params":{"clientInfo":{"name":"codex-handoff","title":"Codex Handoff","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}
            }),
        )?;
        if response.get("error").is_some() {
            return Err(self
                .operation
                .error("Codex app-server rejected initialization"));
        }
        self.notify(serde_json::json!({"method":"initialized","params":{}}))
    }

    pub(crate) fn request(&mut self, id: i64, request: Value) -> Result<Value, HandoffError> {
        self.write(request)?;
        loop {
            let now = Instant::now();
            let Some(remaining) = self.deadline.checked_duration_since(now) else {
                return Err(self.operation.error(self.operation.timeout_message()));
            };
            match self.receiver.recv_timeout(remaining) {
                Ok(Ok(message)) if message.get("id").and_then(Value::as_i64) == Some(id) => {
                    return Ok(message);
                }
                Ok(Ok(_)) => continue,
                Ok(Err(error)) => {
                    return Err(self
                        .operation
                        .error(format!("invalid app-server response: {error}")));
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(self.operation.error(self.operation.timeout_message()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.operation.error(self.operation.ended_message()));
                }
            }
        }
    }

    pub(crate) fn notify(&mut self, notification: Value) -> Result<(), HandoffError> {
        self.write(notification)
    }

    pub(crate) fn read_auth(&self) -> Result<Vec<u8>, HandoffError> {
        Ok(fs::read(&self.auth_path)?)
    }

    fn write(&mut self, message: Value) -> Result<(), HandoffError> {
        writeln!(self.stdin, "{message}").map_err(|error| {
            self.operation
                .error(format!("could not write to app-server: {error}"))
        })
    }
}

impl Drop for AppServerSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

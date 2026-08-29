use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};
use thiserror::Error;

const SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
pub struct ProfileName(String);

impl ProfileName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ProfileNameError> {
        let value = value.into();
        let mut characters = value.chars();
        let Some(first) = characters.next() else {
            return Err(ProfileNameError);
        };

        if !first.is_ascii_alphanumeric()
            || !characters.all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
            })
        {
            return Err(ProfileNameError);
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_email(email: &str) -> Result<Self, ProfileNameError> {
        let local_part = email
            .split_once('@')
            .map(|(local_part, _)| local_part)
            .unwrap_or(email);
        let mut value: String = local_part
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        if !value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        {
            value.insert_str(0, "account-");
        }
        Self::parse(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileNameError;

impl fmt::Display for ProfileNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("profile names must begin with a letter or number and contain only letters, numbers, '.', '_' or '-'")
    }
}

impl std::error::Error for ProfileNameError {}

impl<'de> Deserialize<'de> for ProfileName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error)]
pub enum HandoffError {
    #[error("{0}")]
    InvalidProfileName(#[from] ProfileNameError),
    #[error("no active profile; run `ch init` first")]
    NoActiveProfile,
    #[error("profile `{0}` already exists")]
    ProfileExists(String),
    #[error("profile `{0}` does not exist")]
    ProfileMissing(String),
    #[error("live authentication file is missing or empty: {0}")]
    LiveAuthMissing(String),
    #[error("invalid Codex auth.json: {0}")]
    InvalidAuth(String),
    #[error("profile email changed from `{expected}` to `{actual}`; run `ch relogin {profile}`")]
    EmailMismatch {
        profile: String,
        expected: String,
        actual: String,
    },
    #[error("Codex or ChatGPT is running; close it before switching, or pass --force")]
    ClientRunning,
    #[error("could not check whether Codex or ChatGPT is running: {0}")]
    ClientCheckFailed(String),
    #[error("--force and --close-clients cannot be used together")]
    ConflictingProcessOptions,
    #[error("could not request graceful shutdown for: {0}")]
    ClientShutdownFailed(String),
    #[error("clients did not exit after graceful shutdown: {0}")]
    ClientShutdownTimeout(String),
    #[error("another ch operation is in progress")]
    Busy,
    #[error("authentication preflight failed: {0}")]
    Preflight(String),
    #[error("usage query failed: {0}")]
    Usage(String),
    #[error("insecure permissions on sensitive file: {0}")]
    InsecurePermissions(String),
    #[error("operation failed ({operation}) and rollback also failed: {rollback}")]
    Rollback { operation: String, rollback: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("metadata error: {0}")]
    Metadata(#[from] serde_json::Error),
}

#[derive(Clone, Debug)]
pub struct AppPaths {
    codex_home: PathBuf,
    handoff_home: PathBuf,
}

impl AppPaths {
    pub fn new(codex_home: PathBuf, handoff_home: PathBuf) -> Self {
        Self {
            codex_home,
            handoff_home,
        }
    }

    pub fn from_environment() -> Result<Self, HandoffError> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| HandoffError::Io(std::io::Error::other("HOME is not set")))?;
        let home = PathBuf::from(home);
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let handoff_home = std::env::var_os("CODEX_HANDOFF_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex-handoff"));
        Ok(Self::new(codex_home, handoff_home))
    }

    pub fn codex_home(&self) -> &Path {
        &self.codex_home
    }
    pub fn handoff_home(&self) -> &Path {
        &self.handoff_home
    }
    pub fn live_auth_path(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }
    pub fn profiles_dir(&self) -> PathBuf {
        self.handoff_home.join("profiles")
    }
    pub fn profile_dir(&self, name: &ProfileName) -> PathBuf {
        self.profiles_dir().join(name.as_str())
    }
    pub fn profile_auth_path(&self, name: &ProfileName) -> PathBuf {
        self.profile_dir(name).join("auth.json")
    }
    fn profile_metadata_path(&self, name: &ProfileName) -> PathBuf {
        self.profile_dir(name).join("profile.json")
    }
    fn state_path(&self) -> PathBuf {
        self.handoff_home.join("state.json")
    }
    fn lock_path(&self) -> PathBuf {
        self.handoff_home.join("lock")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileMetadata {
    pub schema_version: u8,
    pub name: ProfileName,
    pub email: String,
    pub created_at: DateTime<Utc>,
    pub last_synced_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct State {
    schema_version: u8,
    active_profile: ProfileName,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub active: ProfileMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageWindow {
    pub used_percent: u8,
    pub resets_at: Option<i64>,
    pub window_duration_mins: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageBucket {
    pub id: String,
    pub primary: Option<UsageWindow>,
    pub secondary: Option<UsageWindow>,
    pub reached_type: Option<String>,
    pub spend_control_reached: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResetCredit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResetCredits {
    pub available_count: u64,
    pub credits: Vec<ResetCredit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageReport {
    pub buckets: Vec<UsageBucket>,
    pub reset_credits: Option<ResetCredits>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageStatus {
    Available(UsageReport),
    Unavailable(String),
    NotQueried,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalHealth {
    Healthy,
    Unhealthy(String),
}

impl fmt::Display for LocalHealth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => formatter.write_str("ok"),
            Self::Unhealthy(reason) => write!(formatter, "error: {reason}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProfileListEntry {
    pub name: ProfileName,
    pub metadata: Option<ProfileMetadata>,
    pub active: bool,
    pub health: LocalHealth,
    pub usage: UsageStatus,
}

pub trait AuthProbe: Send + Sync {
    fn probe(&self, auth: &[u8]) -> Result<Vec<u8>, HandoffError>;
}

pub trait UsageReader: Send + Sync {
    fn read(&self, auth: &[u8]) -> Result<UsageReport, HandoffError>;
}

pub struct UnavailableUsageReader;

impl UsageReader for UnavailableUsageReader {
    fn read(&self, _auth: &[u8]) -> Result<UsageReport, HandoffError> {
        Err(HandoffError::Preflight(
            "usage reader is unavailable".into(),
        ))
    }
}

pub struct StaticProbe {
    result: Result<Vec<u8>, String>,
}

impl StaticProbe {
    pub fn success() -> Self {
        Self {
            result: Ok(Vec::new()),
        }
    }
    #[allow(dead_code)]
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            result: Err(message.into()),
        }
    }
}

impl AuthProbe for StaticProbe {
    fn probe(&self, auth: &[u8]) -> Result<Vec<u8>, HandoffError> {
        self.result
            .clone()
            .map(|replacement| {
                if replacement.is_empty() {
                    auth.to_vec()
                } else {
                    replacement
                }
            })
            .map_err(HandoffError::Preflight)
    }
}

pub trait ProcessGuard: Send + Sync {
    fn ensure_stopped(&self, force: bool) -> Result<(), HandoffError>;

    fn close_gracefully(&self) -> Result<(), HandoffError> {
        self.ensure_stopped(false)
    }
}

pub struct NoopProcessGuard;

impl ProcessGuard for NoopProcessGuard {
    fn ensure_stopped(&self, _force: bool) -> Result<(), HandoffError> {
        Ok(())
    }
}

pub struct SystemProcessGuard;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ClientProcess {
    name: &'static str,
    pid: u32,
}

impl ClientProcess {
    fn display_list(processes: &[Self]) -> String {
        processes
            .iter()
            .map(|process| format!("{} ({})", process.name, process.pid))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl ProcessGuard for SystemProcessGuard {
    fn ensure_stopped(&self, force: bool) -> Result<(), HandoffError> {
        ensure_clients_stopped(force, || self.running_clients())
    }

    fn close_gracefully(&self) -> Result<(), HandoffError> {
        let clients = self.running_clients()?;
        for application in ["Codex", "ChatGPT"] {
            if clients.iter().any(|client| client.name == application) {
                let status = Command::new("osascript")
                    .args(["-e", &format!("tell application \"{application}\" to quit")])
                    .status()?;
                if !status.success() {
                    return Err(HandoffError::ClientShutdownFailed(application.into()));
                }
            }
        }
        for client in clients.iter().filter(|client| client.name == "codex") {
            let status = Command::new("kill")
                .args(["-TERM", &client.pid.to_string()])
                .status()?;
            if !status.success() {
                return Err(HandoffError::ClientShutdownFailed(format!(
                    "codex ({})",
                    client.pid
                )));
            }
        }
        for _ in 0..50 {
            let remaining = self.running_clients()?;
            if remaining.is_empty() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
        let remaining = self.running_clients()?;
        if remaining.is_empty() {
            return Ok(());
        }
        Err(HandoffError::ClientShutdownTimeout(
            ClientProcess::display_list(&remaining),
        ))
    }
}

impl SystemProcessGuard {
    fn running_clients(&self) -> Result<Vec<ClientProcess>, HandoffError> {
        let mut clients = Vec::new();
        for name in ["codex", "Codex", "ChatGPT"] {
            let output = Command::new("pgrep").args(["-x", name]).output()?;
            if output.status.success() {
                for pid in String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| line.trim().parse::<u32>().ok())
                {
                    clients.push(ClientProcess { name, pid });
                }
            } else if output.status.code() != Some(1) {
                return Err(HandoffError::ClientCheckFailed(format!(
                    "process lookup for {name} failed"
                )));
            }
        }
        Ok(clients)
    }
}

fn ensure_clients_stopped(
    force: bool,
    running_clients: impl FnOnce() -> Result<Vec<ClientProcess>, HandoffError>,
) -> Result<(), HandoffError> {
    if force {
        return Ok(());
    }
    if running_clients()?.is_empty() {
        Ok(())
    } else {
        Err(HandoffError::ClientRunning)
    }
}

pub struct AppServerProbe {
    codex_binary: PathBuf,
}

impl AppServerProbe {
    pub fn from_path(codex_binary: impl Into<PathBuf>) -> Self {
        Self {
            codex_binary: codex_binary.into(),
        }
    }
}

impl AuthProbe for AppServerProbe {
    fn probe(&self, auth: &[u8]) -> Result<Vec<u8>, HandoffError> {
        let temporary = tempfile::tempdir()?;
        let auth_path = temporary.path().join("auth.json");
        fs::write(&auth_path, auth)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600))?;
        }
        let mut child = Command::new(&self.codex_binary)
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", temporary.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                HandoffError::Preflight(format!("could not start Codex app-server: {error}"))
            })?;
        let verified = (|| {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| HandoffError::Preflight("could not open app-server stdin".into()))?;
            let stdout = child.stdout.take().ok_or_else(|| {
                HandoffError::Preflight("could not open app-server stdout".into())
            })?;
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let response = line.map_err(HandoffError::Io).and_then(|line| {
                        serde_json::from_str::<serde_json::Value>(&line).map_err(|error| {
                            HandoffError::Preflight(format!("invalid app-server response: {error}"))
                        })
                    });
                    if sender.send(response).is_err() {
                        break;
                    }
                }
            });
            let receive = |expected_id| loop {
                match receiver.recv_timeout(Duration::from_secs(45)) {
                    Ok(Ok(message))
                        if message.get("id").and_then(serde_json::Value::as_i64)
                            == Some(expected_id) =>
                    {
                        break Ok(message);
                    }
                    Ok(Ok(_)) => continue,
                    Ok(Err(error)) => break Err(error),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        break Err(HandoffError::Preflight(
                            "Codex app-server timed out while verifying authentication".into(),
                        ));
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break Err(HandoffError::Preflight(
                            "Codex app-server ended before authentication was verified".into(),
                        ));
                    }
                }
            };

            writeln!(
                stdin,
                "{}",
                serde_json::json!({
                    "method":"initialize",
                    "id":1,
                    "params":{"clientInfo":{"name":"codex-handoff","title":"Codex Handoff","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}
                })
            )?;
            let initialized = receive(1)?;
            if initialized.get("error").is_some() {
                return Err(HandoffError::Preflight(
                    "Codex app-server rejected initialization".into(),
                ));
            }
            writeln!(
                stdin,
                "{}",
                serde_json::json!({"method":"initialized","params":{}})
            )?;
            writeln!(
                stdin,
                "{}",
                serde_json::json!({
                    "method":"account/read",
                    "id":2,
                    "params":{"refreshToken":true}
                })
            )?;

            let message = receive(2)?;
            if message.get("error").is_some() {
                return Err(HandoffError::Preflight(
                    "Codex rejected the authentication refresh".into(),
                ));
            }
            let account = message
                .pointer("/result/account")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    HandoffError::Preflight(
                        "Codex reported no authenticated ChatGPT account".into(),
                    )
                })?;
            if account.get("type").and_then(serde_json::Value::as_str) != Some("chatgpt") {
                return Err(HandoffError::Preflight(
                    "Codex did not verify ChatGPT authentication".into(),
                ));
            }
            let updated_auth = fs::read(auth_path)?;
            parse_auth(&updated_auth)?;
            Ok(updated_auth)
        })();
        let _ = child.kill();
        let _ = child.wait();
        verified
    }
}

pub struct AppServerUsageReader {
    codex_binary: PathBuf,
}

impl AppServerUsageReader {
    pub fn from_path(codex_binary: impl Into<PathBuf>) -> Self {
        Self {
            codex_binary: codex_binary.into(),
        }
    }
}

impl UsageReader for AppServerUsageReader {
    fn read(&self, auth: &[u8]) -> Result<UsageReport, HandoffError> {
        parse_auth(auth)?;
        let temporary = tempfile::tempdir()?;
        let auth_path = temporary.path().join("auth.json");
        fs::write(&auth_path, auth)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600))?;
        }
        let mut child = Command::new(&self.codex_binary)
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", temporary.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                HandoffError::Usage(format!("could not start Codex app-server: {error}"))
            })?;
        let report = (|| {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| HandoffError::Usage("could not open app-server stdin".into()))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| HandoffError::Usage("could not open app-server stdout".into()))?;
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let response = line.map_err(HandoffError::Io).and_then(|line| {
                        serde_json::from_str::<serde_json::Value>(&line).map_err(|error| {
                            HandoffError::Usage(format!("invalid app-server response: {error}"))
                        })
                    });
                    if sender.send(response).is_err() {
                        break;
                    }
                }
            });
            let receive = |expected_id| loop {
                match receiver.recv_timeout(Duration::from_secs(45)) {
                    Ok(Ok(message))
                        if message.get("id").and_then(serde_json::Value::as_i64)
                            == Some(expected_id) =>
                    {
                        break Ok(message);
                    }
                    Ok(Ok(_)) => continue,
                    Ok(Err(error)) => break Err(error),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        break Err(HandoffError::Usage(
                            "Codex app-server timed out while reading usage".into(),
                        ));
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break Err(HandoffError::Usage(
                            "Codex app-server ended before returning usage".into(),
                        ));
                    }
                }
            };

            writeln!(
                stdin,
                "{}",
                serde_json::json!({
                    "method":"initialize",
                    "id":1,
                    "params":{"clientInfo":{"name":"codex-handoff","title":"Codex Handoff","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}
                })
            )?;
            let initialized = receive(1)?;
            if initialized.get("error").is_some() {
                return Err(HandoffError::Usage(
                    "Codex app-server rejected initialization".into(),
                ));
            }
            writeln!(
                stdin,
                "{}",
                serde_json::json!({"method":"initialized","params":{}})
            )?;
            writeln!(
                stdin,
                "{}",
                serde_json::json!({
                    "method":"account/read",
                    "id":2,
                    "params":{"refreshToken":true}
                })
            )?;
            let account = receive(2)?;
            if account.get("error").is_some() {
                return Err(usage_rpc_error(
                    "Codex rejected the authentication refresh",
                    &account,
                ));
            }
            if account
                .pointer("/result/account/type")
                .and_then(serde_json::Value::as_str)
                != Some("chatgpt")
            {
                return Err(HandoffError::Usage(
                    "Codex did not verify ChatGPT authentication".into(),
                ));
            }
            parse_auth(&fs::read(&auth_path)?)?;
            writeln!(
                stdin,
                "{}",
                serde_json::json!({"method":"account/rateLimits/read","id":3})
            )?;
            let response = receive(3)?;
            if response.get("error").is_some() {
                return Err(usage_rpc_error(
                    "Codex rejected the usage request",
                    &response,
                ));
            }
            parse_usage_report(
                response
                    .get("result")
                    .ok_or_else(|| HandoffError::Usage("Codex returned no usage result".into()))?,
            )
        })();
        let _ = child.kill();
        let _ = child.wait();
        report
    }
}

fn usage_rpc_error(context: &str, response: &serde_json::Value) -> HandoffError {
    let message = response
        .pointer("/error/code")
        .and_then(serde_json::Value::as_i64)
        .map(|code| format!("{context} (JSON-RPC code {code})"))
        .unwrap_or_else(|| context.into());
    HandoffError::Usage(message)
}

fn parse_usage_report(value: &serde_json::Value) -> Result<UsageReport, HandoffError> {
    let object = value
        .as_object()
        .ok_or_else(|| HandoffError::Usage("usage result was not an object".into()))?;
    let mut buckets = object
        .get("rateLimitsByLimitId")
        .and_then(serde_json::Value::as_object)
        .filter(|buckets| !buckets.is_empty())
        .map(|buckets| {
            buckets
                .iter()
                .map(|(id, snapshot)| parse_usage_bucket(id, snapshot))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    if buckets.is_empty() {
        let legacy = object
            .get("rateLimits")
            .ok_or_else(|| HandoffError::Usage("Codex returned no rate-limit buckets".into()))?;
        buckets.push(parse_usage_bucket("default", legacy)?);
    }
    buckets.sort_by(|left, right| left.id.cmp(&right.id));
    let reset_credits = object
        .get("rateLimitResetCredits")
        .and_then(serde_json::Value::as_object)
        .map(parse_reset_credits)
        .transpose()?;
    Ok(UsageReport {
        buckets,
        reset_credits,
    })
}

fn parse_usage_bucket(id: &str, value: &serde_json::Value) -> Result<UsageBucket, HandoffError> {
    let object = value
        .as_object()
        .ok_or_else(|| HandoffError::Usage("rate-limit bucket was not an object".into()))?;
    Ok(UsageBucket {
        id: id.into(),
        primary: parse_usage_window(object.get("primary"))?,
        secondary: parse_usage_window(object.get("secondary"))?,
        reached_type: object
            .get("rateLimitReachedType")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        spend_control_reached: object
            .get("spendControlReached")
            .and_then(serde_json::Value::as_bool),
    })
}

fn parse_usage_window(
    value: Option<&serde_json::Value>,
) -> Result<Option<UsageWindow>, HandoffError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| HandoffError::Usage("rate-limit window was not an object".into()))?;
    let used_percent = object
        .get("usedPercent")
        .and_then(serde_json::Value::as_u64)
        .filter(|value| *value <= 100)
        .ok_or_else(|| {
            HandoffError::Usage("rate-limit window had an invalid used percentage".into())
        })? as u8;
    Ok(Some(UsageWindow {
        used_percent,
        resets_at: object.get("resetsAt").and_then(serde_json::Value::as_i64),
        window_duration_mins: object
            .get("windowDurationMins")
            .and_then(serde_json::Value::as_i64),
    }))
}

fn parse_reset_credits(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<ResetCredits, HandoffError> {
    let available_count = object
        .get("availableCount")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| HandoffError::Usage("reset credits had no available count".into()))?;
    let credits = object
        .get("credits")
        .and_then(serde_json::Value::as_array)
        .map(|credits| {
            credits
                .iter()
                .map(|credit| {
                    let credit = credit.as_object().ok_or_else(|| {
                        HandoffError::Usage("reset credit was not an object".into())
                    })?;
                    Ok(ResetCredit {
                        title: credit
                            .get("title")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        description: credit
                            .get("description")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        status: credit
                            .get("status")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown")
                            .into(),
                    })
                })
                .collect::<Result<Vec<_>, HandoffError>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(ResetCredits {
        available_count,
        credits,
    })
}

pub trait LoginRunner: Send + Sync {
    fn login(&self, codex_home: &Path) -> Result<(), HandoffError>;

    fn binary_path(&self) -> Option<&Path> {
        None
    }
}

pub struct SystemLoginRunner {
    codex_binary: PathBuf,
}

impl SystemLoginRunner {
    pub fn from_path(codex_binary: impl Into<PathBuf>) -> Self {
        Self {
            codex_binary: codex_binary.into(),
        }
    }
}

impl LoginRunner for SystemLoginRunner {
    fn login(&self, codex_home: &Path) -> Result<(), HandoffError> {
        let status = Command::new(&self.codex_binary)
            .arg("login")
            .env("CODEX_HOME", codex_home)
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(HandoffError::Preflight(
                "official `codex login` did not complete successfully".into(),
            ))
        }
    }

    fn binary_path(&self) -> Option<&Path> {
        Some(&self.codex_binary)
    }
}

pub struct NoopLoginRunner;
impl LoginRunner for NoopLoginRunner {
    fn login(&self, _codex_home: &Path) -> Result<(), HandoffError> {
        Err(HandoffError::Preflight(
            "login runner is unavailable".into(),
        ))
    }
}

pub struct App {
    paths: AppPaths,
    probe: Box<dyn AuthProbe>,
    process_guard: Box<dyn ProcessGuard>,
    login_runner: Box<dyn LoginRunner>,
    usage_reader: Box<dyn UsageReader>,
}

#[derive(Default)]
struct FileSnapshot {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl FileSnapshot {
    fn capture(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self, HandoffError> {
        let mut snapshot = Self::default();
        snapshot.capture_additional(paths)?;
        Ok(snapshot)
    }

    fn capture_additional(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<(), HandoffError> {
        for path in paths {
            if self.files.iter().any(|(known, _)| known == &path) {
                continue;
            }
            let contents = if path.exists() {
                Some(fs::read(&path)?)
            } else {
                None
            };
            self.files.push((path, contents));
        }
        Ok(())
    }

    fn restore(self, app: &App) -> Result<(), HandoffError> {
        for (path, contents) in self.files.into_iter().rev() {
            match contents {
                Some(contents) => app.atomic_write(&path, &contents)?,
                None if path.is_file() => fs::remove_file(path)?,
                None => {}
            }
        }
        Ok(())
    }
}

impl App {
    pub fn with_components(
        paths: AppPaths,
        probe: Box<dyn AuthProbe>,
        process_guard: Box<dyn ProcessGuard>,
    ) -> Self {
        Self {
            paths,
            probe,
            process_guard,
            login_runner: Box::new(NoopLoginRunner),
            usage_reader: Box::new(UnavailableUsageReader),
        }
    }

    pub fn with_all_components(
        paths: AppPaths,
        probe: Box<dyn AuthProbe>,
        process_guard: Box<dyn ProcessGuard>,
        login_runner: Box<dyn LoginRunner>,
    ) -> Self {
        Self {
            paths,
            probe,
            process_guard,
            login_runner,
            usage_reader: Box::new(UnavailableUsageReader),
        }
    }

    pub fn with_usage_reader(mut self, usage_reader: Box<dyn UsageReader>) -> Self {
        self.usage_reader = usage_reader;
        self
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn init(&self) -> Result<(), HandoffError> {
        let _lock = self.lock()?;
        self.ensure_layout()?;
        let auth = self.read_live_auth()?;
        let name = ProfileName::from_email(&parse_auth(&auth)?.email)?;
        if self.paths.state_path().exists() {
            return Err(HandoffError::ProfileExists(name.as_str().into()));
        }
        self.remove_empty_profile_dir(&name)?;
        if self.paths.profile_dir(&name).exists() {
            return Err(HandoffError::ProfileExists(name.as_str().into()));
        }
        let metadata = self.new_metadata(name.clone(), &auth)?;
        let result = self.transaction(
            vec![
                self.paths.profile_auth_path(&name),
                self.paths.profile_metadata_path(&name),
                self.paths.state_path(),
            ],
            || {
                self.save_profile(&metadata, &auth)?;
                self.save_state(&State {
                    schema_version: SCHEMA_VERSION,
                    active_profile: name.clone(),
                })
            },
        );
        if let Err(operation_error) = result {
            return match self.remove_empty_profile_dir(&name) {
                Ok(()) => Err(operation_error),
                Err(rollback_error) => Err(HandoffError::Rollback {
                    operation: operation_error.to_string(),
                    rollback: rollback_error.to_string(),
                }),
            };
        }
        Ok(())
    }

    pub fn status(&self) -> Result<Status, HandoffError> {
        let state = self.load_state()?;
        Ok(Status {
            active: self.load_profile(&state.active_profile)?,
        })
    }

    pub fn current_live_usage(&self) -> Result<UsageStatus, HandoffError> {
        let status = self.status()?;
        let usage = self
            .read_live_auth()
            .and_then(|auth| {
                self.ensure_profile_email(&status.active, &auth)?;
                self.usage_reader.read(&auth)
            })
            .map(UsageStatus::Available)
            .unwrap_or_else(|error| UsageStatus::Unavailable(error.to_string()));
        Ok(usage)
    }

    pub fn list(&self) -> Result<Vec<ProfileListEntry>, HandoffError> {
        self.list_with_usage(true)
    }

    fn list_with_usage(&self, include_usage: bool) -> Result<Vec<ProfileListEntry>, HandoffError> {
        let active = self.load_state().ok().map(|state| state.active_profile);
        let mut profiles = Vec::new();
        if !self.paths.profiles_dir().exists() {
            return Ok(profiles);
        }
        for entry in fs::read_dir(self.paths.profiles_dir())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|name| ProfileName::parse(name).ok())
            else {
                continue;
            };
            let is_active = active.as_ref() == Some(&name);
            let (metadata, health) = match self.load_profile(&name) {
                Ok(metadata) => match self.read_profile_auth(&name) {
                    Ok(auth) => match self.ensure_profile_email(&metadata, &auth) {
                        Ok(()) => (Some(metadata), LocalHealth::Healthy),
                        Err(error) => (Some(metadata), LocalHealth::Unhealthy(error.to_string())),
                    },
                    Err(error) => (Some(metadata), LocalHealth::Unhealthy(error.to_string())),
                },
                Err(error) => (None, LocalHealth::Unhealthy(error.to_string())),
            };
            let usage = if include_usage {
                match (&metadata, &health) {
                    (Some(metadata), LocalHealth::Healthy) => {
                        let auth = if is_active {
                            self.read_live_auth()
                        } else {
                            self.read_profile_auth(&name)
                        };
                        match auth
                            .and_then(|auth| {
                                self.ensure_profile_email(metadata, &auth)?;
                                self.usage_reader.read(&auth)
                            })
                            .map(UsageStatus::Available)
                        {
                            Ok(usage) => usage,
                            Err(error) => UsageStatus::Unavailable(error.to_string()),
                        }
                    }
                    _ => UsageStatus::Unavailable("local profile is unhealthy".into()),
                }
            } else {
                UsageStatus::NotQueried
            };
            profiles.push(ProfileListEntry {
                name,
                metadata,
                active: is_active,
                health,
                usage,
            });
        }
        profiles.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
        Ok(profiles)
    }

    pub fn sync(&self, force: bool) -> Result<(), HandoffError> {
        self.process_guard.ensure_stopped(force)?;
        let _lock = self.lock()?;
        let state = self.load_state()?;
        let auth = self.read_live_auth()?;
        let mut profile = self.load_profile(&state.active_profile)?;
        self.ensure_profile_email(&profile, &auth)?;
        profile.last_synced_at = Utc::now();
        self.transaction(
            vec![
                self.paths.profile_auth_path(&profile.name),
                self.paths.profile_metadata_path(&profile.name),
            ],
            || {
                self.write_auth(&self.paths.profile_auth_path(&profile.name), &auth)?;
                self.save_metadata(&profile)
            },
        )
    }

    pub fn switch(&self, name: ProfileName, force: bool) -> Result<(), HandoffError> {
        self.switch_with_options(name, force, false)
    }

    pub fn switch_with_options(
        &self,
        name: ProfileName,
        force: bool,
        close_clients: bool,
    ) -> Result<(), HandoffError> {
        if force && close_clients {
            return Err(HandoffError::ConflictingProcessOptions);
        }
        let _lock = self.lock()?;
        if close_clients {
            self.process_guard.close_gracefully()?;
        } else {
            self.process_guard.ensure_stopped(force)?;
        }
        let state = self.load_state()?;
        let target = self.load_profile(&name)?;
        let target_auth = self.read_profile_auth(&name)?;
        self.ensure_profile_email(&target, &target_auth)?;
        let probed_auth = self.probe.probe(&target_auth)?;
        self.ensure_profile_email(&target, &probed_auth)?;

        let live_auth = self.read_live_auth()?;
        let mut current = self.load_profile(&state.active_profile)?;
        self.ensure_profile_email(&current, &live_auth)?;
        current.last_synced_at = Utc::now();
        let mut refreshed_target = target;
        refreshed_target.last_synced_at = Utc::now();
        self.transaction(
            vec![
                self.paths.profile_auth_path(&current.name),
                self.paths.profile_metadata_path(&current.name),
                self.paths.profile_auth_path(&refreshed_target.name),
                self.paths.profile_metadata_path(&refreshed_target.name),
                self.paths.live_auth_path(),
                self.paths.state_path(),
            ],
            || {
                self.write_auth(&self.paths.profile_auth_path(&current.name), &live_auth)?;
                self.save_metadata(&current)?;
                self.write_auth(
                    &self.paths.profile_auth_path(&refreshed_target.name),
                    &probed_auth,
                )?;
                self.save_metadata(&refreshed_target)?;
                self.write_auth(&self.paths.live_auth_path(), &probed_auth)?;
                self.save_state(&State {
                    schema_version: SCHEMA_VERSION,
                    active_profile: name,
                })
            },
        )
    }

    pub fn add(&self, force: bool) -> Result<ProfileName, HandoffError> {
        self.login_profile(None, force, false)
    }

    pub fn relogin(&self, name: ProfileName, force: bool) -> Result<(), HandoffError> {
        self.login_profile(Some(name), force, true).map(|_| ())
    }

    fn login_profile(
        &self,
        requested_name: Option<ProfileName>,
        force: bool,
        replace: bool,
    ) -> Result<ProfileName, HandoffError> {
        self.process_guard.ensure_stopped(force)?;
        let _lock = self.lock()?;
        if replace {
            let name = requested_name
                .as_ref()
                .expect("relogin always provides a profile name");
            if !self.paths.profile_dir(name).exists() {
                return Err(HandoffError::ProfileMissing(name.as_str().into()));
            }
        }
        let current_state = self.load_state()?;
        let live = self.paths.live_auth_path();
        let current_auth = self.read_live_auth()?;
        let mut current_profile = self.load_profile(&current_state.active_profile)?;
        self.ensure_profile_email(&current_profile, &current_auth)?;
        current_profile.last_synced_at = Utc::now();
        let staging_dir = self.paths.handoff_home().join("staging");
        private_dir(&staging_dir)?;
        let staging = tempfile::NamedTempFile::new_in(&staging_dir)?;
        private_file(staging.as_file())?;
        let staging_path = staging.path().to_path_buf();
        drop(staging);
        let mut snapshot_paths = vec![
            self.paths.profile_auth_path(&current_profile.name),
            self.paths.profile_metadata_path(&current_profile.name),
            live.clone(),
            self.paths.state_path(),
        ];
        if let Some(name) = requested_name.as_ref() {
            snapshot_paths.extend([
                self.paths.profile_auth_path(name),
                self.paths.profile_metadata_path(name),
            ]);
        }
        let mut snapshot = FileSnapshot::capture(snapshot_paths)?;
        let mut saved_name = None;
        let result = (|| {
            self.write_auth(
                &self.paths.profile_auth_path(&current_profile.name),
                &current_auth,
            )?;
            self.save_metadata(&current_profile)?;
            fs::rename(&live, &staging_path)?;
            self.login_runner.login(self.paths.codex_home())?;
            let new_auth = self.read_live_auth()?;
            let name = match requested_name.as_ref() {
                Some(name) => name.clone(),
                None => ProfileName::from_email(&parse_auth(&new_auth)?.email)?,
            };
            if !replace && self.paths.profile_dir(&name).exists() {
                return Err(HandoffError::ProfileExists(name.as_str().into()));
            }
            if !replace {
                snapshot.capture_additional([
                    self.paths.profile_auth_path(&name),
                    self.paths.profile_metadata_path(&name),
                ])?;
            }
            let metadata = if replace {
                let mut existing = self.load_profile(&name)?;
                existing.email = parse_auth(&new_auth)?.email;
                existing.last_synced_at = Utc::now();
                existing
            } else {
                self.new_metadata(name.clone(), &new_auth)?
            };
            saved_name = Some(name.clone());
            self.save_profile(&metadata, &new_auth)?;
            self.write_auth(&live, &current_auth)?;
            self.save_state(&current_state)
        })();
        let cleanup = fs::remove_file(&staging_path);
        match result {
            Ok(()) => {
                cleanup?;
                Ok(saved_name.expect("saved profile name is set after successful login"))
            }
            Err(operation_error) => match snapshot.restore(self) {
                Ok(()) => {
                    let _ = cleanup;
                    if !replace && let Some(name) = saved_name {
                        let _ = self.remove_empty_profile_dir(&name);
                    }
                    Err(operation_error)
                }
                Err(rollback_error) => Err(HandoffError::Rollback {
                    operation: operation_error.to_string(),
                    rollback: rollback_error.to_string(),
                }),
            },
        }
    }

    pub fn doctor(&self) -> Vec<String> {
        let mut items = Vec::new();
        items.push(match self.login_runner.binary_path() {
            Some(binary) => match Command::new(binary).arg("--version").output() {
                Ok(output) if output.status.success() => format!(
                    "Codex CLI: ok ({})",
                    String::from_utf8_lossy(&output.stdout).trim()
                ),
                Ok(_) => "Codex CLI: error (could not read version)".into(),
                Err(error) => format!("Codex CLI: error ({error})"),
            },
            None => "Codex CLI: unavailable".into(),
        });
        items.push(format!(
            "vault: {}",
            if private_path_is_safe(self.paths.handoff_home(), true) {
                "ok"
            } else {
                "missing or insecure"
            }
        ));
        items.push(format!(
            "live auth: {}",
            if self
                .read_live_auth()
                .and_then(|auth| parse_auth(&auth).map(|_| ()))
                .is_ok()
            {
                "ok"
            } else {
                "missing, invalid, or unreadable"
            }
        ));
        match self.status() {
            Ok(status) => items.push(format!(
                "active profile: {} ({})",
                status.active.name.as_str(),
                status.active.email
            )),
            Err(error) => items.push(format!("active profile: error ({error})")),
        }
        match self.list_with_usage(false) {
            Ok(entries) => {
                for entry in entries {
                    items.push(format!("profile {}: {}", entry.name.as_str(), entry.health));
                }
            }
            Err(error) => items.push(format!("profiles: error ({error})")),
        }
        items.push(match self.process_guard.ensure_stopped(false) {
            Ok(()) => "client processes: stopped".into(),
            Err(HandoffError::ClientRunning) => "client processes: running".into(),
            Err(error) => format!("client processes: error ({error})"),
        });
        items.push(match self.lock_status() {
            Ok(status) => format!("lock: {status}"),
            Err(error) => format!("lock: error ({error})"),
        });
        items
    }

    fn ensure_layout(&self) -> Result<(), HandoffError> {
        private_dir(self.paths.handoff_home())?;
        private_dir(&self.paths.profiles_dir())
    }

    fn remove_empty_profile_dir(&self, name: &ProfileName) -> Result<(), HandoffError> {
        let path = self.paths.profile_dir(name);
        if path.is_dir() && fs::read_dir(&path)?.next().is_none() {
            fs::remove_dir(path)?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<File, HandoffError> {
        self.ensure_layout()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.paths.lock_path())?;
        private_file(&file)?;
        file.try_lock_exclusive().map_err(|_| HandoffError::Busy)?;
        Ok(file)
    }

    fn lock_status(&self) -> Result<&'static str, HandoffError> {
        let path = self.paths.lock_path();
        if !path.is_file() {
            return Ok("not created");
        }
        ensure_private_file_path(&path)?;
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                FileExt::unlock(&file)?;
                Ok("available")
            }
            Err(_) => Ok("busy"),
        }
    }

    fn read_live_auth(&self) -> Result<Vec<u8>, HandoffError> {
        let path = self.paths.live_auth_path();
        if !path.is_file() {
            return Err(HandoffError::LiveAuthMissing(path.display().to_string()));
        }
        ensure_private_file_path(&path)?;
        self.read_auth(&path)
    }

    fn read_profile_auth(&self, name: &ProfileName) -> Result<Vec<u8>, HandoffError> {
        self.ensure_private_profile_dir(name)?;
        let path = self.paths.profile_auth_path(name);
        ensure_private_file_path(&path)?;
        self.read_auth(&path)
    }

    fn read_auth(&self, path: &Path) -> Result<Vec<u8>, HandoffError> {
        let bytes = fs::read(path)?;
        if bytes.is_empty() {
            return Err(HandoffError::InvalidAuth("file is empty".into()));
        }
        parse_auth(&bytes)?;
        Ok(bytes)
    }

    fn new_metadata(
        &self,
        name: ProfileName,
        auth: &[u8],
    ) -> Result<ProfileMetadata, HandoffError> {
        let now = Utc::now();
        Ok(ProfileMetadata {
            schema_version: SCHEMA_VERSION,
            name,
            email: parse_auth(auth)?.email,
            created_at: now,
            last_synced_at: now,
        })
    }

    fn ensure_profile_email(
        &self,
        profile: &ProfileMetadata,
        auth: &[u8],
    ) -> Result<(), HandoffError> {
        let actual = parse_auth(auth)?.email;
        if actual != profile.email {
            return Err(HandoffError::EmailMismatch {
                profile: profile.name.as_str().into(),
                expected: profile.email.clone(),
                actual,
            });
        }
        Ok(())
    }

    fn save_profile(&self, profile: &ProfileMetadata, auth: &[u8]) -> Result<(), HandoffError> {
        private_dir(&self.paths.profile_dir(&profile.name))?;
        self.write_auth(&self.paths.profile_auth_path(&profile.name), auth)?;
        self.save_metadata(profile)
    }

    fn load_profile(&self, name: &ProfileName) -> Result<ProfileMetadata, HandoffError> {
        self.ensure_private_profile_dir(name)?;
        let path = self.paths.profile_metadata_path(name);
        if !path.is_file() {
            return Err(HandoffError::ProfileMissing(name.as_str().into()));
        }
        ensure_private_file_path(&path)?;
        let profile: ProfileMetadata = serde_json::from_slice(&fs::read(path)?)?;
        if profile.schema_version != SCHEMA_VERSION || profile.name != *name {
            return Err(HandoffError::InvalidAuth(
                "profile metadata is incompatible".into(),
            ));
        }
        Ok(profile)
    }

    fn ensure_private_profile_dir(&self, name: &ProfileName) -> Result<(), HandoffError> {
        let path = self.paths.profile_dir(name);
        if !path.is_dir() {
            return Err(HandoffError::ProfileMissing(name.as_str().into()));
        }
        ensure_private_dir_path(&path)
    }

    fn save_metadata(&self, profile: &ProfileMetadata) -> Result<(), HandoffError> {
        self.atomic_write(
            &self.paths.profile_metadata_path(&profile.name),
            &serde_json::to_vec_pretty(profile)?,
        )
    }

    fn load_state(&self) -> Result<State, HandoffError> {
        let path = self.paths.state_path();
        if !path.is_file() {
            return Err(HandoffError::NoActiveProfile);
        }
        ensure_private_file_path(&path)?;
        let state: State = serde_json::from_slice(&fs::read(path)?)?;
        if state.schema_version != SCHEMA_VERSION {
            return Err(HandoffError::InvalidAuth(
                "state metadata is incompatible".into(),
            ));
        }
        Ok(state)
    }

    fn save_state(&self, state: &State) -> Result<(), HandoffError> {
        self.atomic_write(&self.paths.state_path(), &serde_json::to_vec_pretty(state)?)
    }

    fn transaction<T>(
        &self,
        paths: Vec<PathBuf>,
        operation: impl FnOnce() -> Result<T, HandoffError>,
    ) -> Result<T, HandoffError> {
        let snapshot = FileSnapshot::capture(paths)?;
        match operation() {
            Ok(value) => Ok(value),
            Err(operation_error) => match snapshot.restore(self) {
                Ok(()) => Err(operation_error),
                Err(rollback_error) => Err(HandoffError::Rollback {
                    operation: operation_error.to_string(),
                    rollback: rollback_error.to_string(),
                }),
            },
        }
    }

    fn write_auth(&self, path: &Path, bytes: &[u8]) -> Result<(), HandoffError> {
        self.atomic_write(path, bytes)
    }

    fn atomic_write(&self, path: &Path, bytes: &[u8]) -> Result<(), HandoffError> {
        let parent = path
            .parent()
            .ok_or_else(|| HandoffError::Io(std::io::Error::other("path has no parent")))?;
        private_dir(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        private_file(file.as_file())?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        file.persist(path)
            .map_err(|error| HandoffError::Io(error.error))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn private_dir(path: &Path) -> Result<(), HandoffError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(file: &File) -> Result<(), HandoffError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn ensure_private_file_path(path: &Path) -> Result<(), HandoffError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(HandoffError::InsecurePermissions(
                path.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn ensure_private_dir_path(path: &Path) -> Result<(), HandoffError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(HandoffError::InsecurePermissions(
                path.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn private_path_is_safe(path: &Path, directory: bool) -> bool {
    if directory != path.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o077 == 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

struct AuthInfo {
    email: String,
}

fn parse_auth(bytes: &[u8]) -> Result<AuthInfo, HandoffError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| HandoffError::InvalidAuth(error.to_string()))?;
    if value.get("auth_mode").and_then(serde_json::Value::as_str) != Some("chatgpt") {
        return Err(HandoffError::InvalidAuth(
            "auth_mode must be chatgpt".into(),
        ));
    }
    let tokens = value
        .get("tokens")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| HandoffError::InvalidAuth("tokens object is missing".into()))?;
    for field in ["access_token", "id_token", "refresh_token"] {
        if tokens
            .get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            return Err(HandoffError::InvalidAuth(format!(
                "tokens.{field} is missing"
            )));
        }
    }
    let id_token = tokens["id_token"].as_str().expect("checked above");
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| HandoffError::InvalidAuth("id_token is not a JWT".into()))?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| HandoffError::InvalidAuth("id_token payload is invalid".into()))?;
    let payload: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|_| HandoffError::InvalidAuth("id_token payload is not JSON".into()))?;
    let email = payload
        .get("email")
        .and_then(serde_json::Value::as_str)
        .filter(|email| !email.is_empty())
        .ok_or_else(|| HandoffError::InvalidAuth("id_token has no email".into()))?;
    Ok(AuthInfo {
        email: email.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        App, AppPaths, AppServerProbe, AppServerUsageReader, AuthProbe, HandoffError, LoginRunner,
        NoopProcessGuard, ProcessGuard, ProfileName, StaticProbe, UsageBucket, UsageReader,
        UsageReport, UsageStatus, UsageWindow, ensure_clients_stopped,
    };
    use std::{
        fs,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    struct FakeLogin(Vec<u8>);

    struct RejectingProcessGuard;

    impl ProcessGuard for RejectingProcessGuard {
        fn ensure_stopped(&self, _force: bool) -> Result<(), HandoffError> {
            Err(HandoffError::ClientRunning)
        }
    }

    #[derive(Clone)]
    struct CloseableProcessGuard {
        closed: Arc<AtomicBool>,
    }

    impl CloseableProcessGuard {
        fn new() -> Self {
            Self {
                closed: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl ProcessGuard for CloseableProcessGuard {
        fn ensure_stopped(&self, force: bool) -> Result<(), HandoffError> {
            if force || self.closed.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(HandoffError::ClientRunning)
            }
        }

        fn close_gracefully(&self) -> Result<(), HandoffError> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    impl LoginRunner for FakeLogin {
        fn login(&self, codex_home: &Path) -> Result<(), HandoffError> {
            let auth_path = codex_home.join("auth.json");
            fs::write(&auth_path, &self.0)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        }
    }

    struct FakeUsageReader;

    #[derive(Clone)]
    struct CountingUsageReader(Arc<AtomicBool>);

    impl UsageReader for FakeUsageReader {
        fn read(&self, auth: &[u8]) -> Result<UsageReport, HandoffError> {
            let email = super::parse_auth(auth)?.email;
            if email == "work@example.com" {
                return Err(HandoffError::Preflight("quota service unavailable".into()));
            }
            Ok(UsageReport {
                buckets: vec![UsageBucket {
                    id: "codex".into(),
                    primary: Some(UsageWindow {
                        used_percent: 25,
                        resets_at: Some(1_700_000_000),
                        window_duration_mins: Some(300),
                    }),
                    secondary: None,
                    reached_type: None,
                    spend_control_reached: None,
                }],
                reset_credits: None,
            })
        }
    }

    impl UsageReader for CountingUsageReader {
        fn read(&self, _auth: &[u8]) -> Result<UsageReport, HandoffError> {
            self.0.store(true, Ordering::SeqCst);
            Err(HandoffError::Usage("usage lookup should not run".into()))
        }
    }

    fn jwt(email: &str) -> String {
        use base64::Engine;
        let payload = serde_json::json!({"email": email});
        format!(
            "header.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
        )
    }

    fn auth(email: &str, version: u8) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": format!("access-{version}"),
                "id_token": jwt(email),
                "refresh_token": format!("refresh-{version}")
            }
        }))
        .unwrap()
    }

    fn app() -> (tempfile::TempDir, App) {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        );
        (temporary, app)
    }

    fn write_live_auth(app: &App, bytes: &[u8]) {
        fs::create_dir_all(app.paths().codex_home()).unwrap();
        fs::write(app.paths().live_auth_path(), bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                app.paths().live_auth_path(),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
    }

    #[test]
    fn accepts_safe_profile_names() {
        assert!(ProfileName::parse("personal.work_1").is_ok());
    }

    #[test]
    fn rejects_path_traversal_profile_names() {
        assert!(ProfileName::parse("../outside").is_err());
        assert!(ProfileName::parse("work/client").is_err());
    }

    #[test]
    fn client_process_lookup_failures_are_reported_unless_forced() {
        let error = ensure_clients_stopped(false, || {
            Err(HandoffError::ClientCheckFailed("pgrep unavailable".into()))
        });
        assert!(matches!(error, Err(HandoffError::ClientCheckFailed(_))));

        let looked_up = AtomicBool::new(false);
        ensure_clients_stopped(true, || {
            looked_up.store(true, Ordering::SeqCst);
            Err(HandoffError::ClientCheckFailed(
                "should not be called".into(),
            ))
        })
        .unwrap();
        assert!(!looked_up.load(Ordering::SeqCst));
    }

    #[test]
    fn init_creates_a_private_profile_and_records_the_email() {
        let (_temporary, app) = app();
        write_live_auth(&app, &auth("personal@example.com", 1));

        app.init().unwrap();

        let status = app.status().unwrap();
        assert_eq!(status.active.name.as_str(), "personal");
        assert_eq!(status.active.email, "personal@example.com");
        assert_eq!(
            fs::read(app.paths().profile_auth_path(&status.active.name)).unwrap(),
            auth("personal@example.com", 1)
        );
    }

    #[test]
    fn init_derives_the_profile_name_from_the_email_local_part() {
        let (_temporary, app) = app();
        write_live_auth(&app, &auth("litesky+codex@example.com", 1));

        app.init().unwrap();

        let status = app.status().unwrap();
        assert_eq!(status.active.name.as_str(), "litesky-codex");
        assert_eq!(status.active.email, "litesky+codex@example.com");
    }

    #[test]
    fn init_recovers_from_an_empty_profile_directory() {
        let (_temporary, app) = app();
        write_live_auth(&app, &auth("personal@example.com", 1));
        fs::create_dir_all(
            app.paths()
                .profile_dir(&ProfileName::parse("personal").unwrap()),
        )
        .unwrap();

        app.init().unwrap();

        assert_eq!(app.status().unwrap().active.name.as_str(), "personal");
    }

    #[test]
    fn init_does_not_require_codex_processes_to_be_stopped() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(RejectingProcessGuard),
        );
        write_live_auth(&app, &auth("personal@example.com", 1));

        app.init().unwrap();

        assert_eq!(app.status().unwrap().active.name.as_str(), "personal");
    }

    #[test]
    fn sync_preserves_the_latest_live_authentication_bytes() {
        let (_temporary, app) = app();
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let refreshed = auth("personal@example.com", 2);
        write_live_auth(&app, &refreshed);

        app.sync(false).unwrap();

        assert_eq!(
            fs::read(
                app.paths()
                    .profile_auth_path(&ProfileName::parse("personal").unwrap())
            )
            .unwrap(),
            refreshed
        );
    }

    #[test]
    fn switch_saves_the_refreshed_source_before_activating_the_target() {
        let (_temporary, app) = app();
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        let work_metadata = app.new_metadata(work.clone(), &work_auth).unwrap();
        app.save_profile(&work_metadata, &work_auth).unwrap();
        let refreshed_personal = auth("personal@example.com", 2);
        write_live_auth(&app, &refreshed_personal);

        app.switch(work.clone(), false).unwrap();

        assert_eq!(
            fs::read(
                app.paths()
                    .profile_auth_path(&ProfileName::parse("personal").unwrap())
            )
            .unwrap(),
            refreshed_personal
        );
        assert_eq!(fs::read(app.paths().live_auth_path()).unwrap(), work_auth);
        assert_eq!(app.status().unwrap().active.name, work);
    }

    #[test]
    fn switch_can_close_clients_before_activating_the_target_profile() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let guard = CloseableProcessGuard::new();
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(guard.clone()),
        );
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        let work_metadata = app.new_metadata(work.clone(), &work_auth).unwrap();
        app.save_profile(&work_metadata, &work_auth).unwrap();

        app.switch_with_options(work.clone(), false, true).unwrap();

        assert!(guard.closed.load(Ordering::SeqCst));
        assert_eq!(app.status().unwrap().active.name, work);
    }

    #[test]
    fn list_includes_usage_for_each_profile_without_failing_all_profiles() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        )
        .with_usage_reader(Box::new(FakeUsageReader));
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        let work_metadata = app.new_metadata(work.clone(), &work_auth).unwrap();
        app.save_profile(&work_metadata, &work_auth).unwrap();

        let profiles = app.list().unwrap();

        let personal = profiles
            .iter()
            .find(|profile| profile.name.as_str() == "personal")
            .unwrap();
        assert!(matches!(personal.usage, UsageStatus::Available(_)));
        let work = profiles
            .iter()
            .find(|profile| profile.name.as_str() == "work")
            .unwrap();
        assert!(matches!(work.usage, UsageStatus::Unavailable(_)));
    }

    #[test]
    fn current_live_usage_reads_the_active_live_profile() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        )
        .with_usage_reader(Box::new(FakeUsageReader));
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();

        assert!(matches!(
            app.current_live_usage().unwrap(),
            UsageStatus::Available(_)
        ));
    }

    #[test]
    fn doctor_only_checks_local_profile_health_without_querying_usage() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let usage_was_read = Arc::new(AtomicBool::new(false));
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        )
        .with_usage_reader(Box::new(CountingUsageReader(usage_was_read.clone())));
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();

        app.doctor();

        assert!(!usage_was_read.load(Ordering::SeqCst));
    }

    #[test]
    fn failed_preflight_keeps_the_current_authentication_unchanged() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::failure("expired")),
            Box::new(NoopProcessGuard),
        );
        let personal_auth = auth("personal@example.com", 1);
        write_live_auth(&app, &personal_auth);
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        let work_metadata = app.new_metadata(work.clone(), &work_auth).unwrap();
        app.save_profile(&work_metadata, &work_auth).unwrap();

        assert!(matches!(
            app.switch(work, false),
            Err(super::HandoffError::Preflight(_))
        ));
        assert_eq!(
            fs::read(app.paths().live_auth_path()).unwrap(),
            personal_auth
        );
        assert_eq!(app.status().unwrap().active.name.as_str(), "personal");
    }

    #[test]
    fn add_stashes_the_live_profile_and_restores_it_after_official_login() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let personal_auth = auth("personal@example.com", 1);
        let work_auth = auth("work@example.com", 1);
        let app = App::with_all_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
            Box::new(FakeLogin(work_auth.clone())),
        );
        write_live_auth(&app, &personal_auth);
        app.init().unwrap();
        let refreshed_personal = auth("personal@example.com", 2);
        write_live_auth(&app, &refreshed_personal);

        app.add(false).unwrap();

        assert_eq!(
            fs::read(app.paths().live_auth_path()).unwrap(),
            refreshed_personal
        );
        assert_eq!(
            fs::read(
                app.paths()
                    .profile_auth_path(&ProfileName::parse("personal").unwrap())
            )
            .unwrap(),
            refreshed_personal
        );
        assert_eq!(
            fs::read(
                app.paths()
                    .profile_auth_path(&ProfileName::parse("work").unwrap())
            )
            .unwrap(),
            work_auth
        );
        assert_eq!(app.status().unwrap().active.name.as_str(), "personal");
    }

    #[test]
    fn failed_login_restores_the_active_authentication_and_state() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let personal_auth = auth("personal@example.com", 1);
        let app = App::with_all_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
            Box::new(super::NoopLoginRunner),
        );
        write_live_auth(&app, &personal_auth);
        app.init().unwrap();

        assert!(matches!(app.add(false), Err(HandoffError::Preflight(_))));
        assert_eq!(
            fs::read(app.paths().live_auth_path()).unwrap(),
            personal_auth
        );
        assert_eq!(app.status().unwrap().active.name.as_str(), "personal");
        assert!(
            !app.paths()
                .profile_auth_path(&ProfileName::parse("work").unwrap())
                .exists()
        );
        assert!(
            !app.paths()
                .profile_dir(&ProfileName::parse("work").unwrap())
                .exists()
        );
    }

    #[test]
    fn add_derives_the_profile_name_after_successful_login() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let personal_auth = auth("personal@example.com", 1);
        let work_auth = auth("litesky+codex@example.com", 1);
        let app = App::with_all_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
            Box::new(FakeLogin(work_auth.clone())),
        );
        write_live_auth(&app, &personal_auth);
        app.init().unwrap();

        let name = app.add(false).unwrap();

        assert_eq!(name.as_str(), "litesky-codex");
        assert_eq!(
            fs::read(app.paths().profile_auth_path(&name)).unwrap(),
            work_auth
        );
        assert_eq!(
            fs::read(app.paths().live_auth_path()).unwrap(),
            personal_auth
        );
    }

    #[test]
    fn add_rejects_an_existing_derived_profile_without_changing_the_active_account() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let personal_auth = auth("personal@example.com", 1);
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        );
        write_live_auth(&app, &personal_auth);
        app.init().unwrap();
        let app = App::with_all_components(
            app.paths().clone(),
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
            Box::new(FakeLogin(auth("personal@example.com", 2))),
        );

        assert!(
            matches!(app.add(false), Err(HandoffError::ProfileExists(name)) if name == "personal")
        );
        assert_eq!(
            fs::read(app.paths().live_auth_path()).unwrap(),
            personal_auth
        );
        assert_eq!(app.status().unwrap().active.name.as_str(), "personal");
    }

    #[cfg(unix)]
    #[test]
    fn switch_rejects_a_profile_auth_file_with_insecure_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (_temporary, app) = app();
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        let work_metadata = app.new_metadata(work.clone(), &work_auth).unwrap();
        app.save_profile(&work_metadata, &work_auth).unwrap();
        fs::set_permissions(
            app.paths().profile_auth_path(&work),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        assert!(matches!(
            app.switch(work, false),
            Err(super::HandoffError::InsecurePermissions(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn live_auth_with_insecure_permissions_is_rejected_and_reported_by_doctor() {
        use std::os::unix::fs::PermissionsExt;

        let (_temporary, app) = app();
        write_live_auth(&app, &auth("personal@example.com", 1));
        fs::set_permissions(
            app.paths().live_auth_path(),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        assert!(matches!(
            app.init(),
            Err(super::HandoffError::InsecurePermissions(_))
        ));
        assert!(
            app.doctor()
                .iter()
                .any(|item| item == "live auth: missing, invalid, or unreadable")
        );
    }

    #[cfg(unix)]
    #[test]
    fn switch_rejects_a_profile_directory_with_insecure_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (_temporary, app) = app();
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        let work_metadata = app.new_metadata(work.clone(), &work_auth).unwrap();
        app.save_profile(&work_metadata, &work_auth).unwrap();
        fs::set_permissions(
            app.paths().profile_dir(&work),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        assert!(matches!(
            app.switch(work, false),
            Err(super::HandoffError::InsecurePermissions(_))
        ));
    }

    #[test]
    fn relogin_updates_the_email_but_preserves_profile_creation_time() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let personal_auth = auth("personal@example.com", 1);
        let replacement_auth = auth("renamed@example.com", 1);
        let app = App::with_all_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
            Box::new(FakeLogin(replacement_auth.clone())),
        );
        write_live_auth(&app, &personal_auth);
        app.init().unwrap();
        let created_at = app.status().unwrap().active.created_at;

        app.relogin(ProfileName::parse("personal").unwrap(), false)
            .unwrap();

        let profile = app.status().unwrap().active;
        assert_eq!(profile.email, "renamed@example.com");
        assert_eq!(profile.created_at, created_at);
        assert_eq!(
            fs::read(app.paths().live_auth_path()).unwrap(),
            personal_auth
        );
        assert_eq!(
            fs::read(
                app.paths()
                    .profile_auth_path(&ProfileName::parse("personal").unwrap())
            )
            .unwrap(),
            replacement_auth
        );
    }

    #[cfg(unix)]
    #[test]
    fn app_server_probe_requires_a_chatgpt_account_and_returns_refreshed_auth() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("fake-codex");
        fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"id":2'*) printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt","email":"work@example.com","planType":"plus"},"requiresOpenaiAuth":true}}'; exit 0 ;;
  esac
done
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let auth = auth("work@example.com", 1);

        let refreshed = AppServerProbe::from_path(script).probe(&auth).unwrap();

        assert_eq!(refreshed, auth);
    }

    #[cfg(unix)]
    #[test]
    fn app_server_usage_reader_parses_all_limit_buckets_without_exposing_credit_ids() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("fake-codex");
        fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"id":2,"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}' ;;
    *'"id":3,"method":"account/rateLimits/read"'*) printf '%s\n' '{"id":3,"result":{"rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":25,"resetsAt":1700000000,"windowDurationMins":300},"secondary":null,"rateLimitReachedType":null,"spendControlReached":false},"other":{"primary":null,"secondary":{"usedPercent":80},"rateLimitReachedType":"rate_limit_reached","spendControlReached":true}},"rateLimitResetCredits":{"availableCount":1,"credits":[{"id":"opaque-credit-id","title":"Reset","description":"One reset","status":"available"}]}}}'; exit 0 ;;
  esac
done
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let report = AppServerUsageReader::from_path(script)
            .read(&auth("work@example.com", 1))
            .unwrap();

        assert_eq!(report.buckets.len(), 2);
        assert_eq!(report.buckets[0].id, "codex");
        assert_eq!(report.buckets[0].primary.as_ref().unwrap().used_percent, 25);
        assert_eq!(report.buckets[1].id, "other");
        assert_eq!(
            report.buckets[1].secondary.as_ref().unwrap().used_percent,
            80
        );
        let credits = report.reset_credits.unwrap();
        assert_eq!(credits.available_count, 1);
        assert_eq!(credits.credits[0].title.as_deref(), Some("Reset"));
        assert_eq!(credits.credits[0].status, "available");
    }

    #[cfg(unix)]
    #[test]
    fn app_server_usage_reader_keeps_remote_error_details_private() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("fake-codex");
        fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"id":2,"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}' ;;
    *'"id":3,"method":"account/rateLimits/read"'*) printf '%s\n' '{"id":3,"error":{"code":-32001,"message":"super-secret-token"}}'; exit 0 ;;
  esac
done
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let error = AppServerUsageReader::from_path(script)
            .read(&auth("work@example.com", 1))
            .unwrap_err()
            .to_string();

        assert!(error.contains("JSON-RPC code -32001"));
        assert!(!error.contains("super-secret-token"));
    }
}

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;

mod activity;
mod app_server;
mod profile_inventory;

use activity::ActivityLease;
use app_server::{AppServerSession, Operation as AppServerOperation};
use profile_inventory::InventoryEntry;

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
    #[error("profile `{0}` is currently in use")]
    ProfileBusy(String),
    #[error("profile `{0}` changed while authentication was being refreshed; retry the operation")]
    ProfileChanged(String),
    #[error("concurrency must be between 1 and 16")]
    InvalidConcurrency,
    #[error("authentication preflight failed: {0}")]
    Preflight(String),
    #[error("usage query failed: {0}")]
    Usage(String),
    #[error("hi command failed: {0}")]
    Hi(String),
    #[error("could not start Codex: {0}")]
    Run(String),
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
    fn runtime_initialized_path(&self, name: &ProfileName) -> PathBuf {
        self.profile_dir(name).join(".ch-runtime-initialized")
    }
    fn state_path(&self) -> PathBuf {
        self.handoff_home.join("state.json")
    }
    fn lock_path(&self) -> PathBuf {
        self.handoff_home.join("lock")
    }
    fn sessions_dir(&self) -> PathBuf {
        self.handoff_home.join("sessions")
    }
    fn runtime_locks_dir(&self) -> PathBuf {
        self.handoff_home.join("runtime-locks")
    }
    fn runtime_lock_path(&self, name: &ProfileName) -> PathBuf {
        self.runtime_locks_dir()
            .join(format!("{}.lock", name.as_str()))
    }
    fn session_path(&self, pid: u32) -> PathBuf {
        self.sessions_dir().join(format!("{pid}.json"))
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

#[derive(Serialize, Deserialize)]
struct RunSession {
    profile: ProfileName,
    pid: u32,
    #[serde(default)]
    uses_live_home: bool,
    #[serde(default)]
    started_at: Option<String>,
}

struct SwitchClientScope {
    default_clients: Vec<ClientProcess>,
    target_sessions: Vec<ClientProcess>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub active: ProfileMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageWindow {
    pub used_percent: u8,
    pub resets_at: Option<i64>,
    pub window_duration_mins: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageBucket {
    pub id: String,
    pub primary: Option<UsageWindow>,
    pub secondary: Option<UsageWindow>,
    pub reached_type: Option<String>,
    pub spend_control_reached: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResetCredit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResetCredits {
    pub available_count: u64,
    pub credits: Vec<ResetCredit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageReport {
    pub buckets: Vec<UsageBucket>,
    pub reset_credits: Option<ResetCredits>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum UsageStatus {
    Available(UsageReport),
    Unavailable(String),
    NotQueried,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum LocalHealth {
    Healthy,
    Unhealthy(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Pass,
    Warning,
    Fail,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub label: String,
    pub status: DoctorStatus,
    pub message: String,
}

impl DoctorCheck {
    fn line(&self) -> String {
        format!("{}: {}", self.label, self.message)
    }
}

impl fmt::Display for LocalHealth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => formatter.write_str("ok"),
            Self::Unhealthy(reason) => write!(formatter, "error: {reason}"),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfileListEntry {
    pub name: ProfileName,
    pub metadata: Option<ProfileMetadata>,
    pub active: bool,
    pub health: LocalHealth,
    pub usage: UsageStatus,
}

#[derive(Clone, Debug, Serialize)]
pub struct HiProfileResult {
    pub name: ProfileName,
    pub email: String,
    pub active: bool,
    pub reply: Result<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BestEvaluation {
    pub profile: ProfileName,
    pub eligible: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BestRecommendation {
    pub profile: Option<ProfileName>,
    pub evaluations: Vec<BestEvaluation>,
}

pub trait AuthProbe: Send + Sync {
    fn probe(&self, auth: &[u8]) -> Result<Vec<u8>, HandoffError>;

    fn check_compatibility(&self) -> Result<(), HandoffError> {
        Ok(())
    }
}

pub trait UsageReader: Send + Sync {
    fn read(&self, auth: &[u8]) -> Result<(UsageReport, Vec<u8>), HandoffError>;
}

pub struct UnavailableUsageReader;

impl UsageReader for UnavailableUsageReader {
    fn read(&self, _auth: &[u8]) -> Result<(UsageReport, Vec<u8>), HandoffError> {
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

    fn running_clients(&self) -> Result<Vec<ClientProcess>, HandoffError> {
        self.ensure_stopped(false)?;
        Ok(Vec::new())
    }

    fn close_clients(&self, clients: &[ClientProcess]) -> Result<(), HandoffError> {
        if clients.is_empty() {
            Ok(())
        } else {
            Err(HandoffError::ClientShutdownFailed(
                ClientProcess::display_list(clients),
            ))
        }
    }

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
pub struct ClientProcess {
    pub name: &'static str,
    pub pid: u32,
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
        self.close_clients(&clients)
    }

    fn running_clients(&self) -> Result<Vec<ClientProcess>, HandoffError> {
        SystemProcessGuard::running_clients(self)
    }

    fn close_clients(&self, clients: &[ClientProcess]) -> Result<(), HandoffError> {
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
            if clients.iter().all(|client| !process_exists(client.pid)) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
        let remaining = clients
            .iter()
            .filter(|client| process_exists(client.pid))
            .cloned()
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            Ok(())
        } else {
            Err(HandoffError::ClientShutdownTimeout(
                ClientProcess::display_list(&remaining),
            ))
        }
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

fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn process_start_time(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
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
        parse_auth(auth)?;
        let mut session = AppServerSession::start(
            &self.codex_binary,
            auth,
            AppServerOperation::Preflight,
            Duration::from_secs(45),
        )?;
        session.initialize()?;
        let message = session.request(
            2,
            serde_json::json!({
                "method":"account/read",
                "id":2,
                "params":{"refreshToken":true}
            }),
        )?;
        if message.get("error").is_some() {
            return Err(HandoffError::Preflight(
                "Codex rejected the authentication refresh".into(),
            ));
        }
        let account = message
            .pointer("/result/account")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                HandoffError::Preflight("Codex reported no authenticated ChatGPT account".into())
            })?;
        if account.get("type").and_then(serde_json::Value::as_str) != Some("chatgpt") {
            return Err(HandoffError::Preflight(
                "Codex did not verify ChatGPT authentication".into(),
            ));
        }
        let updated_auth = session.read_auth()?;
        parse_auth(&updated_auth)?;
        Ok(updated_auth)
    }

    fn check_compatibility(&self) -> Result<(), HandoffError> {
        let mut session = AppServerSession::start(
            &self.codex_binary,
            b"{}",
            AppServerOperation::Preflight,
            Duration::from_secs(5),
        )?;
        session.initialize()
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
    fn read(&self, auth: &[u8]) -> Result<(UsageReport, Vec<u8>), HandoffError> {
        parse_auth(auth)?;
        let mut session = AppServerSession::start(
            &self.codex_binary,
            auth,
            AppServerOperation::Usage,
            Duration::from_secs(45),
        )?;
        session.initialize()?;
        let account = session.request(
            2,
            serde_json::json!({
                "method":"account/read",
                "id":2,
                "params":{"refreshToken":true}
            }),
        )?;
        if account.get("error").is_some() {
            return Err(usage_rpc_error(
                "Codex rejected the authentication refresh",
                &account,
            ));
        }
        parse_auth(&session.read_auth()?)?;
        let response = session.request(
            3,
            serde_json::json!({"method":"account/rateLimits/read","id":3}),
        )?;
        if response.get("error").is_some() {
            return Err(usage_rpc_error(
                "Codex rejected the usage request",
                &response,
            ));
        }
        let report = parse_usage_report(
            response
                .get("result")
                .ok_or_else(|| HandoffError::Usage("Codex returned no usage result".into()))?,
        )?;
        let updated_auth = session.read_auth()?;
        parse_auth(&updated_auth)?;
        Ok((report, updated_auth))
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

pub trait HiRunner: Send + Sync {
    fn send_hi(&self, auth: &[u8], prompt: &str) -> Result<(String, Vec<u8>), HandoffError>;

    fn send_hi_with_timeout(
        &self,
        auth: &[u8],
        prompt: &str,
        _timeout: Duration,
    ) -> Result<(String, Vec<u8>), HandoffError> {
        self.send_hi(auth, prompt)
    }
}

pub struct UnavailableHiRunner;

impl HiRunner for UnavailableHiRunner {
    fn send_hi(&self, _auth: &[u8], _prompt: &str) -> Result<(String, Vec<u8>), HandoffError> {
        Err(HandoffError::Hi("hi runner is unavailable".into()))
    }
}

pub struct CodexExecHiRunner {
    codex_binary: PathBuf,
}

impl CodexExecHiRunner {
    pub fn from_path(codex_binary: impl Into<PathBuf>) -> Self {
        Self {
            codex_binary: codex_binary.into(),
        }
    }
}

impl HiRunner for CodexExecHiRunner {
    fn send_hi(&self, auth: &[u8], prompt: &str) -> Result<(String, Vec<u8>), HandoffError> {
        self.send_hi_with_timeout(auth, prompt, Duration::from_secs(120))
    }

    fn send_hi_with_timeout(
        &self,
        auth: &[u8],
        prompt: &str,
        timeout: Duration,
    ) -> Result<(String, Vec<u8>), HandoffError> {
        parse_auth(auth)?;
        let temporary = tempfile::tempdir()?;
        let auth_path = temporary.path().join("auth.json");
        fs::write(&auth_path, auth)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600))?;
        }

        let mut stdout_file = tempfile::tempfile()?;
        let mut stderr_file = tempfile::tempfile()?;
        let mut child = Command::new(&self.codex_binary)
            .args([
                "exec",
                "--ephemeral",
                "--skip-git-repo-check",
                "--color",
                "never",
                "--sandbox",
                "read-only",
                prompt,
            ])
            .env("CODEX_HOME", temporary.path())
            .current_dir(temporary.path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout_file.try_clone()?))
            .stderr(Stdio::from(stderr_file.try_clone()?))
            .spawn()
            .map_err(|error| HandoffError::Hi(format!("could not start Codex exec: {error}")))?;
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HandoffError::Hi(format!(
                    "Codex exec timed out after {} seconds",
                    timeout.as_secs()
                )));
            }
            thread::sleep(Duration::from_millis(25));
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        stdout_file.rewind()?;
        stderr_file.rewind()?;
        stdout_file.read_to_end(&mut stdout)?;
        stderr_file.read_to_end(&mut stderr)?;

        let updated_auth = fs::read(&auth_path).unwrap_or_else(|_| auth.to_vec());
        let updated_auth = if parse_auth(&updated_auth).is_ok() {
            updated_auth
        } else {
            auth.to_vec()
        };

        if status.success() {
            let stdout = String::from_utf8_lossy(&stdout).trim().to_string();
            let msg = if stdout.is_empty() {
                "(empty response)".to_string()
            } else {
                stdout
            };
            Ok((msg, updated_auth))
        } else {
            let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&stdout).trim().to_string();
            let details = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("exit code {:?}", status.code())
            };
            Err(HandoffError::Hi(details))
        }
    }
}

pub trait CodexRunner: Send + Sync {
    fn spawn(
        &self,
        codex_home: &Path,
        args: &[OsString],
    ) -> Result<std::process::Child, HandoffError>;
}

pub struct SystemCodexRunner {
    codex_binary: PathBuf,
}

impl SystemCodexRunner {
    pub fn from_path(codex_binary: impl Into<PathBuf>) -> Self {
        Self {
            codex_binary: codex_binary.into(),
        }
    }
}

impl CodexRunner for SystemCodexRunner {
    fn spawn(
        &self,
        codex_home: &Path,
        args: &[OsString],
    ) -> Result<std::process::Child, HandoffError> {
        Command::new(&self.codex_binary)
            .args(args)
            .env("CODEX_HOME", codex_home)
            .spawn()
            .map_err(|error| HandoffError::Run(error.to_string()))
    }
}

pub struct UnavailableCodexRunner;

impl CodexRunner for UnavailableCodexRunner {
    fn spawn(
        &self,
        _codex_home: &Path,
        _args: &[OsString],
    ) -> Result<std::process::Child, HandoffError> {
        Err(HandoffError::Run("Codex runner is unavailable".into()))
    }
}

pub struct App {
    paths: AppPaths,
    probe: Box<dyn AuthProbe>,
    process_guard: Box<dyn ProcessGuard>,
    login_runner: Box<dyn LoginRunner>,
    usage_reader: Box<dyn UsageReader>,
    hi_runner: Box<dyn HiRunner>,
    codex_runner: Box<dyn CodexRunner>,
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
            hi_runner: Box::new(UnavailableHiRunner),
            codex_runner: Box::new(UnavailableCodexRunner),
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
            hi_runner: Box::new(UnavailableHiRunner),
            codex_runner: Box::new(UnavailableCodexRunner),
        }
    }

    pub fn with_usage_reader(mut self, usage_reader: Box<dyn UsageReader>) -> Self {
        self.usage_reader = usage_reader;
        self
    }

    pub fn with_hi_runner(mut self, hi_runner: Box<dyn HiRunner>) -> Self {
        self.hi_runner = hi_runner;
        self
    }

    pub fn with_codex_runner(mut self, codex_runner: Box<dyn CodexRunner>) -> Self {
        self.codex_runner = codex_runner;
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

    pub fn run_profile(
        &self,
        name: &ProfileName,
        args: &[OsString],
    ) -> Result<ExitStatus, HandoffError> {
        let (mut child, runtime_lease) = {
            let _lock = self.lock()?;
            let state = self.load_state()?;
            let profile = self.load_profile(name)?;
            let is_active = state.active_profile == *name;
            let auth = if is_active {
                self.read_live_auth()?
            } else {
                self.read_profile_auth(name)?
            };
            self.ensure_profile_email(&profile, &auth)?;
            let (codex_home, runtime_lease) = if is_active {
                (self.paths.codex_home().to_path_buf(), None)
            } else {
                self.bootstrap_profile_runtime(name)?;
                (
                    self.paths.profile_dir(name),
                    Some(self.acquire_runtime_lock_shared(name)?),
                )
            };
            let mut child = self.codex_runner.spawn(&codex_home, args)?;
            if let Err(error) = self.register_session(name, child.id(), is_active) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            (child, runtime_lease)
        };
        let status = child.wait();
        drop(runtime_lease);
        let _ = self.remove_session(child.id());
        Ok(status?)
    }

    pub fn current_live_usage(&self) -> Result<UsageStatus, HandoffError> {
        let status = self.status()?;
        if self.default_clients_are_running(&status.active.name)? {
            return Ok(UsageStatus::Unavailable(
                "active Codex or ChatGPT client is running; usage was not refreshed".into(),
            ));
        }
        let auth = self.read_live_auth()?;
        self.ensure_profile_email(&status.active, &auth)?;
        match self.usage_reader.read(&auth) {
            Ok((usage_report, updated_auth)) => {
                self.persist_refreshed_auth(&status.active, &auth, &updated_auth, true)?;
                Ok(UsageStatus::Available(usage_report))
            }
            Err(error) => Ok(UsageStatus::Unavailable(error.to_string())),
        }
    }

    pub fn list(&self) -> Result<Vec<ProfileListEntry>, HandoffError> {
        self.list_with_concurrency(true, 4)
    }

    fn list_with_usage(&self, include_usage: bool) -> Result<Vec<ProfileListEntry>, HandoffError> {
        self.list_with_concurrency(include_usage, 4)
    }

    pub fn list_offline(&self) -> Result<Vec<ProfileListEntry>, HandoffError> {
        self.list_with_concurrency(false, 1)
    }

    pub fn list_with_concurrency(
        &self,
        include_usage: bool,
        concurrency: usize,
    ) -> Result<Vec<ProfileListEntry>, HandoffError> {
        if !(1..=16).contains(&concurrency) {
            return Err(HandoffError::InvalidConcurrency);
        }
        let inventory = self.profile_inventory()?;
        if !include_usage {
            return Ok(inventory
                .into_iter()
                .map(|entry| ProfileListEntry {
                    name: entry.name,
                    metadata: entry.metadata,
                    active: entry.active,
                    health: entry.health,
                    usage: UsageStatus::NotQueried,
                })
                .collect());
        }

        let queue = std::sync::Mutex::new(inventory.into_iter());
        let results = std::sync::Mutex::new(Vec::new());
        thread::scope(|scope| {
            for _ in 0..concurrency {
                scope.spawn(|| {
                    loop {
                        let Some(entry) = queue.lock().expect("inventory lock poisoned").next()
                        else {
                            break;
                        };
                        let usage = match (&entry.metadata, &entry.health) {
                            (Some(metadata), LocalHealth::Healthy) => {
                                self.profile_usage(&entry.name, metadata, entry.active)
                            }
                            _ => UsageStatus::Unavailable("local profile is unhealthy".into()),
                        };
                        results.lock().expect("profile result lock poisoned").push(
                            ProfileListEntry {
                                name: entry.name,
                                metadata: entry.metadata,
                                active: entry.active,
                                health: entry.health,
                                usage,
                            },
                        );
                    }
                });
            }
        });
        let mut profiles = results.into_inner().expect("profile result lock poisoned");
        profiles.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
        Ok(profiles)
    }

    pub fn best(&self, concurrency: usize) -> Result<BestRecommendation, HandoffError> {
        Ok(best_from_entries(
            &self.list_with_concurrency(true, concurrency)?,
        ))
    }

    fn profile_inventory(&self) -> Result<Vec<InventoryEntry>, HandoffError> {
        let active = self.load_state().ok().map(|state| state.active_profile);
        profile_inventory::scan(&self.paths.profiles_dir(), active.as_ref(), |name| {
            let (metadata, health) = match self.load_profile(name) {
                Ok(metadata) => match self.read_profile_auth(name) {
                    Ok(auth) => match self.ensure_profile_email(&metadata, &auth) {
                        Ok(()) => (Some(metadata), LocalHealth::Healthy),
                        Err(error) => (Some(metadata), LocalHealth::Unhealthy(error.to_string())),
                    },
                    Err(error) => (Some(metadata), LocalHealth::Unhealthy(error.to_string())),
                },
                Err(error) => (None, LocalHealth::Unhealthy(error.to_string())),
            };
            (metadata, health)
        })
    }

    pub fn hi(&self, prompt: &str) -> Result<Vec<HiProfileResult>, HandoffError> {
        self.hi_with_concurrency(prompt, 4)
    }

    pub fn hi_with_concurrency(
        &self,
        prompt: &str,
        concurrency: usize,
    ) -> Result<Vec<HiProfileResult>, HandoffError> {
        self.hi_with_options(prompt, concurrency, Duration::from_secs(120))
    }

    pub fn hi_with_options(
        &self,
        prompt: &str,
        concurrency: usize,
        timeout: Duration,
    ) -> Result<Vec<HiProfileResult>, HandoffError> {
        if !(1..=16).contains(&concurrency) {
            return Err(HandoffError::InvalidConcurrency);
        }
        let queue = std::sync::Mutex::new(self.profile_inventory()?.into_iter());
        let results = std::sync::Mutex::new(Vec::new());
        thread::scope(|scope| {
            for _ in 0..concurrency {
                scope.spawn(|| {
                    loop {
                        let Some(entry) = queue.lock().expect("inventory lock poisoned").next()
                        else {
                            break;
                        };
                        results
                            .lock()
                            .expect("hi result lock poisoned")
                            .push(self.hi_profile(entry, prompt, timeout));
                    }
                });
            }
        });
        let mut results = results.into_inner().expect("hi result lock poisoned");
        results.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
        Ok(results)
    }

    fn hi_profile(
        &self,
        entry: InventoryEntry,
        prompt: &str,
        timeout: Duration,
    ) -> HiProfileResult {
        let InventoryEntry {
            name,
            metadata,
            active: is_active,
            health,
        } = entry;
        let email = metadata
            .as_ref()
            .map(|meta| meta.email.clone())
            .unwrap_or_else(|| "<unknown>".to_string());

        let reply = match (&metadata, &health) {
            (Some(metadata), LocalHealth::Healthy) => {
                if is_active {
                    match self.default_clients_are_running(&name) {
                        Ok(true) => {
                            return HiProfileResult {
                                name,
                                email,
                                active: is_active,
                                reply: Err(
                                    "active Codex or ChatGPT client is running; prompt was not sent"
                                        .into(),
                                ),
                            };
                        }
                        Err(error) => {
                            return HiProfileResult {
                                name,
                                email,
                                active: is_active,
                                reply: Err(error.to_string()),
                            };
                        }
                        Ok(false) => {}
                    }
                }
                let runtime_lock = if is_active {
                    None
                } else {
                    match self.acquire_runtime_lock_exclusive(&name) {
                        Ok(lock) => Some(lock),
                        Err(HandoffError::ProfileBusy(_)) => {
                            return HiProfileResult {
                                name,
                                email,
                                active: is_active,
                                reply: Err(
                                    "profile is currently in use; prompt was not sent".into()
                                ),
                            };
                        }
                        Err(error) => {
                            return HiProfileResult {
                                name,
                                email,
                                active: is_active,
                                reply: Err(error.to_string()),
                            };
                        }
                    }
                };
                let auth = if is_active {
                    self.read_live_auth()
                } else {
                    self.read_profile_auth(&name)
                };
                let reply = match auth {
                    Ok(auth) => match self.ensure_profile_email(metadata, &auth) {
                        Ok(()) => match self.hi_runner.send_hi_with_timeout(&auth, prompt, timeout)
                        {
                            Ok((msg, updated_auth)) => self
                                .persist_refreshed_auth(metadata, &auth, &updated_auth, is_active)
                                .map(|()| msg)
                                .map_err(|error| error.to_string()),
                            Err(error) => Err(error.to_string()),
                        },
                        Err(error) => Err(error.to_string()),
                    },
                    Err(error) => Err(error.to_string()),
                };
                drop(runtime_lock);
                reply
            }
            (_, LocalHealth::Unhealthy(reason)) => Err(format!("unhealthy profile: {reason}")),
            _ => Err("profile is unavailable".into()),
        };

        HiProfileResult {
            name,
            email,
            active: is_active,
            reply,
        }
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
        let state = self.load_state()?;
        let mut client_scope = self.switch_client_scope(&name)?;
        if close_clients && !client_scope.default_clients.is_empty() {
            self.process_guard
                .close_clients(&client_scope.default_clients)?;
            client_scope = self.switch_client_scope(&name)?;
        }
        if !client_scope.default_clients.is_empty() {
            return Err(HandoffError::ClientRunning);
        }
        if !client_scope.target_sessions.is_empty() {
            return Err(HandoffError::ProfileBusy(name.as_str().into()));
        }
        let _target_runtime_lock = (state.active_profile != name)
            .then(|| self.acquire_runtime_lock_exclusive(&name))
            .transpose()?;
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

    pub fn add_named(&self, name: ProfileName, force: bool) -> Result<ProfileName, HandoffError> {
        self.login_profile(Some(name), force, false)
    }

    pub fn relogin(&self, name: ProfileName, force: bool) -> Result<(), HandoffError> {
        self.login_profile(Some(name), force, true).map(|_| ())
    }

    pub fn rename_profile(
        &self,
        current: &ProfileName,
        new_name: ProfileName,
    ) -> Result<(), HandoffError> {
        let _lock = self.lock()?;
        if current == &new_name {
            self.load_profile(current)?;
            return Ok(());
        }
        if self.paths.profile_dir(&new_name).exists() {
            return Err(HandoffError::ProfileExists(new_name.as_str().into()));
        }
        let state = self.load_state()?;
        if state.active_profile == *current && self.default_clients_are_running(current)? {
            return Err(HandoffError::ProfileBusy(current.as_str().into()));
        }
        let _runtime_lease = self.acquire_runtime_lock_exclusive(current)?;
        let mut profile = self.load_profile(current)?;
        let original_metadata = fs::read(self.paths.profile_metadata_path(current))?;
        let original_state = fs::read(self.paths.state_path())?;
        let old_directory = self.paths.profile_dir(current);
        let new_directory = self.paths.profile_dir(&new_name);
        fs::rename(&old_directory, &new_directory)?;
        profile.name = new_name.clone();
        let updated_state = State {
            schema_version: SCHEMA_VERSION,
            active_profile: if state.active_profile == *current {
                new_name.clone()
            } else {
                state.active_profile
            },
        };
        let result = self
            .save_metadata(&profile)
            .and_then(|()| self.save_state(&updated_state));
        if let Err(operation_error) = result {
            let rollback = (|| {
                fs::rename(&new_directory, &old_directory)?;
                self.atomic_write(
                    &self.paths.profile_metadata_path(current),
                    &original_metadata,
                )?;
                self.atomic_write(&self.paths.state_path(), &original_state)
            })();
            return match rollback {
                Ok(()) => Err(operation_error),
                Err(rollback_error) => Err(HandoffError::Rollback {
                    operation: operation_error.to_string(),
                    rollback: rollback_error.to_string(),
                }),
            };
        }
        Ok(())
    }

    pub fn remove_profile(&self, name: &ProfileName) -> Result<(), HandoffError> {
        let _lock = self.lock()?;
        let state = self.load_state()?;
        if state.active_profile == *name {
            return Err(HandoffError::ProfileBusy(name.as_str().into()));
        }
        let _runtime_lease = self.acquire_runtime_lock_exclusive(name)?;
        self.load_profile(name)?;
        fs::remove_dir_all(self.paths.profile_dir(name))?;
        Ok(())
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
        self.doctor_checks()
            .into_iter()
            .map(|check| check.line())
            .collect()
    }

    pub fn doctor_checks(&self) -> Vec<DoctorCheck> {
        let mut items = Vec::new();
        let (status, message) = match self.login_runner.binary_path() {
            Some(binary) => match Command::new(binary).arg("--version").output() {
                Ok(output) if output.status.success() => (
                    DoctorStatus::Pass,
                    format!("ok ({})", String::from_utf8_lossy(&output.stdout).trim()),
                ),
                Ok(_) => (DoctorStatus::Fail, "error (could not read version)".into()),
                Err(error) => (DoctorStatus::Fail, format!("error ({error})")),
            },
            None => (DoctorStatus::Warning, "unavailable".into()),
        };
        items.push(DoctorCheck {
            id: "codex_cli".into(),
            label: "Codex CLI".into(),
            status,
            message,
        });
        let vault_safe = private_path_is_safe(self.paths.handoff_home(), true);
        items.push(DoctorCheck {
            id: "vault".into(),
            label: "vault".into(),
            status: if vault_safe {
                DoctorStatus::Pass
            } else {
                DoctorStatus::Fail
            },
            message: if vault_safe {
                "ok"
            } else {
                "missing or insecure"
            }
            .into(),
        });
        let live_auth_safe = self
            .read_live_auth()
            .and_then(|auth| parse_auth(&auth).map(|_| ()))
            .is_ok();
        items.push(DoctorCheck {
            id: "live_auth".into(),
            label: "live auth".into(),
            status: if live_auth_safe {
                DoctorStatus::Pass
            } else {
                DoctorStatus::Fail
            },
            message: if live_auth_safe {
                "ok"
            } else {
                "missing, invalid, or unreadable"
            }
            .into(),
        });
        let (status, message) = match self.probe.check_compatibility() {
            Ok(()) => (DoctorStatus::Pass, "ok".into()),
            Err(error) => (DoctorStatus::Fail, format!("error ({error})")),
        };
        items.push(DoctorCheck {
            id: "app_server_protocol".into(),
            label: "app-server protocol".into(),
            status,
            message,
        });
        match self.status() {
            Ok(status) => items.push(DoctorCheck {
                id: "active_profile".into(),
                label: "active profile".into(),
                status: DoctorStatus::Pass,
                message: format!("{} ({})", status.active.name.as_str(), status.active.email),
            }),
            Err(error) => items.push(DoctorCheck {
                id: "active_profile".into(),
                label: "active profile".into(),
                status: DoctorStatus::Fail,
                message: format!("error ({error})"),
            }),
        }
        match self.list_with_usage(false) {
            Ok(entries) => {
                for entry in entries {
                    let status = if entry.health == LocalHealth::Healthy {
                        DoctorStatus::Pass
                    } else {
                        DoctorStatus::Fail
                    };
                    items.push(DoctorCheck {
                        id: format!("profile.{}", entry.name.as_str()),
                        label: format!("profile {}", entry.name.as_str()),
                        status,
                        message: entry.health.to_string(),
                    });
                }
            }
            Err(error) => items.push(DoctorCheck {
                id: "profiles".into(),
                label: "profiles".into(),
                status: DoctorStatus::Fail,
                message: format!("error ({error})"),
            }),
        }
        let (status, message) = match self.process_guard.ensure_stopped(false) {
            Ok(()) => (DoctorStatus::Pass, "stopped".into()),
            Err(HandoffError::ClientRunning) => (DoctorStatus::Warning, "running".into()),
            Err(error) => (DoctorStatus::Fail, format!("error ({error})")),
        };
        items.push(DoctorCheck {
            id: "client_processes".into(),
            label: "client processes".into(),
            status,
            message,
        });
        let (status, message) = match self.lock_status() {
            Ok("busy") => (DoctorStatus::Warning, "busy".into()),
            Ok(message) => (DoctorStatus::Pass, message.into()),
            Err(error) => (DoctorStatus::Fail, format!("error ({error})")),
        };
        items.push(DoctorCheck {
            id: "lock".into(),
            label: "lock".into(),
            status,
            message,
        });
        items
    }

    fn ensure_layout(&self) -> Result<(), HandoffError> {
        private_dir(self.paths.handoff_home())?;
        private_dir(&self.paths.profiles_dir())?;
        private_dir(&self.paths.sessions_dir())?;
        private_dir(&self.paths.runtime_locks_dir())
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

    fn bootstrap_profile_runtime(&self, name: &ProfileName) -> Result<(), HandoffError> {
        let marker = self.paths.runtime_initialized_path(name);
        if marker.exists() {
            ensure_private_file_path(&marker)?;
            return Ok(());
        }

        let source = self.paths.codex_home();
        if source.is_dir() {
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let file_name = entry.file_name();
                let Some(file_name) = file_name.to_str() else {
                    continue;
                };
                if file_name != "config.toml" && !file_name.ends_with(".config.toml") {
                    continue;
                }
                let destination = self.paths.profile_dir(name).join(file_name);
                if !destination.exists() {
                    self.atomic_write(&destination, &fs::read(entry.path())?)?;
                }
            }
        }
        self.atomic_write(&marker, b"initialized\n")
    }

    fn acquire_runtime_lock_shared(
        &self,
        name: &ProfileName,
    ) -> Result<ActivityLease, HandoffError> {
        activity::acquire_shared(
            &self.paths.runtime_locks_dir(),
            &self.paths.runtime_lock_path(name),
            name,
        )
    }

    fn acquire_runtime_lock_exclusive(
        &self,
        name: &ProfileName,
    ) -> Result<ActivityLease, HandoffError> {
        activity::acquire_exclusive(
            &self.paths.runtime_locks_dir(),
            &self.paths.runtime_lock_path(name),
            name,
        )
    }

    fn register_session(
        &self,
        profile: &ProfileName,
        pid: u32,
        uses_live_home: bool,
    ) -> Result<(), HandoffError> {
        self.atomic_write(
            &self.paths.session_path(pid),
            &serde_json::to_vec(&RunSession {
                profile: profile.clone(),
                pid,
                uses_live_home,
                started_at: process_start_time(pid),
            })?,
        )
    }

    fn remove_session(&self, pid: u32) -> Result<(), HandoffError> {
        let _lock = self.lock()?;
        let path = self.paths.session_path(pid);
        if path.exists() {
            ensure_private_file_path(&path)?;
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn managed_sessions(&self) -> Result<Vec<RunSession>, HandoffError> {
        let directory = self.paths.sessions_dir();
        let mut sessions = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            ensure_private_file_path(&entry.path())?;
            let session: RunSession = serde_json::from_slice(&fs::read(entry.path())?)?;
            if process_exists(session.pid)
                && session.started_at.as_deref() == process_start_time(session.pid).as_deref()
            {
                sessions.push(session);
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(sessions)
    }

    fn switch_client_scope(&self, target: &ProfileName) -> Result<SwitchClientScope, HandoffError> {
        let sessions = self.managed_sessions()?;
        let mut default_clients = Vec::new();
        let mut target_sessions = Vec::new();
        for client in self.process_guard.running_clients()? {
            match sessions.iter().find(|session| session.pid == client.pid) {
                Some(session) if session.profile == *target && !session.uses_live_home => {
                    target_sessions.push(client)
                }
                Some(session) if !session.uses_live_home => {}
                _ => default_clients.push(client),
            }
        }
        Ok(SwitchClientScope {
            default_clients,
            target_sessions,
        })
    }

    fn default_clients_are_running(&self, active: &ProfileName) -> Result<bool, HandoffError> {
        Ok(!self.switch_client_scope(active)?.default_clients.is_empty())
    }

    fn profile_usage(
        &self,
        name: &ProfileName,
        metadata: &ProfileMetadata,
        is_active: bool,
    ) -> UsageStatus {
        if is_active {
            match self.default_clients_are_running(name) {
                Ok(true) => {
                    return UsageStatus::Unavailable(
                        "active Codex or ChatGPT client is running; usage was not refreshed".into(),
                    );
                }
                Err(error) => return UsageStatus::Unavailable(error.to_string()),
                Ok(false) => {}
            }
        }

        let runtime_lock = if is_active {
            None
        } else {
            match self.acquire_runtime_lock_exclusive(name) {
                Ok(lock) => Some(lock),
                Err(HandoffError::ProfileBusy(_)) => {
                    return UsageStatus::Unavailable(
                        "profile is currently in use; usage was not refreshed".into(),
                    );
                }
                Err(error) => return UsageStatus::Unavailable(error.to_string()),
            }
        };
        let auth = if is_active {
            self.read_live_auth()
        } else {
            self.read_profile_auth(name)
        };
        let result = auth.and_then(|auth| {
            self.ensure_profile_email(metadata, &auth)?;
            self.usage_reader
                .read(&auth)
                .map(|(report, updated_auth)| (report, auth, updated_auth))
        });
        let usage = match result {
            Ok((report, original_auth, updated_auth)) => {
                match self.persist_refreshed_auth(
                    metadata,
                    &original_auth,
                    &updated_auth,
                    is_active,
                ) {
                    Ok(()) => UsageStatus::Available(report),
                    Err(error) => UsageStatus::Unavailable(error.to_string()),
                }
            }
            Err(error) => UsageStatus::Unavailable(error.to_string()),
        };
        drop(runtime_lock);
        usage
    }

    fn save_metadata(&self, profile: &ProfileMetadata) -> Result<(), HandoffError> {
        self.atomic_write(
            &self.paths.profile_metadata_path(&profile.name),
            &serde_json::to_vec_pretty(profile)?,
        )
    }

    fn persist_refreshed_auth(
        &self,
        profile: &ProfileMetadata,
        original_auth: &[u8],
        refreshed_auth: &[u8],
        was_active: bool,
    ) -> Result<(), HandoffError> {
        if refreshed_auth == original_auth {
            return Ok(());
        }
        self.ensure_profile_email(profile, refreshed_auth)?;

        let _lock = self.lock()?;
        let is_active = self.load_state()?.active_profile == profile.name;
        let persisted_auth = if is_active {
            self.read_live_auth()?
        } else {
            self.read_profile_auth(&profile.name)?
        };
        if is_active != was_active || persisted_auth != original_auth {
            return Err(HandoffError::ProfileChanged(profile.name.as_str().into()));
        }

        let mut refreshed_profile = self.load_profile(&profile.name)?;
        self.ensure_profile_email(&refreshed_profile, refreshed_auth)?;
        refreshed_profile.last_synced_at = Utc::now();
        let mut paths = vec![
            self.paths.profile_auth_path(&profile.name),
            self.paths.profile_metadata_path(&profile.name),
        ];
        if is_active {
            paths.push(self.paths.live_auth_path());
        }

        self.transaction(paths, || {
            self.write_auth(&self.paths.profile_auth_path(&profile.name), refreshed_auth)?;
            self.save_metadata(&refreshed_profile)?;
            if is_active {
                self.write_auth(&self.paths.live_auth_path(), refreshed_auth)?;
            }
            Ok(())
        })
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

fn best_from_entries(entries: &[ProfileListEntry]) -> BestRecommendation {
    let mut evaluations = Vec::with_capacity(entries.len());
    let mut candidates = Vec::new();
    for entry in entries {
        let (bucket, reason) = match (&entry.health, &entry.usage) {
            (LocalHealth::Unhealthy(reason), _) => {
                (None, format!("local profile is unhealthy: {reason}"))
            }
            (_, UsageStatus::Unavailable(reason)) => {
                (None, format!("usage is unavailable: {reason}"))
            }
            (_, UsageStatus::NotQueried) => (None, "usage was not queried".into()),
            (LocalHealth::Healthy, UsageStatus::Available(report)) => {
                let bucket = report
                    .buckets
                    .iter()
                    .find(|bucket| bucket.id == "codex")
                    .or_else(|| report.buckets.iter().find(|bucket| bucket.id == "default"))
                    .or_else(|| (report.buckets.len() == 1).then(|| &report.buckets[0]));
                (bucket, "no recognizable primary quota bucket".into())
            }
        };
        let Some(bucket) = bucket else {
            evaluations.push(BestEvaluation {
                profile: entry.name.clone(),
                eligible: false,
                reason,
            });
            continue;
        };
        if bucket.reached_type.is_some() {
            evaluations.push(BestEvaluation {
                profile: entry.name.clone(),
                eligible: false,
                reason: "quota limit has been reached".into(),
            });
            continue;
        }
        if bucket.spend_control_reached == Some(true) {
            evaluations.push(BestEvaluation {
                profile: entry.name.clone(),
                eligible: false,
                reason: "spend control has been reached".into(),
            });
            continue;
        }
        let Some(primary) = bucket.primary.as_ref() else {
            evaluations.push(BestEvaluation {
                profile: entry.name.clone(),
                eligible: false,
                reason: "primary quota window is unavailable".into(),
            });
            continue;
        };
        evaluations.push(BestEvaluation {
            profile: entry.name.clone(),
            eligible: true,
            reason: format!("{} primary {}% used", bucket.id, primary.used_percent),
        });
        candidates.push((
            entry.name.clone(),
            primary.used_percent,
            bucket
                .secondary
                .as_ref()
                .map(|window| window.used_percent)
                .unwrap_or(101),
            primary.resets_at.unwrap_or(i64::MAX),
        ));
    }
    candidates.sort_by(|left, right| {
        (left.1, left.2, left.3, left.0.as_str()).cmp(&(
            right.1,
            right.2,
            right.3,
            right.0.as_str(),
        ))
    });
    BestRecommendation {
        profile: candidates.first().map(|candidate| candidate.0.clone()),
        evaluations,
    }
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
        App, AppPaths, AppServerProbe, AppServerUsageReader, AuthProbe, ClientProcess,
        CodexExecHiRunner, CodexRunner, HandoffError, HiRunner, LocalHealth, LoginRunner,
        NoopProcessGuard, ProcessGuard, ProfileListEntry, ProfileName, StaticProbe, UsageBucket,
        UsageReader, UsageReport, UsageStatus, UsageWindow, ensure_clients_stopped,
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

    type CodexRunCall = (std::path::PathBuf, Vec<std::ffi::OsString>);

    #[derive(Clone)]
    struct RecordingCodexRunner {
        calls: Arc<std::sync::Mutex<Vec<CodexRunCall>>>,
    }

    impl RecordingCodexRunner {
        fn new() -> Self {
            Self {
                calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    impl CodexRunner for RecordingCodexRunner {
        fn spawn(
            &self,
            codex_home: &Path,
            args: &[std::ffi::OsString],
        ) -> Result<std::process::Child, HandoffError> {
            self.calls
                .lock()
                .unwrap()
                .push((codex_home.to_path_buf(), args.to_vec()));
            Ok(std::process::Command::new("true").spawn()?)
        }
    }

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

        fn running_clients(&self) -> Result<Vec<ClientProcess>, HandoffError> {
            if self.closed.load(Ordering::SeqCst) {
                Ok(Vec::new())
            } else {
                Ok(vec![ClientProcess {
                    name: "codex",
                    pid: 424_242,
                }])
            }
        }

        fn close_clients(&self, clients: &[ClientProcess]) -> Result<(), HandoffError> {
            assert_eq!(clients.len(), 1);
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

    struct FakeUsageReader {
        refreshed_auth: Option<Vec<u8>>,
    }

    struct MetadataBlockingUsageReader {
        metadata_path: std::path::PathBuf,
        refreshed_auth: Vec<u8>,
    }

    struct AuthChangingUsageReader {
        auth_paths: Vec<std::path::PathBuf>,
        concurrent_auth: Vec<u8>,
        refreshed_auth: Vec<u8>,
    }

    impl FakeUsageReader {
        fn new() -> Self {
            Self {
                refreshed_auth: None,
            }
        }

        fn with_refreshed_auth(auth: Vec<u8>) -> Self {
            Self {
                refreshed_auth: Some(auth),
            }
        }
    }

    #[derive(Clone)]
    struct CountingUsageReader(Arc<AtomicBool>);

    impl UsageReader for FakeUsageReader {
        fn read(&self, auth: &[u8]) -> Result<(UsageReport, Vec<u8>), HandoffError> {
            let email = super::parse_auth(auth)?.email;
            if self.refreshed_auth.is_none() && email == "work@example.com" {
                return Err(HandoffError::Preflight("quota service unavailable".into()));
            }
            let updated = self.refreshed_auth.clone().unwrap_or_else(|| auth.to_vec());
            Ok((
                UsageReport {
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
                },
                updated,
            ))
        }
    }

    impl UsageReader for MetadataBlockingUsageReader {
        fn read(&self, auth: &[u8]) -> Result<(UsageReport, Vec<u8>), HandoffError> {
            fs::remove_file(&self.metadata_path)?;
            fs::create_dir(&self.metadata_path)?;
            FakeUsageReader::with_refreshed_auth(self.refreshed_auth.clone()).read(auth)
        }
    }

    impl UsageReader for AuthChangingUsageReader {
        fn read(&self, auth: &[u8]) -> Result<(UsageReport, Vec<u8>), HandoffError> {
            for path in &self.auth_paths {
                fs::write(path, &self.concurrent_auth)?;
            }
            FakeUsageReader::with_refreshed_auth(self.refreshed_auth.clone()).read(auth)
        }
    }

    impl UsageReader for CountingUsageReader {
        fn read(&self, _auth: &[u8]) -> Result<(UsageReport, Vec<u8>), HandoffError> {
            self.0.store(true, Ordering::SeqCst);
            Err(HandoffError::Usage("usage lookup should not run".into()))
        }
    }

    struct FakeHiRunner {
        fail_email: Option<String>,
        refreshed_auth: Option<Vec<u8>>,
    }

    struct MetadataBlockingHiRunner {
        metadata_path: std::path::PathBuf,
        refreshed_auth: Vec<u8>,
    }

    impl FakeHiRunner {
        fn success() -> Self {
            Self {
                fail_email: None,
                refreshed_auth: None,
            }
        }

        fn with_failing_email(email: impl Into<String>) -> Self {
            Self {
                fail_email: Some(email.into()),
                refreshed_auth: None,
            }
        }

        fn with_refreshed_auth(auth: Vec<u8>) -> Self {
            Self {
                fail_email: None,
                refreshed_auth: Some(auth),
            }
        }
    }

    impl HiRunner for FakeHiRunner {
        fn send_hi(&self, auth: &[u8], prompt: &str) -> Result<(String, Vec<u8>), HandoffError> {
            let email = super::parse_auth(auth)?.email;
            if self.fail_email.as_deref() == Some(&email) {
                return Err(HandoffError::Hi("failed to execute prompt".into()));
            }
            let updated = self.refreshed_auth.clone().unwrap_or_else(|| auth.to_vec());
            Ok((format!("reply to {prompt} for {email}"), updated))
        }
    }

    impl HiRunner for MetadataBlockingHiRunner {
        fn send_hi(&self, auth: &[u8], prompt: &str) -> Result<(String, Vec<u8>), HandoffError> {
            fs::remove_file(&self.metadata_path)?;
            fs::create_dir(&self.metadata_path)?;
            FakeHiRunner::with_refreshed_auth(self.refreshed_auth.clone()).send_hi(auth, prompt)
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
    fn run_uses_a_non_active_profile_as_a_persistent_codex_home_and_bootstraps_config_once() {
        let (_temporary, app) = app();
        let runner = RecordingCodexRunner::new();
        let calls = runner.calls.clone();
        let app = app.with_codex_runner(Box::new(runner));
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let profile = ProfileName::parse("work").unwrap();
        let profile_auth = auth("work@example.com", 1);
        let profile_metadata = app.new_metadata(profile.clone(), &profile_auth).unwrap();
        app.save_profile(&profile_metadata, &profile_auth).unwrap();
        fs::create_dir_all(app.paths().codex_home()).unwrap();
        fs::write(
            app.paths().codex_home().join("config.toml"),
            "model = \"gpt-5\"\n",
        )
        .unwrap();
        fs::write(
            app.paths().codex_home().join("review.config.toml"),
            "model = \"gpt-5-mini\"\n",
        )
        .unwrap();

        app.run_profile(&profile, &["--no-alt-screen".into()])
            .unwrap();
        fs::write(
            app.paths().codex_home().join("config.toml"),
            "model = \"replacement\"\n",
        )
        .unwrap();
        app.run_profile(&profile, &[]).unwrap();

        let profile_dir = app.paths().profile_dir(&profile);
        assert_eq!(
            fs::read_to_string(profile_dir.join("config.toml")).unwrap(),
            "model = \"gpt-5\"\n"
        );
        assert_eq!(
            fs::read_to_string(profile_dir.join("review.config.toml")).unwrap(),
            "model = \"gpt-5-mini\"\n"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                (profile_dir.clone(), vec!["--no-alt-screen".into()]),
                (profile_dir, vec![]),
            ]
        );
        assert!(
            fs::read_dir(app.paths().sessions_dir())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn run_uses_the_global_home_for_the_active_profile() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let runner = RecordingCodexRunner::new();
        let calls = runner.calls.clone();
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        )
        .with_codex_runner(Box::new(runner));
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();

        app.run_profile(&ProfileName::parse("personal").unwrap(), &[])
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(app.paths().codex_home().to_path_buf(), vec![])]
        );
        assert!(
            !app.paths()
                .runtime_initialized_path(&ProfileName::parse("personal").unwrap())
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_codex_runner_passes_the_profile_home_to_the_child_process() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("profile");
        let script = temporary.path().join("fake-codex");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ \"$CODEX_HOME\" = \"{}\" ] || exit 7\nexit 42\n",
                home.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let runner = super::SystemCodexRunner::from_path(script);
        let status = runner.spawn(&home, &[]).unwrap().wait().unwrap();

        assert_eq!(status.code(), Some(42));
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
    fn best_prefers_lowest_primary_then_secondary_usage() {
        let entry = |name: &str, primary: u8, secondary: Option<u8>| ProfileListEntry {
            name: ProfileName::parse(name).unwrap(),
            metadata: None,
            active: false,
            health: LocalHealth::Healthy,
            usage: UsageStatus::Available(UsageReport {
                buckets: vec![UsageBucket {
                    id: "codex".into(),
                    primary: Some(UsageWindow {
                        used_percent: primary,
                        resets_at: Some(100),
                        window_duration_mins: None,
                    }),
                    secondary: secondary.map(|used_percent| UsageWindow {
                        used_percent,
                        resets_at: None,
                        window_duration_mins: None,
                    }),
                    reached_type: None,
                    spend_control_reached: Some(false),
                }],
                reset_credits: None,
            }),
        };
        let recommendation = super::best_from_entries(&[
            entry("missing-secondary", 10, None),
            entry("higher-primary", 11, Some(0)),
            entry("winner", 10, Some(50)),
        ]);

        assert_eq!(recommendation.profile.unwrap().as_str(), "winner");
    }

    #[test]
    fn rename_profile_updates_metadata_and_preserves_authentication() {
        let (_temporary, app) = app();
        let original_auth = auth("personal@example.com", 1);
        write_live_auth(&app, &original_auth);
        app.init().unwrap();
        let original = ProfileName::parse("personal").unwrap();
        let renamed = ProfileName::parse("home").unwrap();

        app.rename_profile(&original, renamed.clone()).unwrap();

        assert_eq!(app.status().unwrap().active.name, renamed);
        assert_eq!(
            fs::read(app.paths().profile_auth_path(&renamed)).unwrap(),
            original_auth
        );
        assert!(!app.paths().profile_dir(&original).exists());
    }

    #[test]
    fn remove_profile_rejects_active_and_busy_profiles() {
        let (_temporary, app) = app();
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let active = ProfileName::parse("personal").unwrap();
        assert!(matches!(
            app.remove_profile(&active),
            Err(HandoffError::ProfileBusy(name)) if name == "personal"
        ));

        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        app.save_profile(
            &app.new_metadata(work.clone(), &work_auth).unwrap(),
            &work_auth,
        )
        .unwrap();
        let lease = app.acquire_runtime_lock_shared(&work).unwrap();
        assert!(matches!(
            app.remove_profile(&work),
            Err(HandoffError::ProfileBusy(name)) if name == "work"
        ));
        drop(lease);

        app.remove_profile(&work).unwrap();
        assert!(!app.paths().profile_dir(&work).exists());
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
    fn switch_only_blocks_the_target_profile_runtime() {
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
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        app.save_profile(
            &app.new_metadata(work.clone(), &work_auth).unwrap(),
            &work_auth,
        )
        .unwrap();
        let other = ProfileName::parse("other").unwrap();
        let other_auth = auth("other@example.com", 1);
        app.save_profile(
            &app.new_metadata(other.clone(), &other_auth).unwrap(),
            &other_auth,
        )
        .unwrap();

        let target_lease = app.acquire_runtime_lock_shared(&work).unwrap();
        assert!(matches!(
            app.switch(work.clone(), false),
            Err(HandoffError::ProfileBusy(name)) if name == "work"
        ));
        drop(target_lease);

        let unrelated_lease = app.acquire_runtime_lock_shared(&other).unwrap();
        app.switch(work.clone(), false).unwrap();
        drop(unrelated_lease);
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
        .with_usage_reader(Box::new(FakeUsageReader::new()));
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
        .with_usage_reader(Box::new(FakeUsageReader::new()));
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
    fn app_server_compatibility_check_only_initializes_the_protocol() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("fake-codex");
        fs::write(
            &script,
            "#!/bin/sh\nread initialize\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nread initialized\ncase \"$initialized\" in *initialized*) exit 0;; *) exit 9;; esac\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        AppServerProbe::from_path(script)
            .check_compatibility()
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn app_server_usage_reader_accepts_successful_account_reads_without_account_type() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("fake-codex");
        fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"id":2,"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{}}' ;;
    *'"id":3,"method":"account/rateLimits/read"'*) printf '%s\n' '{"id":3,"result":{"rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":25,"resetsAt":1700000000,"windowDurationMins":300},"secondary":null,"rateLimitReachedType":null,"spendControlReached":false},"other":{"primary":null,"secondary":{"usedPercent":80},"rateLimitReachedType":"rate_limit_reached","spendControlReached":true}},"rateLimitResetCredits":{"availableCount":1,"credits":[{"id":"opaque-credit-id","title":"Reset","description":"One reset","status":"available"}]}}}'; exit 0 ;;
  esac
done
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let (report, updated_auth) = AppServerUsageReader::from_path(script)
            .read(&auth("work@example.com", 1))
            .unwrap();

        assert_eq!(updated_auth, auth("work@example.com", 1));
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

    #[test]
    fn hi_executes_prompt_for_all_profiles() {
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
        .with_hi_runner(Box::new(FakeHiRunner::success()));

        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        let work_metadata = app.new_metadata(work.clone(), &work_auth).unwrap();
        app.save_profile(&work_metadata, &work_auth).unwrap();

        let results = app.hi("hi").unwrap();

        assert_eq!(results.len(), 2);
        let personal = results
            .iter()
            .find(|r| r.name.as_str() == "personal")
            .unwrap();
        assert!(personal.active);
        assert_eq!(
            personal.reply.as_deref(),
            Ok("reply to hi for personal@example.com")
        );

        let work_res = results.iter().find(|r| r.name.as_str() == "work").unwrap();
        assert!(!work_res.active);
        assert_eq!(
            work_res.reply.as_deref(),
            Ok("reply to hi for work@example.com")
        );
    }

    #[test]
    fn hi_continues_when_single_profile_fails() {
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
        .with_hi_runner(Box::new(FakeHiRunner::with_failing_email(
            "work@example.com",
        )));

        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        let work_metadata = app.new_metadata(work.clone(), &work_auth).unwrap();
        app.save_profile(&work_metadata, &work_auth).unwrap();

        let results = app.hi("hi").unwrap();

        assert_eq!(results.len(), 2);
        let personal = results
            .iter()
            .find(|r| r.name.as_str() == "personal")
            .unwrap();
        assert!(personal.reply.is_ok());

        let work_res = results.iter().find(|r| r.name.as_str() == "work").unwrap();
        assert!(work_res.reply.is_err());
    }

    #[test]
    fn hi_reports_unhealthy_profile() {
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
        .with_hi_runner(Box::new(FakeHiRunner::success()));

        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let broken = ProfileName::parse("broken").unwrap();
        let broken_dir = app.paths().profile_dir(&broken);
        fs::create_dir_all(&broken_dir).unwrap();
        let broken_auth = app.paths().profile_auth_path(&broken);
        fs::write(&broken_auth, b"invalid json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&broken_auth, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let results = app.hi("hi").unwrap();
        let broken_res = results
            .iter()
            .find(|r| r.name.as_str() == "broken")
            .unwrap();
        assert!(broken_res.reply.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn codex_exec_hi_runner_invokes_codex_exec() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("fake-codex");
        fs::write(
            &script,
            r#"#!/bin/sh
if [ "$1" = "exec" ] && echo "$*" | grep -q "hi$"; then
  printf "hello from model\n"
  exit 0
fi
printf "unexpected args: %s\n" "$*" >&2
exit 1
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let runner = CodexExecHiRunner::from_path(script);
        let (reply, _updated_auth) = runner.send_hi(&auth("work@example.com", 1), "hi").unwrap();

        assert_eq!(reply, "hello from model");
    }

    #[cfg(unix)]
    #[test]
    fn codex_exec_hi_runner_enforces_its_deadline() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("slow-codex");
        fs::write(&script, "#!/bin/sh\nsleep 1\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let error = CodexExecHiRunner::from_path(script)
            .send_hi_with_timeout(
                &auth("slow@example.com", 1),
                "hi",
                std::time::Duration::from_millis(10),
            )
            .unwrap_err();

        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn list_persists_refreshed_active_auth_to_the_profile_and_live_home() {
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
        .with_usage_reader(Box::new(FakeUsageReader::with_refreshed_auth(auth(
            "work@example.com",
            2,
        ))));

        write_live_auth(&app, &auth("work@example.com", 1));
        app.init().unwrap();

        let initial_meta = app
            .load_profile(&ProfileName::parse("work").unwrap())
            .unwrap();

        let profiles = app.list().unwrap();
        assert_eq!(profiles.len(), 1);

        // The active profile uses the default live home, so both copies stay in sync.
        let live = app.read_live_auth().unwrap();
        assert_eq!(live, auth("work@example.com", 2));

        let vault = app
            .read_profile_auth(&ProfileName::parse("work").unwrap())
            .unwrap();
        assert_eq!(vault, auth("work@example.com", 2));

        let updated_meta = app
            .load_profile(&ProfileName::parse("work").unwrap())
            .unwrap();
        assert!(updated_meta.last_synced_at >= initial_meta.last_synced_at);
    }

    #[test]
    fn list_skips_a_non_active_profile_while_it_is_running() {
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
        .with_usage_reader(Box::new(FakeUsageReader::with_refreshed_auth(auth(
            "work@example.com",
            2,
        ))));
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();
        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        app.save_profile(
            &app.new_metadata(work.clone(), &work_auth).unwrap(),
            &work_auth,
        )
        .unwrap();

        let runtime_lease = app.acquire_runtime_lock_shared(&work).unwrap();
        let work_entry = app
            .list()
            .unwrap()
            .into_iter()
            .find(|entry| entry.name == work)
            .unwrap();
        drop(runtime_lease);

        assert!(
            matches!(work_entry.usage, UsageStatus::Unavailable(message) if message.contains("currently in use"))
        );
        assert_eq!(app.read_profile_auth(&work).unwrap(), work_auth);
    }

    #[test]
    fn active_usage_and_hi_do_not_refresh_while_a_default_client_is_running() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let guard = CloseableProcessGuard::new();
        let usage_queried = Arc::new(AtomicBool::new(false));
        let app = App::with_components(paths, Box::new(StaticProbe::success()), Box::new(guard))
            .with_usage_reader(Box::new(CountingUsageReader(usage_queried.clone())))
            .with_hi_runner(Box::new(FakeHiRunner::with_refreshed_auth(auth(
                "personal@example.com",
                2,
            ))));
        let original_auth = auth("personal@example.com", 1);
        write_live_auth(&app, &original_auth);
        app.init().unwrap();

        assert!(matches!(
            app.current_live_usage().unwrap(),
            UsageStatus::Unavailable(message) if message.contains("client is running")
        ));
        assert!(!usage_queried.load(Ordering::SeqCst));
        let result = app.hi("hi").unwrap().pop().unwrap();

        assert!(matches!(result.reply, Err(message) if message.contains("prompt was not sent")));
        assert_eq!(app.read_live_auth().unwrap(), original_auth);
        assert_eq!(
            app.read_profile_auth(&ProfileName::parse("personal").unwrap())
                .unwrap(),
            original_auth
        );
    }

    #[test]
    fn current_live_usage_persists_refreshed_auth_to_vault_and_live() {
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
        .with_usage_reader(Box::new(FakeUsageReader::with_refreshed_auth(auth(
            "personal@example.com",
            2,
        ))));

        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();

        let status_usage = app.current_live_usage().unwrap();
        assert!(matches!(status_usage, UsageStatus::Available(_)));

        // Verify live auth and vault auth were updated to version 2
        let live = app.read_live_auth().unwrap();
        assert_eq!(live, auth("personal@example.com", 2));

        let vault = app
            .read_profile_auth(&ProfileName::parse("personal").unwrap())
            .unwrap();
        assert_eq!(vault, auth("personal@example.com", 2));
    }

    #[test]
    fn current_live_usage_does_not_report_success_when_refreshed_auth_cannot_be_persisted() {
        let temporary = tempfile::tempdir().unwrap();
        let profile = ProfileName::parse("personal").unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let metadata_path = paths.profile_metadata_path(&profile);
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        )
        .with_usage_reader(Box::new(MetadataBlockingUsageReader {
            metadata_path,
            refreshed_auth: auth("personal@example.com", 2),
        }));
        let original_auth = auth("personal@example.com", 1);
        write_live_auth(&app, &original_auth);
        app.init().unwrap();

        let result = app.current_live_usage();

        assert!(result.is_err());
        assert_eq!(
            fs::read(app.paths().live_auth_path()).unwrap(),
            original_auth
        );
        assert_eq!(
            fs::read(app.paths().profile_auth_path(&profile)).unwrap(),
            auth("personal@example.com", 1)
        );
    }

    #[test]
    fn current_live_usage_rejects_refreshed_auth_for_another_account() {
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
        .with_usage_reader(Box::new(FakeUsageReader::with_refreshed_auth(auth(
            "other@example.com",
            2,
        ))));
        let original_auth = auth("personal@example.com", 1);
        write_live_auth(&app, &original_auth);
        app.init().unwrap();

        let result = app.current_live_usage();

        assert!(matches!(result, Err(HandoffError::EmailMismatch { .. })));
        assert_eq!(app.read_live_auth().unwrap(), original_auth);
        assert_eq!(
            app.read_profile_auth(&ProfileName::parse("personal").unwrap())
                .unwrap(),
            auth("personal@example.com", 1)
        );
    }

    #[test]
    fn current_live_usage_does_not_overwrite_auth_changed_during_refresh() {
        let temporary = tempfile::tempdir().unwrap();
        let profile = ProfileName::parse("personal").unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let auth_paths = vec![paths.live_auth_path(), paths.profile_auth_path(&profile)];
        let concurrent_auth = auth("personal@example.com", 3);
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        )
        .with_usage_reader(Box::new(AuthChangingUsageReader {
            auth_paths,
            concurrent_auth: concurrent_auth.clone(),
            refreshed_auth: auth("personal@example.com", 2),
        }));
        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();

        let result = app.current_live_usage();

        assert!(matches!(result, Err(HandoffError::ProfileChanged(_))));
        assert_eq!(app.read_live_auth().unwrap(), concurrent_auth);
        assert_eq!(app.read_profile_auth(&profile).unwrap(), concurrent_auth);
    }

    #[test]
    fn list_marks_usage_unavailable_when_refreshed_auth_cannot_be_persisted() {
        let temporary = tempfile::tempdir().unwrap();
        let profile = ProfileName::parse("personal").unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let metadata_path = paths.profile_metadata_path(&profile);
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        )
        .with_usage_reader(Box::new(MetadataBlockingUsageReader {
            metadata_path,
            refreshed_auth: auth("personal@example.com", 2),
        }));
        let original_auth = auth("personal@example.com", 1);
        write_live_auth(&app, &original_auth);
        app.init().unwrap();

        let profile = app.list().unwrap().pop().unwrap();

        assert!(matches!(profile.usage, UsageStatus::Unavailable(_)));
        assert_eq!(
            fs::read(app.paths().live_auth_path()).unwrap(),
            original_auth
        );
        assert_eq!(
            fs::read(app.paths().profile_auth_path(&profile.name)).unwrap(),
            auth("personal@example.com", 1)
        );
    }

    #[test]
    fn hi_marks_reply_failed_when_refreshed_auth_cannot_be_persisted() {
        let temporary = tempfile::tempdir().unwrap();
        let profile = ProfileName::parse("personal").unwrap();
        let paths = AppPaths::new(
            temporary.path().join("codex"),
            temporary.path().join("vault"),
        );
        let metadata_path = paths.profile_metadata_path(&profile);
        let app = App::with_components(
            paths,
            Box::new(StaticProbe::success()),
            Box::new(NoopProcessGuard),
        )
        .with_hi_runner(Box::new(MetadataBlockingHiRunner {
            metadata_path,
            refreshed_auth: auth("personal@example.com", 2),
        }));
        let original_auth = auth("personal@example.com", 1);
        write_live_auth(&app, &original_auth);
        app.init().unwrap();

        let result = app.hi("hi").unwrap().pop().unwrap();

        assert!(result.reply.is_err());
        assert_eq!(
            fs::read(app.paths().live_auth_path()).unwrap(),
            original_auth
        );
        assert_eq!(
            fs::read(app.paths().profile_auth_path(&profile)).unwrap(),
            auth("personal@example.com", 1)
        );
    }

    #[test]
    fn hi_persists_refreshed_auth_to_vault_and_live() {
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
        .with_hi_runner(Box::new(FakeHiRunner::with_refreshed_auth(auth(
            "work@example.com",
            2,
        ))));

        write_live_auth(&app, &auth("personal@example.com", 1));
        app.init().unwrap();

        let work = ProfileName::parse("work").unwrap();
        let work_auth = auth("work@example.com", 1);
        let work_metadata = app.new_metadata(work.clone(), &work_auth).unwrap();
        app.save_profile(&work_metadata, &work_auth).unwrap();

        let results = app.hi("hi").unwrap();
        assert_eq!(results.len(), 2);

        // Verify work profile in vault was updated to version 2
        let vault = app.read_profile_auth(&work).unwrap();
        assert_eq!(vault, auth("work@example.com", 2));
    }
}

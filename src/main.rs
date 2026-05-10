use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::io::ErrorKind;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use cpal::Sample;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use reqwest::multipart;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

const DEFAULT_CONTEXT: &str = "user recorded voice memo";
const DEFAULT_PROFILE: &str = "default";
const KEYCHAIN_SERVICE: &str = "mnemo-secrets";

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Record your voice, transcribe with ElevenLabs, and retain in Hindsight"
)]
struct CliArgs {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[arg(long, global = true)]
    profile: Option<String>,

    #[arg(long, global = true)]
    hindsight_url: Option<String>,

    #[arg(long, global = true)]
    bank: Option<String>,

    #[arg(long, global = true)]
    language: Option<String>,

    #[arg(long, global = true)]
    model: Option<String>,

    #[arg(long, global = true)]
    elevenlabs_api_key: Option<String>,

    #[arg(long, global = true)]
    hindsight_api_key: Option<String>,

    #[arg(long, global = true)]
    socket_path: Option<PathBuf>,

    #[arg(long, global = true)]
    context: Option<String>,

    #[arg(long, global = true, value_delimiter = ',')]
    tags: Vec<String>,

    #[arg(long, global = true)]
    strategy: Option<String>,

    #[arg(long, global = true, value_name = "KEY=VALUE")]
    metadata: Vec<String>,
}

#[derive(Clone, Debug, Subcommand)]
enum Command {
    /// Create the default configuration file.
    Init {
        /// Overwrite an existing configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Start recording a voice note.
    Record,
    /// Stop the currently running recorder.
    Stop,
    /// Manage mnemo secrets in macOS Keychain.
    Keychain {
        #[command(subcommand)]
        command: KeychainCommand,
    },
}

#[derive(Clone, Debug, Subcommand)]
enum KeychainCommand {
    /// Store configured API keys in macOS Keychain.
    Sync,
    /// List API keys stored in macOS Keychain.
    List,
    /// Remove API keys from macOS Keychain.
    Remove {
        /// Remove all mnemo API keys from Keychain.
        #[arg(long)]
        all: bool,
        /// Skip confirmation prompts.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    profiles: BTreeMap<String, ProfileConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileConfig {
    hindsight_url: Option<String>,
    bank: Option<String>,
    language: Option<String>,
    model: Option<String>,
    elevenlabs_api_key: Option<String>,
    hindsight_api_key: Option<String>,
    socket_path: Option<PathBuf>,
    context: Option<String>,
    metadata: Option<BTreeMap<String, String>>,
    tags: Option<Vec<String>>,
    strategy: Option<String>,
}

#[derive(Debug)]
struct Config {
    profile: String,
    hindsight_url: Option<String>,
    bank: Option<String>,
    language: String,
    model: String,
    cli_elevenlabs_api_key: Option<String>,
    env_elevenlabs_api_key: Option<String>,
    elevenlabs_api_key: Option<String>,
    cli_hindsight_api_key: Option<String>,
    env_hindsight_api_key: Option<String>,
    hindsight_api_key: Option<String>,
    socket_path: PathBuf,
    context: String,
    metadata: Option<BTreeMap<String, String>>,
    tags: Option<Vec<String>>,
    strategy: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ElevenLabsTranscription {
    text: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum RecordingState {
    Recording,
    Processing,
    Complete,
    Error,
}

#[derive(Clone, Debug, Serialize)]
struct RecordingStatus {
    state: RecordingState,
    #[serde(rename = "startedAt")]
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Serialize)]
struct StatusMessage {
    #[serde(rename = "type")]
    message_type: &'static str,
    #[serde(flatten)]
    status: RecordingStatus,
}

#[derive(Serialize)]
struct ErrorMessage {
    #[serde(rename = "type")]
    message_type: &'static str,
    message: String,
}

#[derive(Debug, Deserialize)]
struct SocketCommand {
    #[serde(rename = "type")]
    command_type: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();
    let command = args.command.clone().unwrap_or(Command::Record);
    if let Command::Init { force } = command {
        return init_config(args.config.as_ref(), force);
    }

    let config = Config::load(args)?;
    match &command {
        Command::Record => record(config).await,
        Command::Stop => stop_recording(&config).await,
        Command::Keychain { command } => handle_keychain_command(&config, command),
        Command::Init { .. } => unreachable!(),
    }
}

async fn record(config: Config) -> Result<()> {
    let elevenlabs_api_key = resolve_api_key(&config)?.ok_or_else(missing_api_key_error)?;
    let hindsight_url = config.hindsight_url.clone().ok_or_else(|| {
        anyhow!(
            "MNEMO_HINDSIGHT_API_URL must be set in the environment, profile '{}' in the config file, or --hindsight-url",
            config.profile
        )
    })?;
    let bank = config.bank.clone().ok_or_else(|| {
        anyhow!(
            "MNEMO_BANK_ID must be set in the environment, profile '{}' in the config file, or --bank",
            config.profile
        )
    })?;
    ensure_singleton_socket(&config.socket_path)?;
    let (_socket_guard, listener) = bind_control_socket(config.socket_path.clone()).await?;
    let started_at = now_rfc3339();
    let (status_tx, status_rx) = watch::channel(RecordingStatus {
        state: RecordingState::Recording,
        started_at,
        message: None,
    });
    let (stop_tx, stop_rx) = mpsc::channel();
    let socket_task = tokio::spawn(control_socket_server(listener, stop_tx.clone(), status_rx));
    spawn_enter_stop_thread(stop_tx);

    let result = record_and_retain(
        config,
        elevenlabs_api_key,
        hindsight_url,
        bank,
        stop_rx,
        status_tx.clone(),
    )
    .await;

    if let Err(error) = &result {
        send_recording_status(&status_tx, RecordingState::Error, Some(error.to_string()));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    socket_task.abort();
    result
}

async fn record_and_retain(
    config: Config,
    elevenlabs_api_key: String,
    hindsight_url: String,
    bank: String,
    stop_rx: mpsc::Receiver<()>,
    status_tx: watch::Sender<RecordingStatus>,
) -> Result<()> {
    let started_at = status_tx.borrow().started_at.clone();

    let recording = tokio::task::spawn_blocking(move || record_until_stop(stop_rx))
        .await
        .context("recording task failed")??;
    status_tx.send_replace(RecordingStatus {
        state: RecordingState::Processing,
        started_at: started_at.clone(),
        message: None,
    });
    println!("Recording complete. Sending to ElevenLabs...");

    let wav = wav_bytes(
        &recording.samples,
        recording.sample_rate,
        recording.channels,
    )?;
    let transcript = transcribe(&config, &elevenlabs_api_key, wav).await?;
    let transcript = transcript.trim();

    if transcript.is_empty() {
        println!("No speech detected. Nothing retained in Hindsight.");
        status_tx.send_replace(RecordingStatus {
            state: RecordingState::Complete,
            started_at,
            message: Some("No speech detected".to_string()),
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        return Ok(());
    }

    println!("\nTranscription: {transcript}");
    println!("Retaining transcript in Hindsight bank '{bank}'...");
    retain_in_hindsight(&config, &hindsight_url, &bank, transcript).await?;
    println!("Retained in Hindsight.");
    status_tx.send_replace(RecordingStatus {
        state: RecordingState::Complete,
        started_at,
        message: None,
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(())
}

async fn stop_recording(config: &Config) -> Result<()> {
    let mut stream = UnixStream::connect(&config.socket_path)
        .await
        .with_context(|| {
            format!(
                "failed to connect to socket {}",
                config.socket_path.display()
            )
        })?;
    stream
        .write_all(b"{\"type\":\"stop\"}\n")
        .await
        .context("failed to send stop command")?;
    stream.shutdown().await.ok();

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .context("failed to read stop response")?;
    print!("{response}");
    Ok(())
}

impl Config {
    fn load(args: CliArgs) -> Result<Self> {
        let config_path = args.config.clone().unwrap_or(default_config_path()?);
        let file_config = read_file_config(&config_path)?;
        let profile = first_some([
            args.profile,
            env::var("MNEMO_PROFILE").ok(),
            Some(DEFAULT_PROFILE.to_string()),
        ])
        .expect("default profile is set");
        let profile_config = file_config
            .profiles
            .get(&profile)
            .cloned()
            .unwrap_or_default();

        let hindsight_url = first_some([
            args.hindsight_url,
            env::var("MNEMO_HINDSIGHT_API_URL").ok(),
            profile_config.hindsight_url,
        ]);
        let bank = first_some([
            args.bank,
            env::var("MNEMO_BANK_ID").ok(),
            profile_config.bank,
        ]);
        let language = first_some([
            args.language,
            env::var("MNEMO_ELEVENLABS_LANGUAGE").ok(),
            profile_config.language,
            Some("eng".to_string()),
        ])
        .expect("default language is set");
        let model = first_some([
            args.model,
            env::var("MNEMO_ELEVENLABS_MODEL").ok(),
            profile_config.model,
            Some("scribe_v2".to_string()),
        ])
        .expect("default model is set");
        let cli_elevenlabs_api_key = args.elevenlabs_api_key;
        let env_elevenlabs_api_key = env::var("MNEMO_ELEVENLABS_API_KEY").ok();
        let elevenlabs_api_key = profile_config.elevenlabs_api_key;
        let cli_hindsight_api_key = args.hindsight_api_key;
        let env_hindsight_api_key = env::var("MNEMO_HINDSIGHT_API_KEY").ok();
        let hindsight_api_key = profile_config.hindsight_api_key;
        let socket_path = first_some_path([
            args.socket_path,
            env::var_os("MNEMO_SOCKET_PATH").map(PathBuf::from),
            profile_config.socket_path,
            Some(default_socket_path()?),
        ])
        .expect("default socket path is set");
        let context = first_some([
            args.context,
            env::var("MNEMO_CONTEXT").ok(),
            profile_config.context,
            Some(DEFAULT_CONTEXT.to_string()),
        ])
        .expect("default context is set");
        let metadata = first_some_metadata([
            parse_metadata_entries(args.metadata)?,
            parse_metadata_env()?,
            profile_config.metadata,
        ]);
        let tags = first_some_vec([
            non_empty_vec(args.tags),
            parse_tags_env(),
            profile_config.tags,
        ]);
        let strategy = first_some([
            args.strategy,
            env::var("MNEMO_STRATEGY").ok(),
            profile_config.strategy,
        ]);

        Ok(Self {
            profile,
            hindsight_url,
            bank,
            language,
            model,
            cli_elevenlabs_api_key,
            env_elevenlabs_api_key,
            elevenlabs_api_key,
            cli_hindsight_api_key,
            env_hindsight_api_key,
            hindsight_api_key,
            socket_path,
            context,
            metadata,
            tags,
            strategy,
        })
    }
}

fn init_config(config_path: Option<&PathBuf>, force: bool) -> Result<()> {
    let config_path = match config_path {
        Some(path) => path.clone(),
        None => default_config_path()?,
    };

    if config_path.exists() && !force {
        bail!(
            "config file already exists at {}. Use --force to overwrite it",
            config_path.display()
        );
    }

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }

    fs::write(&config_path, DEFAULT_CONFIG)
        .with_context(|| format!("failed to write config file {}", config_path.display()))?;
    println!("Created config file at {}", config_path.display());
    Ok(())
}

fn handle_keychain_command(config: &Config, command: &KeychainCommand) -> Result<()> {
    match command {
        KeychainCommand::Sync => keychain_sync_command(config),
        KeychainCommand::List => keychain_list_command(),
        KeychainCommand::Remove { all, force } => keychain_remove_command(config, *all, *force),
    }
}

fn keychain_sync_command(config: &Config) -> Result<()> {
    let elevenlabs_api_key = first_some([
        config.cli_elevenlabs_api_key.clone(),
        config.env_elevenlabs_api_key.clone(),
        config.elevenlabs_api_key.clone(),
    ]);
    let hindsight_api_key = first_some([
        config.cli_hindsight_api_key.clone(),
        config.env_hindsight_api_key.clone(),
        config.hindsight_api_key.clone(),
    ]);

    if elevenlabs_api_key.is_none() && hindsight_api_key.is_none() {
        bail!(
            "no API keys found to sync. Set MNEMO_ELEVENLABS_API_KEY or MNEMO_HINDSIGHT_API_KEY in your shell first, then re-run"
        );
    }

    if let Some(api_key) = elevenlabs_api_key {
        keychain_write(&config.profile, SecretKind::ElevenLabs, &api_key)?;
        println!(
            "Stored ElevenLabs API key for profile '{}' in macOS Keychain.",
            config.profile
        );
    }
    if let Some(api_key) = hindsight_api_key {
        keychain_write(&config.profile, SecretKind::Hindsight, &api_key)?;
        println!(
            "Stored Hindsight API key for profile '{}' in macOS Keychain.",
            config.profile
        );
    }
    Ok(())
}

fn keychain_list_command() -> Result<()> {
    let entries = keychain_list_entries()?;
    if entries.is_empty() {
        println!("No mnemo API keys found in macOS Keychain.");
        return Ok(());
    }

    for entry in entries {
        println!("{} {}", entry.profile, entry.kind.name());
    }
    Ok(())
}

fn keychain_remove_command(config: &Config, all: bool, force: bool) -> Result<()> {
    let entries = keychain_list_entries()?;
    if entries.is_empty() {
        println!("No mnemo API keys found in macOS Keychain.");
        return Ok(());
    }

    if all {
        if !force && !confirm("Remove all mnemo API keys from macOS Keychain?")? {
            println!("Cancelled.");
            return Ok(());
        }

        for entry in entries {
            keychain_remove(&entry.profile, entry.kind)?;
            println!(
                "Removed {} API key for profile '{}'.",
                entry.kind.display_name(),
                entry.profile
            );
        }
        return Ok(());
    }

    let selected_entries = if force {
        entries
            .into_iter()
            .filter(|entry| entry.profile == config.profile)
            .collect()
    } else {
        prompt_for_keychain_entries(&entries)?
    };

    if selected_entries.is_empty() {
        println!("Cancelled.");
        return Ok(());
    }

    for entry in selected_entries {
        keychain_remove(&entry.profile, entry.kind)?;
        println!(
            "Removed {} API key for profile '{}'.",
            entry.kind.display_name(),
            entry.profile
        );
    }
    Ok(())
}

fn resolve_api_key(config: &Config) -> Result<Option<String>> {
    if let Some(api_key) = first_some([
        config.cli_elevenlabs_api_key.clone(),
        config.env_elevenlabs_api_key.clone(),
    ]) {
        return Ok(Some(api_key));
    }

    if let Some(api_key) = keychain_read(&config.profile, SecretKind::ElevenLabs)? {
        return Ok(Some(api_key));
    }

    Ok(config.elevenlabs_api_key.clone())
}

fn resolve_hindsight_api_key(config: &Config) -> Result<Option<String>> {
    if let Some(api_key) = first_some([
        config.cli_hindsight_api_key.clone(),
        config.env_hindsight_api_key.clone(),
    ]) {
        return Ok(Some(api_key));
    }

    if let Some(api_key) = keychain_read(&config.profile, SecretKind::Hindsight)? {
        return Ok(Some(api_key));
    }

    Ok(config.hindsight_api_key.clone())
}

fn missing_api_key_error() -> anyhow::Error {
    anyhow!(
        "no API key found. Set one of:\n  export MNEMO_ELEVENLABS_API_KEY=...\n  mnemo keychain sync\n  elevenlabs_api_key = \"...\" in ~/.config/mnemo/config.toml"
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecretKind {
    ElevenLabs,
    Hindsight,
}

impl SecretKind {
    fn name(self) -> &'static str {
        match self {
            Self::ElevenLabs => "elevenlabs",
            Self::Hindsight => "hindsight",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::ElevenLabs => "ElevenLabs",
            Self::Hindsight => "Hindsight",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "elevenlabs" => Some(Self::ElevenLabs),
            "hindsight" => Some(Self::Hindsight),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct KeychainEntry {
    profile: String,
    kind: SecretKind,
}

fn keychain_account(profile: &str, kind: SecretKind) -> String {
    format!("profile:{profile}:{}-api-key", kind.name())
}

fn entry_from_keychain_account(account: &str) -> Option<KeychainEntry> {
    let account = account.strip_prefix("profile:")?;
    let (profile, kind) = account.rsplit_once(':')?;
    let kind = kind.strip_suffix("-api-key")?;
    Some(KeychainEntry {
        profile: profile.to_string(),
        kind: SecretKind::from_name(kind)?,
    })
}

fn keychain_write(profile: &str, kind: SecretKind, api_key: &str) -> Result<()> {
    // `security -w <secret>` briefly exposes the secret in process arguments.
    // This is limited to an explicit one-shot sync command and avoids binding
    // Keychain ACLs to mnemo's changing binary signature.
    let output = ProcessCommand::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &keychain_account(profile, kind),
            "-w",
            api_key,
        ])
        .output()
        .context("failed to run /usr/bin/security")?;

    if !output.status.success() {
        bail!(
            "/usr/bin/security failed to write keychain entry: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn keychain_read(profile: &str, kind: SecretKind) -> Result<Option<String>> {
    let output = ProcessCommand::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &keychain_account(profile, kind),
            "-w",
        ])
        .output()
        .context("failed to run /usr/bin/security")?;

    if !output.status.success() {
        return Ok(None);
    }

    let api_key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!api_key.is_empty()).then_some(api_key))
}

fn keychain_remove(profile: &str, kind: SecretKind) -> Result<()> {
    let output = ProcessCommand::new("/usr/bin/security")
        .args([
            "delete-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &keychain_account(profile, kind),
        ])
        .output()
        .context("failed to run /usr/bin/security")?;

    if !output.status.success() {
        bail!(
            "no keychain entry for profile '{}' or deletion failed: {}",
            profile,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn keychain_list_entries() -> Result<Vec<KeychainEntry>> {
    let output = ProcessCommand::new("/usr/bin/security")
        .args(["dump-keychain"])
        .output()
        .context("failed to run /usr/bin/security")?;

    if !output.status.success() {
        bail!("/usr/bin/security failed to list keychain entries");
    }

    let mut entries = Vec::new();
    let mut current_service: Option<String> = None;
    let mut current_account: Option<String> = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.starts_with("keychain:") || line.starts_with("class:") {
            push_keychain_entry(&mut entries, &current_service, &current_account);
            current_service = None;
            current_account = None;
        }
        if let Some(service) = parse_keychain_blob(line, "svce") {
            current_service = Some(service);
        }
        if let Some(account) = parse_keychain_blob(line, "acct") {
            current_account = Some(account);
        }
    }
    push_keychain_entry(&mut entries, &current_service, &current_account);

    entries.sort_by(|a, b| {
        a.profile
            .cmp(&b.profile)
            .then(a.kind.name().cmp(b.kind.name()))
    });
    entries.dedup_by(|a, b| a.profile == b.profile && a.kind == b.kind);
    Ok(entries)
}

fn push_keychain_entry(
    entries: &mut Vec<KeychainEntry>,
    service: &Option<String>,
    account: &Option<String>,
) {
    if service.as_deref() != Some(KEYCHAIN_SERVICE) {
        return;
    }
    let Some(account) = account else {
        return;
    };
    if let Some(entry) = entry_from_keychain_account(account) {
        entries.push(entry);
    }
}

fn parse_keychain_blob(line: &str, key: &str) -> Option<String> {
    let prefix = format!("\"{key}\"<blob>=\"");
    let start = line.find(&prefix)? + prefix.len();
    let end = line[start..].find('"')?;
    Some(line[start..start + end].to_string())
}

fn prompt_for_keychain_entries(entries: &[KeychainEntry]) -> Result<Vec<KeychainEntry>> {
    println!("Select mnemo API keys to remove:");
    for (index, entry) in entries.iter().enumerate() {
        println!("  {}. {} {}", index + 1, entry.profile, entry.kind.name());
    }
    println!("Enter numbers separated by commas, or 'all'. Leave blank to cancel.");

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read selection")?;
    let input = input.trim();
    if input.is_empty() {
        return Ok(Vec::new());
    }
    if input.eq_ignore_ascii_case("all") {
        return Ok(entries.to_vec());
    }

    let mut selected = Vec::new();
    for part in input.split(',') {
        let index: usize = part
            .trim()
            .parse()
            .with_context(|| format!("invalid selection: {part}"))?;
        let Some(entry) = entries.get(index.saturating_sub(1)) else {
            bail!("selection out of range: {index}");
        };
        selected.push(entry.clone());
    }
    selected.sort_by(|a, b| {
        a.profile
            .cmp(&b.profile)
            .then(a.kind.name().cmp(b.kind.name()))
    });
    selected.dedup_by(|a, b| a.profile == b.profile && a.kind == b.kind);
    Ok(selected)
}

fn confirm(prompt: &str) -> Result<bool> {
    println!("{prompt} Type 'yes' to continue:");
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read confirmation")?;
    Ok(input.trim() == "yes")
}

const DEFAULT_CONFIG: &str = r#"# mnemo config
# Default location: ~/.config/mnemo/config.toml
# Environment variables override these values. Explicit CLI flags override both.

[profiles.default]
# Required: your Hindsight API URL.
# hindsight_url = "https://your-hindsight-api.example.com"

# Required: Hindsight memory bank to retain voice notes into.
bank = "personal"

# ElevenLabs STT settings.
language = "eng"
model = "scribe_v2"

# Default context sent to Hindsight with each retained voice note.
context = "user recorded voice memo"

# Optional Hindsight retain settings. Leave unset to send null.
# metadata = { source = "mnemo" }
# tags = ["voice-note"]
# strategy = "append"

# Prefer macOS Keychain or MNEMO_ELEVENLABS_API_KEY for secrets, but config is supported.
# elevenlabs_api_key = "..."

# Optional. Prefer macOS Keychain or MNEMO_HINDSIGHT_API_KEY for secrets, but config is supported.
# hindsight_api_key = "..."

# Defaults to ~/.local/state/mnemo/mnemo.sock
# socket_path = "/Users/you/.local/state/mnemo/mnemo.sock"
"#;

fn first_some<const N: usize>(values: [Option<String>; N]) -> Option<String> {
    values.into_iter().flatten().find(|value| !value.is_empty())
}

fn first_some_path<const N: usize>(values: [Option<PathBuf>; N]) -> Option<PathBuf> {
    values
        .into_iter()
        .flatten()
        .find(|value| !value.as_os_str().is_empty())
}

fn first_some_metadata<const N: usize>(
    values: [Option<BTreeMap<String, String>>; N],
) -> Option<BTreeMap<String, String>> {
    values.into_iter().flatten().find(|value| !value.is_empty())
}

fn first_some_vec<const N: usize>(values: [Option<Vec<String>>; N]) -> Option<Vec<String>> {
    values.into_iter().flatten().find(|value| !value.is_empty())
}

fn non_empty_vec(values: Vec<String>) -> Option<Vec<String>> {
    let values: Vec<String> = values
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect();
    (!values.is_empty()).then_some(values)
}

fn parse_tags_env() -> Option<Vec<String>> {
    env::var("MNEMO_TAGS")
        .ok()
        .and_then(|tags| non_empty_vec(tags.split(',').map(|tag| tag.trim().to_string()).collect()))
}

fn parse_metadata_env() -> Result<Option<BTreeMap<String, String>>> {
    let Some(metadata) = env::var("MNEMO_METADATA").ok() else {
        return Ok(None);
    };
    let metadata =
        serde_json::from_str(&metadata).context("failed to parse MNEMO_METADATA JSON")?;
    Ok(Some(metadata))
}

fn parse_metadata_entries(entries: Vec<String>) -> Result<Option<BTreeMap<String, String>>> {
    if entries.is_empty() {
        return Ok(None);
    }

    let mut metadata = BTreeMap::new();
    for entry in entries {
        let Some((key, value)) = entry.split_once('=') else {
            bail!("metadata entries must use KEY=VALUE format: {entry}");
        };
        if key.is_empty() {
            bail!("metadata keys cannot be empty");
        }
        metadata.insert(key.to_string(), value.to_string());
    }
    Ok(Some(metadata))
}

fn read_file_config(path: &PathBuf) -> Result<FileConfig> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }

    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

fn default_config_path() -> Result<PathBuf> {
    let home =
        env::var_os("HOME").ok_or_else(|| anyhow!("HOME environment variable is not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("mnemo")
        .join("config.toml"))
}

fn default_socket_path() -> Result<PathBuf> {
    let home =
        env::var_os("HOME").ok_or_else(|| anyhow!("HOME environment variable is not set"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("mnemo")
        .join("mnemo.sock"))
}

struct SocketGuard {
    path: PathBuf,
}

async fn bind_control_socket(path: PathBuf) -> Result<(SocketGuard, UnixListener)> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create socket directory {}", parent.display()))?;
    }

    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to remove stale socket {}", path.display()));
        }
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind socket {}", path.display()))?;
    Ok((SocketGuard { path }, listener))
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn ensure_singleton_socket(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    match StdUnixStream::connect(path) {
        Ok(_) => bail!("mnemo is already recording via {}", path.display()),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(path)
                .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
            Ok(())
        }
        Err(err) => Err(err).with_context(|| format!("failed to check socket {}", path.display())),
    }
}

async fn control_socket_server(
    listener: UnixListener,
    stop_tx: mpsc::Sender<()>,
    status_rx: watch::Receiver<RecordingStatus>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(handle_control_client(
            stream,
            stop_tx.clone(),
            status_rx.clone(),
        ));
    }
}

async fn handle_control_client(
    stream: UnixStream,
    stop_tx: mpsc::Sender<()>,
    mut status_rx: watch::Receiver<RecordingStatus>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let status = status_rx.borrow().clone();
    if write_status_message(&mut writer, status).await.is_err() {
        return;
    }

    let mut read_closed = false;
    loop {
        let mut line = String::new();
        tokio::select! {
            status_changed = status_rx.changed() => {
                if status_changed.is_err() {
                    return;
                }
                let status = status_rx.borrow().clone();
                if write_status_message(&mut writer, status).await.is_err() {
                    return;
                }
            }
            read_result = reader.read_line(&mut line), if !read_closed => {
                let Ok(bytes_read) = read_result else {
                    let _ = write_error_message(&mut writer, "failed to read command").await;
                    read_closed = true;
                    continue;
                };
                if bytes_read == 0 {
                    read_closed = true;
                    continue;
                }
                if handle_socket_command(line.trim(), &stop_tx, &status_rx, &mut writer).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn handle_socket_command<W>(
    line: &str,
    stop_tx: &mpsc::Sender<()>,
    status_rx: &watch::Receiver<RecordingStatus>,
    writer: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let command: SocketCommand = match serde_json::from_str(line) {
        Ok(command) => command,
        Err(error) => {
            write_error_message(writer, &format!("invalid JSON command: {error}")).await?;
            return Ok(());
        }
    };

    match command.command_type.as_str() {
        "stop" => {
            let state = status_rx.borrow().state.clone();
            if matches!(state, RecordingState::Recording) {
                let _ = stop_tx.send(());
            } else {
                write_error_message(writer, "cannot stop unless recording").await?;
            }
        }
        "status" => {
            let status = status_rx.borrow().clone();
            write_status_message(writer, status).await?;
        }
        command => {
            write_error_message(writer, &format!("unknown command type: {command}")).await?;
        }
    }
    Ok(())
}

async fn write_status_message<W>(writer: &mut W, status: RecordingStatus) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_json_line(
        writer,
        &StatusMessage {
            message_type: "status",
            status,
        },
    )
    .await
}

async fn write_error_message<W>(writer: &mut W, message: &str) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_json_line(
        writer,
        &ErrorMessage {
            message_type: "error",
            message: message.to_string(),
        },
    )
    .await
}

async fn write_json_line<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(value).context("failed to serialize socket message")?;
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .context("failed to write socket message")
}

fn send_recording_status(
    status_tx: &watch::Sender<RecordingStatus>,
    state: RecordingState,
    message: Option<String>,
) {
    let started_at = status_tx.borrow().started_at.clone();
    status_tx.send_replace(RecordingStatus {
        state,
        started_at,
        message,
    });
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn spawn_enter_stop_thread(stop_tx: mpsc::Sender<()>) {
    thread::spawn(move || {
        let mut line = String::new();
        if matches!(io::stdin().read_line(&mut line), Ok(bytes_read) if bytes_read > 0) {
            let _ = stop_tx.send(());
        }
    });
}

struct Recording {
    samples: Vec<i16>,
    sample_rate: u32,
    channels: u16,
}

fn record_until_stop(stop_rx: mpsc::Receiver<()>) -> Result<Recording> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device available"))?;
    let config = device
        .default_input_config()
        .context("failed to get default input config")?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels();
    let samples = Arc::new(Mutex::new(Vec::new()));

    println!(
        "Recording from '{}' at {sample_rate} Hz, {channels} channel(s). Press Enter or run `mnemo stop` to stop...",
        device
            .name()
            .unwrap_or_else(|_| "default input".to_string())
    );

    let stream = match config.sample_format() {
        cpal::SampleFormat::I8 => {
            build_stream::<i8>(&device, &config.into(), Arc::clone(&samples))?
        }
        cpal::SampleFormat::I16 => {
            build_stream::<i16>(&device, &config.into(), Arc::clone(&samples))?
        }
        cpal::SampleFormat::I32 => {
            build_stream::<i32>(&device, &config.into(), Arc::clone(&samples))?
        }
        cpal::SampleFormat::I64 => {
            build_stream::<i64>(&device, &config.into(), Arc::clone(&samples))?
        }
        cpal::SampleFormat::U8 => {
            build_stream::<u8>(&device, &config.into(), Arc::clone(&samples))?
        }
        cpal::SampleFormat::U16 => {
            build_stream::<u16>(&device, &config.into(), Arc::clone(&samples))?
        }
        cpal::SampleFormat::U32 => {
            build_stream::<u32>(&device, &config.into(), Arc::clone(&samples))?
        }
        cpal::SampleFormat::U64 => {
            build_stream::<u64>(&device, &config.into(), Arc::clone(&samples))?
        }
        cpal::SampleFormat::F32 => {
            build_stream::<f32>(&device, &config.into(), Arc::clone(&samples))?
        }
        cpal::SampleFormat::F64 => {
            build_stream::<f64>(&device, &config.into(), Arc::clone(&samples))?
        }
        format => bail!("unsupported input sample format: {format:?}"),
    };

    stream.play().context("failed to start input stream")?;
    stop_rx.recv().context("failed to receive stop signal")?;
    thread::sleep(Duration::from_millis(100));
    drop(stream);

    let samples = Arc::try_unwrap(samples)
        .map_err(|_| anyhow!("recording buffer is still in use"))?
        .into_inner()
        .map_err(|_| anyhow!("recording buffer lock was poisoned"))?;

    Ok(Recording {
        samples,
        sample_rate,
        channels,
    })
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: Arc<Mutex<Vec<i16>>>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample,
    i16: cpal::FromSample<T>,
{
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _| {
            if let Ok(mut samples) = samples.lock() {
                samples.extend(data.iter().copied().map(i16::from_sample));
            }
        },
        move |err| eprintln!("Audio input error: {err}"),
        None,
    )?;

    Ok(stream)
}

async fn transcribe(config: &Config, elevenlabs_api_key: &str, wav: Vec<u8>) -> Result<String> {
    let client = reqwest::Client::new();
    let file_part = multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/x-wav")?;
    let mut form = multipart::Form::new()
        .part("file", file_part)
        .text("model_id", config.model.clone())
        .text("tag_audio_events", "false");

    if config.language.to_lowercase() != "auto" {
        form = form.text("language_code", config.language.clone());
    }

    let response = client
        .post("https://api.elevenlabs.io/v1/speech-to-text")
        .header("xi-api-key", elevenlabs_api_key)
        .multipart(form)
        .send()
        .await
        .context("failed to call ElevenLabs STT")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read ElevenLabs response")?;
    if !status.is_success() {
        bail!("ElevenLabs STT failed with {status}: {body}");
    }

    let transcription: ElevenLabsTranscription =
        serde_json::from_str(&body).context("failed to parse ElevenLabs response")?;
    Ok(transcription.text)
}

async fn retain_in_hindsight(
    config: &Config,
    hindsight_url: &str,
    bank: &str,
    transcript: &str,
) -> Result<()> {
    let url = format!(
        "{}/v1/default/banks/{}/memories",
        hindsight_url.trim_end_matches('/'),
        bank
    );
    let body = json!({
        "items": [{
            "content": transcript,
            "context": config.context,
            "metadata": config.metadata,
            "tags": config.tags,
            "strategy": config.strategy,
        }],
        "async": false
    });

    let client = reqwest::Client::new();
    let mut request = client.post(url).json(&body);
    if let Some(api_key) = resolve_hindsight_api_key(config)? {
        request = request.bearer_auth(api_key);
    }

    let response = request
        .send()
        .await
        .context("failed to call Hindsight retain")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read Hindsight response")?;
    if !status.is_success() {
        bail!("Hindsight retain failed with {status}: {body}");
    }

    Ok(())
}

fn wav_bytes(samples: &[i16], sample_rate: u32, channels: u16) -> Result<Vec<u8>> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .ok_or_else(|| anyhow!("recording is too large"))?;
    let riff_len = 36usize
        .checked_add(data_len)
        .ok_or_else(|| anyhow!("recording is too large"))?;
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let block_align = channels * 2;

    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(riff_len as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }

    Ok(wav)
}

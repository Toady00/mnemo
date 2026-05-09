use std::env;
use std::fs;
use std::io;
use std::io::ErrorKind;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use cpal::Sample;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use reqwest::multipart;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const DEFAULT_BANK_ID: &str = "personal";

#[derive(Debug, Parser)]
#[command(about = "Record your voice, transcribe with ElevenLabs, and retain in Hindsight")]
struct CliArgs {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(long, global = true)]
    config: Option<PathBuf>,

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
}

#[derive(Clone, Debug, Subcommand)]
enum Command {
    /// Start recording a voice note.
    Record,
    /// Stop the currently running recorder.
    Stop,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    hindsight_url: Option<String>,
    bank: Option<String>,
    language: Option<String>,
    model: Option<String>,
    elevenlabs_api_key: Option<String>,
    hindsight_api_key: Option<String>,
    socket_path: Option<PathBuf>,
}

#[derive(Debug)]
struct Config {
    hindsight_url: Option<String>,
    bank: String,
    language: String,
    model: String,
    elevenlabs_api_key: Option<String>,
    hindsight_api_key: Option<String>,
    socket_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ElevenLabsTranscription {
    text: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();
    let command = args.command.clone().unwrap_or(Command::Record);
    let config = Config::load(args)?;

    match &command {
        Command::Record => record(config).await,
        Command::Stop => stop_recording(&config).await,
    }
}

async fn record(config: Config) -> Result<()> {
    let elevenlabs_api_key = config.elevenlabs_api_key.as_deref().ok_or_else(|| {
        anyhow!(
            "MNEMO_ELEVENLABS_API_KEY must be set in the environment, config file, or --elevenlabs-api-key"
        )
    })?;
    let hindsight_url = config.hindsight_url.as_deref().ok_or_else(|| {
        anyhow!(
            "MNEMO_HINDSIGHT_API_URL must be set in the environment, config file, or --hindsight-url"
        )
    })?;
    ensure_singleton_socket(&config.socket_path)?;
    let (_socket_guard, listener) = bind_control_socket(config.socket_path.clone()).await?;
    let (stop_tx, stop_rx) = mpsc::channel();
    let socket_task = tokio::spawn(control_socket_server(listener, stop_tx.clone()));
    spawn_enter_stop_thread(stop_tx);

    let recording = tokio::task::spawn_blocking(move || record_until_stop(stop_rx))
        .await
        .context("recording task failed")??;
    socket_task.abort();
    println!("Recording complete. Sending to ElevenLabs...");

    let wav = wav_bytes(
        &recording.samples,
        recording.sample_rate,
        recording.channels,
    )?;
    let transcript = transcribe(&config, elevenlabs_api_key, wav).await?;
    let transcript = transcript.trim();

    if transcript.is_empty() {
        println!("No speech detected. Nothing retained in Hindsight.");
        return Ok(());
    }

    println!("\nTranscription: {transcript}");
    println!(
        "Retaining transcript in Hindsight bank '{}'...",
        config.bank
    );
    retain_in_hindsight(&config, hindsight_url, transcript).await?;
    println!("Retained in Hindsight.");

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
        .write_all(b"stop\n")
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
        let file_config = read_file_config(args.config.as_ref())?;

        let hindsight_url = first_some([
            args.hindsight_url,
            env::var("MNEMO_HINDSIGHT_API_URL").ok(),
            file_config.hindsight_url,
        ]);
        let bank = first_some([
            args.bank,
            env::var("MNEMO_BANK_ID").ok(),
            file_config.bank,
            Some(DEFAULT_BANK_ID.to_string()),
        ])
        .expect("default bank is set");
        let language = first_some([
            args.language,
            env::var("MNEMO_ELEVENLABS_LANGUAGE").ok(),
            file_config.language,
            Some("eng".to_string()),
        ])
        .expect("default language is set");
        let model = first_some([
            args.model,
            env::var("MNEMO_ELEVENLABS_MODEL").ok(),
            file_config.model,
            Some("scribe_v2".to_string()),
        ])
        .expect("default model is set");
        let elevenlabs_api_key = first_some([
            args.elevenlabs_api_key,
            env::var("MNEMO_ELEVENLABS_API_KEY").ok(),
            file_config.elevenlabs_api_key,
        ]);
        let hindsight_api_key = first_some([
            args.hindsight_api_key,
            env::var("MNEMO_HINDSIGHT_API_KEY").ok(),
            file_config.hindsight_api_key,
        ]);
        let socket_path = first_some_path([
            args.socket_path,
            env::var_os("MNEMO_SOCKET_PATH").map(PathBuf::from),
            file_config.socket_path,
            Some(default_socket_path()?),
        ])
        .expect("default socket path is set");

        Ok(Self {
            hindsight_url,
            bank,
            language,
            model,
            elevenlabs_api_key,
            hindsight_api_key,
            socket_path,
        })
    }
}

fn first_some<const N: usize>(values: [Option<String>; N]) -> Option<String> {
    values.into_iter().flatten().find(|value| !value.is_empty())
}

fn first_some_path<const N: usize>(values: [Option<PathBuf>; N]) -> Option<PathBuf> {
    values
        .into_iter()
        .flatten()
        .find(|value| !value.as_os_str().is_empty())
}

fn read_file_config(config_path: Option<&PathBuf>) -> Result<FileConfig> {
    let path = match config_path {
        Some(path) => path.clone(),
        None => default_config_path()?,
    };

    if !path.exists() {
        return Ok(FileConfig::default());
    }

    let contents = fs::read_to_string(&path)
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

async fn control_socket_server(listener: UnixListener, stop_tx: mpsc::Sender<()>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let mut command = String::new();
        if stream.read_to_string(&mut command).await.is_err() {
            let _ = stream.write_all(b"error reading command\n").await;
            continue;
        }

        match command.trim() {
            "stop" => {
                let _ = stop_tx.send(());
                let _ = stream.write_all(b"ok stopping\n").await;
            }
            _ => {
                let _ = stream.write_all(b"unknown command\n").await;
            }
        }
    }
}

fn spawn_enter_stop_thread(stop_tx: mpsc::Sender<()>) {
    thread::spawn(move || {
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_ok() {
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

async fn retain_in_hindsight(config: &Config, hindsight_url: &str, transcript: &str) -> Result<()> {
    let url = format!(
        "{}/v1/default/banks/{}/memories",
        hindsight_url.trim_end_matches('/'),
        config.bank
    );
    let body = json!({
        "items": [{ "content": transcript }],
        "async": false
    });

    let client = reqwest::Client::new();
    let mut request = client.post(url).json(&body);
    if let Some(api_key) = &config.hindsight_api_key {
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

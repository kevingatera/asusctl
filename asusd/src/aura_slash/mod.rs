use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration as StdDuration, Instant};

use config::SlashConfig;
use log::warn;
use rog_platform::hid_raw::HidRaw;
use rog_platform::usb_raw::USBRaw;
use rog_slash::usb::{
    slash_pkt_custom_init, slash_pkt_custom_payload, slash_pkt_enable, slash_pkt_init,
    slash_pkt_options, slash_pkt_save, slash_pkt_set_mode,
};
use rog_slash::{SlashMode, SlashType};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::{Mutex, MutexGuard};
use tokio::task::JoinHandle;
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;
use which::which;

use crate::error::RogError;

pub mod config;
pub mod trait_impls;

struct DynamicTask {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub struct Slash {
    hid: Option<Arc<Mutex<HidRaw>>>,
    usb: Option<Arc<Mutex<USBRaw>>>,
    config: Arc<Mutex<SlashConfig>>,
    dynamic_task: Arc<Mutex<Option<DynamicTask>>>,
    runtime_handle: tokio::runtime::Handle,
}

impl Slash {
    pub fn new(
        hid: Option<Arc<Mutex<HidRaw>>>,
        usb: Option<Arc<Mutex<USBRaw>>>,
        config: Arc<Mutex<SlashConfig>>,
    ) -> Self {
        Self {
            hid,
            usb,
            config,
            dynamic_task: Arc::new(Mutex::new(None)),
            runtime_handle: tokio::runtime::Handle::current(),
        }
    }

    pub async fn lock_config(&self) -> MutexGuard<'_, SlashConfig> {
        self.config.lock().await
    }

    pub async fn write_bytes(&self, message: &[u8]) -> Result<(), RogError> {
        if let Some(hid) = &self.hid {
            hid.lock().await.write_bytes(message)?;
        } else if let Some(usb) = &self.usb {
            usb.lock().await.write_bytes(message)?;
        }
        Ok(())
    }

    /// Initialise the device if required. Locks the internal config so be wary
    /// of deadlocks.
    pub async fn do_initialization(&self) -> Result<(), RogError> {
        // Don't try to initialise these models as the asus drivers already did
        let config = self.config.lock().await;
        let slash_type = config.slash_type;
        let brightness = config.brightness;
        let interval = config.display_interval;
        let mode = config.display_mode;
        let enabled = config.enabled;
        for pkt in &slash_pkt_init(config.slash_type) {
            self.write_bytes(pkt).await?;
        }
        self.write_bytes(&slash_pkt_enable(config.slash_type, config.enabled))
            .await?;

        // Apply config upon initialization
        let option_packets = slash_pkt_options(
            config.slash_type,
            config.enabled,
            config.brightness,
            config.display_interval,
        );
        self.write_bytes(&option_packets).await?;

        if !matches!(
            mode,
            SlashMode::BatteryLevel | SlashMode::Audio | SlashMode::AudioSpectrum
        ) {
            let mode_packets = slash_pkt_set_mode(config.slash_type, config.display_mode);
            for pkt in &mode_packets {
                self.write_bytes(pkt).await?;
            }
            self.write_bytes(&slash_pkt_save(config.slash_type))
                .await
                .ok();
        }
        drop(config);
        self.update_dynamic_state(slash_type, mode, enabled, brightness, interval)
            .await;
        Ok(())
    }

    pub async fn update_dynamic_state(
        &self,
        slash_type: SlashType,
        mode: SlashMode,
        enabled: bool,
        brightness: u8,
        interval: u8,
    ) {
        if matches!(
            mode,
            SlashMode::BatteryLevel | SlashMode::Audio | SlashMode::AudioSpectrum
        ) && enabled
            && brightness > 0
        {
            if let Err(err) = self
                .start_dynamic_task(slash_type, mode, brightness, interval)
                .await
            {
                warn!("Failed to start slash dynamic mode: {err}");
            }
        } else {
            self.stop_dynamic_task().await;
        }
    }

    pub async fn stop_dynamic_task(&self) {
        if let Some(task) = self.dynamic_task.lock().await.take() {
            task.cancel.cancel();
            let _ = task.handle.await;
        }
    }

    async fn start_dynamic_task(
        &self,
        slash_type: SlashType,
        mode: SlashMode,
        brightness: u8,
        interval: u8,
    ) -> Result<(), RogError> {
        self.stop_dynamic_task().await;
        let cancel = CancellationToken::new();
        let slash_clone = self.clone();
        let cancel_clone = cancel.clone();
        let handle = self.runtime_handle.spawn(async move {
            match mode {
                SlashMode::BatteryLevel => {
                    if let Err(err) = run_battery_effect(
                        slash_clone, slash_type, brightness, interval, cancel_clone,
                    )
                    .await
                    {
                        warn!("battery effect exited: {err}");
                    }
                }
                SlashMode::Audio | SlashMode::AudioSpectrum => {
                    if let Err(err) =
                        run_audio_effect(slash_clone, slash_type, brightness, mode, cancel_clone)
                            .await
                    {
                        warn!("audio effect exited: {err}");
                    }
                }
                _ => {}
            }
        });

        *self.dynamic_task.lock().await = Some(DynamicTask {
            cancel,
            handle,
        });
        Ok(())
    }

    pub async fn write_custom_init(&self, slash_type: SlashType) -> Result<(), RogError> {
        for pkt in &slash_pkt_custom_init(slash_type) {
            self.write_bytes(pkt).await?;
        }
        // Give the device a moment to process init
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }

    pub async fn write_custom_frame(
        &self,
        slash_type: SlashType,
        data: &[u8; 7],
    ) -> Result<(), RogError> {
        let packet = slash_pkt_custom_payload(slash_type, data);
        self.write_bytes(&packet).await
    }

    pub fn audio_capture_available() -> Result<(), RogError> {
        if which("pw-cat").is_ok() || which("parec").is_ok() {
            Ok(())
        } else {
            Err(RogError::MissingFunction(
                "Audio capture requires pw-cat or parec in PATH".to_string(),
            ))
        }
    }
}

fn battery_capacity_paths() -> &'static Vec<PathBuf> {
    static PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATHS.get_or_init(|| {
        let mut paths = Vec::new();
        let power_supply = Path::new("/sys/class/power_supply");
        if let Ok(entries) = fs::read_dir(power_supply) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.join("type").exists() {
                    if let Ok(kind) = fs::read_to_string(path.join("type")) {
                        if kind.trim() == "Battery" {
                            paths.push(path.join("capacity"));
                        }
                    }
                }
            }
        }
        paths
    })
}

fn read_battery_percentage() -> Option<f32> {
    let mut sum = 0.0;
    let mut count = 0.0;
    for path in battery_capacity_paths() {
        if let Ok(value) = fs::read_to_string(path) {
            if let Ok(parsed) = value.trim().parse::<f32>() {
                sum += parsed;
                count += 1.0;
            }
        }
    }
    if count > 0.0 {
        Some(sum / count)
    } else {
        None
    }
}

async fn run_battery_effect(
    slash: Slash,
    slash_type: SlashType,
    brightness: u8,
    speed: u8,
    cancel: CancellationToken,
) -> Result<(), RogError> {
    slash.write_custom_init(slash_type).await?;
    let mut payload = [0u8; 7];
    let mut ticker = interval(Duration::from_secs(std::cmp::max(
        5,
        (speed as u64 / 4) + 5,
    )));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let pct = read_battery_percentage().unwrap_or(0.0);
                build_battery_pattern(pct, brightness, &mut payload);
                slash.write_custom_frame(slash_type, &payload).await?;
            }
        }
    }
    Ok(())
}

fn build_battery_pattern(percentage: f32, brightness: u8, payload: &mut [u8; 7]) {
    let pct_per_segment = 100.0 / 7.0;
    let mut remaining = percentage.clamp(0.0, 100.0);
    for slot in payload.iter_mut().rev() {
        if remaining >= pct_per_segment {
            *slot = brightness;
            remaining -= pct_per_segment;
        } else if remaining > 0.0 {
            let fraction = (remaining / pct_per_segment).clamp(0.0, 1.0);
            *slot = (brightness as f32 * fraction) as u8;
            remaining = 0.0;
        } else {
            *slot = 0;
        }
    }
}

async fn run_audio_effect(
    slash: Slash,
    slash_type: SlashType,
    brightness: u8,
    mode: SlashMode,
    cancel: CancellationToken,
) -> Result<(), RogError> {
    slash.write_custom_init(slash_type).await?;
    let (tx, mut rx) = mpsc::unbounded_channel::<[u8; 7]>();
    let writer = {
        let cancel = cancel.clone();
        let slash = slash.clone();
        let handle = slash.runtime_handle.clone();
        handle.spawn(async move {
            while let Some(payload) = rx.recv().await {
                if cancel.is_cancelled() {
                    break;
                }
                if let Err(err) = slash.write_custom_frame(slash_type, &payload).await {
                    warn!("slash audio frame failed: {err}");
                    break;
                }
            }
        })
    };

    let handle = slash.runtime_handle.clone();
    let capture = handle.spawn(capture_audio_stream(mode, brightness, tx, cancel.clone())).await;

    writer.abort();

    match capture {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(RogError::DoTask(err)),
        Err(err) => Err(RogError::DoTask(format!("{err}"))),
    }
}

async fn capture_audio_stream(
    mode: SlashMode,
    brightness: u8,
    tx: mpsc::UnboundedSender<[u8; 7]>,
    cancel: CancellationToken,
) -> Result<(), String> {
    let mut command = build_audio_command()?;
    command.stdout(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start audio capture: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Audio capture produced no stdout".to_string())?;
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut buffer = vec![0u8; 4096];
    let mut carry = Vec::new();
    let mut aggregator = AudioAggregator::new(brightness, 2048, mode);
    let _ = tx.send([0; 7]);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            read = reader.read(&mut buffer) => {
                let read = read.map_err(|e| format!("Audio read failed: {e}"))?;
                if read == 0 {
                    break;
                }
                carry.extend_from_slice(&buffer[..read]);

                let mut samples = Vec::with_capacity(carry.len() / 2);
                let mut idx = 0;
                while idx + 1 < carry.len() {
                    let value = i16::from_le_bytes([carry[idx], carry[idx + 1]]);
                    samples.push(value as f32 / i16::MAX as f32);
                    idx += 2;
                }
                carry.drain(..idx);

                if !samples.is_empty() {
                    if let Some(payload) = aggregator.process_samples(&samples) {
                        let _ = tx.send(payload);
                    }
                }
            }
        }
    }

    if let Some(id) = child.id() {
        if let Err(err) = child.kill().await {
            warn!("Failed to stop audio capture ({id}): {err}");
        }
    }
    Ok(())
}

fn build_audio_command() -> Result<tokio::process::Command, String> {
    if which("pw-cat").is_ok() {
        let mut cmd = Command::new("pw-cat");
        apply_audio_env(&mut cmd);
        cmd.args([
            "--record",
            "-",
            "--format=s16",
            "--rate=16000",
            "--channels=1",
            "--raw",
        ]);
        return Ok(cmd);
    }
    if which("parec").is_ok() {
        let mut cmd = Command::new("parec");
        apply_audio_env(&mut cmd);
        cmd.args([
            "--format=s16le",
            "--rate=16000",
            "--channels=1",
            "--device=@DEFAULT_SINK@.monitor",
        ]);
        return Ok(cmd);
    }
    Err("Neither pw-cat nor parec found in PATH".into())
}

fn apply_audio_env(cmd: &mut Command) {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok();
    if std::env::var_os("PULSE_SERVER").is_none() {
        if let Some(dir) = runtime_dir.as_ref() {
            cmd.env("PULSE_SERVER", format!("unix:{}/pulse/native", dir));
        }
    }
    if let Some(dir) = runtime_dir {
        cmd.env("PIPEWIRE_RUNTIME_DIR", dir);
    }
}

struct AudioAggregator {
    brightness: u8,
    chunk_size: usize,
    samples: Vec<f32>,
    avg_peak: f32,
    mode: SlashMode,
    last_emit: Instant,
}

impl AudioAggregator {
    fn new(brightness: u8, chunk_size: usize, mode: SlashMode) -> Self {
        Self {
            brightness,
            chunk_size,
            samples: Vec::with_capacity(chunk_size),
            avg_peak: 0.1,
            mode,
            last_emit: Instant::now(),
        }
    }

    fn process_samples(&mut self, data: &[f32]) -> Option<[u8; 7]> {
        self.samples.extend(data.iter().map(|sample| sample.abs()));
        if self.samples.len() < self.chunk_size {
            return None;
        }
        if self.last_emit.elapsed() < StdDuration::from_millis(40) {
            self.samples.clear();
            return None;
        }
        self.last_emit = Instant::now();
        let mut chunk = vec![0.0; self.chunk_size];
        let start = self.samples.len().saturating_sub(self.chunk_size);
        chunk.copy_from_slice(&self.samples[start..]);
        self.samples.clear();

        let per_segment = std::cmp::max(1, chunk.len() / 7);
        let mut buckets = [0.0f32; 7];
        for (idx, bucket) in buckets.iter_mut().enumerate() {
            let begin = idx * per_segment;
            let end = std::cmp::min(begin + per_segment, chunk.len());
            if begin >= end {
                continue;
            }
            *bucket = chunk[begin..end].iter().copied().sum::<f32>() / (end - begin) as f32;
        }
        let peak = buckets
            .iter()
            .copied()
            .fold(0.0f32, |acc, value| value.max(acc));
        self.avg_peak = (self.avg_peak * 0.9) + (peak * 0.1).max(0.05);
        let mut payload = [0u8; 7];
        let exponent = match self.mode {
            SlashMode::AudioSpectrum => 1.5,
            _ => 1.0,
        };
        for (idx, bucket) in buckets.iter().enumerate() {
            let mut level = (bucket / self.avg_peak).clamp(0.0, 1.5);
            level = level.powf(exponent).min(1.0);
            payload[6 - idx] = (level * self.brightness as f32).clamp(0.0, 255.0) as u8;
        }
        if peak > 0.001 {
             log::debug!("Audio peak: {:.4}, payload: {:?}", peak, payload);
        }
        Some(payload)
    }
}

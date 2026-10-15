//! Built-in dictation (v0.14.0).
//!
//! Why this exists: the user's dictation tool (Handy) never worked while
//! an AppMaka web app held focus, and the hook audit + compatibility
//! mode pointed at the webview focus itself. So dictation moved in:
//! a Whisper model runs locally, on demand — nothing is loaded, resident,
//! or listening until the user presses the dictation hotkey.
//!
//! Shape:
//! - Trigger: a global hotkey (hotkeys.rs registry, RegisterHotKey under
//!   the hood — no keyboard hook, so it works over ANY window and keeps
//!   working in Dictation compatibility mode). Toggle: press to start,
//!   press again to stop. Esc cancels while recording (polled, no hook).
//! - Capture: cpal opens the default microphone only while recording.
//! - Transcription: the official whisper.cpp binary (pinned release
//!   b5454) is downloaded once into the app-data dir and run as a child
//!   process per utterance; it exits after each transcription, so its
//!   memory is fully reclaimed. The model file (tiny.en ~75 MB or
//!   base.en ~142 MB) is likewise downloaded once, on the user's click.
//! - Delivery: the text is saved to the transcript list (last 50,
//!   shown in the clipboard popup's Dictation tab), written to the OS
//!   clipboard, and pasted into the focused window (Windows: SendInput
//!   Ctrl+V; the recording pill never takes focus, so the target is
//!   still focused). Clicking a transcript in the popup just copies it
//!   back — the standard clipboard contract.
//!
//! Standing cost when not dictating: zero. No thread, no model, no mic.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::hotkeys::{self, BindingStatus, HotkeyKind};

pub const BINDING_ID: &str = "dictation:toggle";
pub const DEFAULT_HOTKEY: &str = "Super+Alt+D";
const MAX_TRANSCRIPTS: usize = 50;
const MAX_RECORD_SECS: u64 = 120;
const WHISPER_RELEASE: &str = "b5454";
#[cfg(windows)]
const WHISPER_ARCHIVE: &str = "whisper-bin-x64.zip";
#[cfg(target_os = "linux")]
const WHISPER_ARCHIVE: &str = "whisper-bin-ubuntu-x64.tar.gz";
#[cfg(windows)]
const WHISPER_EXE: &str = "whisper-cli.exe";
#[cfg(target_os = "linux")]
const WHISPER_EXE: &str = "whisper-cli";

// ---------------------------------------------------------------------------
// Persisted data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transcript {
    pub id: String,
    /// Unix epoch milliseconds.
    pub ts: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DictationFile {
    #[serde(default = "default_hotkey")]
    hotkey: String,
    /// "tiny" or "base".
    #[serde(default = "default_model")]
    model: String,
    #[serde(default)]
    transcripts: Vec<Transcript>,
}

fn default_hotkey() -> String {
    DEFAULT_HOTKEY.to_string()
}
fn default_model() -> String {
    "tiny".to_string()
}

impl Default for DictationFile {
    fn default() -> Self {
        Self {
            hotkey: default_hotkey(),
            model: default_model(),
            transcripts: Vec::new(),
        }
    }
}

struct Session {
    stop: Arc<AtomicBool>,
}

pub struct DictationState {
    data: Mutex<DictationFile>,
    session: Mutex<Option<Session>>,
    transcribing: Mutex<bool>,
}

fn dictation_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Couldn't find the app data folder: {e}"))?
        .join("dictation");
    fs::create_dir_all(&dir)
        .map_err(|e| format!("Couldn't create the dictation folder: {e}"))?;
    Ok(dir)
}

fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(dictation_dir(app)?.join("dictation.json"))
}

fn model_file_name(model: &str) -> String {
    format!("ggml-{model}.en.bin")
}

fn model_path(app: &AppHandle, model: &str) -> Result<PathBuf, String> {
    Ok(dictation_dir(app)?.join("models").join(model_file_name(model)))
}

/// Find the extracted whisper-cli under bin/, tolerating the archive's
/// internal folder layout (files land in a subfolder on some builds).
fn binary_path(app: &AppHandle) -> Option<PathBuf> {
    let bin = dictation_dir(app).ok()?.join("bin");
    fn walk(dir: &Path, depth: usize) -> Option<PathBuf> {
        if depth > 3 {
            return None;
        }
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let p = entry.path();
            if p.is_file() && entry.file_name() == WHISPER_EXE {
                return Some(p);
            }
            if p.is_dir() {
                if let Some(found) = walk(&p, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(&bin, 0)
}

pub fn load(app: &AppHandle) -> DictationState {
    let file: DictationFile = store_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();
    DictationState {
        data: Mutex::new(file),
        session: Mutex::new(None),
        transcribing: Mutex::new(false),
    }
}

fn save(app: &AppHandle) -> Result<(), String> {
    let state = app.try_state::<DictationState>().ok_or("dictation state missing")?;
    let data = state.data.lock().unwrap();
    let json = serde_json::to_string_pretty(&*data).map_err(|e| e.to_string())?;
    let path = store_path(app)?;
    fs::write(&path, json).map_err(|e| format!("Couldn't save dictation data: {e}"))
}

fn with_data<T>(app: &AppHandle, f: impl FnOnce(&mut DictationFile) -> T) -> Result<T, String> {
    let state = app
        .try_state::<DictationState>()
        .ok_or("dictation state missing")?;
    let mut data = state.data.lock().unwrap();
    Ok(f(&mut data))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Status + settings commands
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationStatus {
    pub ready: bool,
    pub binary_installed: bool,
    pub model: String,
    pub model_installed: bool,
    pub recording: bool,
    pub transcribing: bool,
    pub hotkey: String,
}

fn status(app: &AppHandle) -> DictationStatus {
    let (model, hotkey) = with_data(app, |d| (d.model.clone(), d.hotkey.clone()))
        .unwrap_or_else(|_| (default_model(), default_hotkey()));
    let model_installed = model_path(app, &model).map(|p| p.exists()).unwrap_or(false);
    let binary_installed = binary_path(app).is_some();
    let state = app.try_state::<DictationState>();
    let recording = state
        .as_ref()
        .map(|s| s.session.lock().unwrap().is_some())
        .unwrap_or(false);
    let transcribing = state
        .as_ref()
        .map(|s| *s.transcribing.lock().unwrap())
        .unwrap_or(false);
    DictationStatus {
        ready: binary_installed && model_installed,
        binary_installed,
        model,
        model_installed,
        recording,
        transcribing,
        hotkey,
    }
}

fn emit_state(app: &AppHandle) {
    let _ = app.emit("dictation-state", status(app));
}

#[tauri::command]
pub fn get_dictation_status(app: AppHandle) -> Result<DictationStatus, String> {
    Ok(status(&app))
}

#[tauri::command]
pub fn list_dictation_transcripts(app: AppHandle) -> Result<Vec<Transcript>, String> {
    with_data(&app, |d| d.transcripts.clone())
}

#[tauri::command]
pub fn clear_dictation_transcripts(app: AppHandle) -> Result<(), String> {
    with_data(&app, |d| d.transcripts.clear())?;
    save(&app)
}

/// Clicking a transcript in the popup: put it back on the OS clipboard,
/// exactly like picking a clipboard entry — Ctrl+V is then the user's.
#[tauri::command]
pub fn copy_dictation_transcript(app: AppHandle, id: String) -> Result<(), String> {
    let text = with_data(&app, |d| {
        d.transcripts.iter().find(|t| t.id == id).map(|t| t.text.clone())
    })?
    .ok_or_else(|| "That transcript is gone.".to_string())?;
    app.clipboard()
        .write_text(text.clone())
        .map_err(|e| format!("Couldn't write to the clipboard: {e}"))?;
    crate::clipboard::note_self_write(&app, &text);
    Ok(())
}

#[tauri::command]
pub fn set_dictation_model(app: AppHandle, model: String) -> Result<DictationStatus, String> {
    if model != "tiny" && model != "base" {
        return Err("Unknown speech model.".to_string());
    }
    with_data(&app, |d| d.model = model)?;
    save(&app)?;
    emit_state(&app);
    Ok(status(&app))
}

#[tauri::command]
pub fn set_dictation_hotkey(app: AppHandle, hotkey: String) -> Result<BindingStatus, String> {
    let hotkey = hotkey.trim().to_string();
    if hotkey.is_empty() {
        let _ = hotkeys::remove_binding(&app, BINDING_ID);
    } else {
        hotkeys::set_binding(&app, BINDING_ID, HotkeyKind::Dictation, &hotkey)?;
    }
    with_data(&app, |d| d.hotkey = hotkey.clone())?;
    save(&app)?;
    Ok(hotkeys::binding_status(&app, BINDING_ID).unwrap_or(BindingStatus {
        hotkey,
        registered: false,
        error: None,
    }))
}

#[tauri::command]
pub fn dictation_hotkey_status(app: AppHandle) -> Result<Option<BindingStatus>, String> {
    Ok(hotkeys::binding_status(&app, BINDING_ID))
}

/// Register the saved hotkey at startup. Best-effort, like the
/// clipboard's: failures land in the binding status for the UI to show.
pub fn register_saved_hotkey(app: &AppHandle) {
    let hotkey = with_data(app, |d| d.hotkey.clone()).unwrap_or_default();
    let hotkey = hotkey.trim().to_string();
    if hotkey.is_empty() {
        return;
    }
    if let Err(e) = hotkeys::set_binding(app, BINDING_ID, HotkeyKind::Dictation, &hotkey) {
        eprintln!("dictation hotkey: {e}");
        hotkeys::record_binding_failure(app, BINDING_ID, hotkey, e);
    }
}

// ---------------------------------------------------------------------------
// One-time downloads (binary + model). Blocking; call from a thread.
// ---------------------------------------------------------------------------

fn download_to(url: &str, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Couldn't create a folder: {e}"))?;
    }
    let resp = ureq::get(url)
        .call()
        .map_err(|e| format!("Download failed: {e}"))?;
    let mut reader = resp.into_reader();
    let tmp = dest.with_file_name(format!(
        "{}.part",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut file = fs::File::create(&tmp).map_err(|e| format!("Couldn't write the download: {e}"))?;
    std::io::copy(&mut reader, &mut file).map_err(|e| format!("Download failed: {e}"))?;
    drop(file);
    fs::rename(&tmp, dest).map_err(|e| format!("Couldn't finish the download: {e}"))?;
    Ok(())
}

fn ensure_binary(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(p) = binary_path(app) {
        return Ok(p);
    }
    let dir = dictation_dir(app)?;
    let archive = dir.join(WHISPER_ARCHIVE);
    let url = format!(
        "https://github.com/ggml-org/whisper.cpp/releases/download/{WHISPER_RELEASE}/{WHISPER_ARCHIVE}"
    );
    download_to(&url, &archive)?;
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).map_err(|e| format!("Couldn't create the bin folder: {e}"))?;
    // System tar (bsdtar on Windows 10+, GNU tar on Linux) reads both
    // .zip and .tar.gz — no archive crates needed.
    let tar = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&bin_dir)
        .output();
    let _ = fs::remove_file(&archive);
    // tar's exit code is not the verdict — some environments warn on
    // file ownership while extracting fine. The binary being present
    // is the verdict.
    match binary_path(app) {
        Some(p) => Ok(p),
        None => match tar {
            Ok(out) => Err(format!(
                "Unpacking the speech engine failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Err(format!("Couldn't run tar to unpack the speech engine: {e}")),
        },
    }
}

fn ensure_model(app: &AppHandle, model: &str) -> Result<PathBuf, String> {
    let path = model_path(app, model)?;
    if path.exists() {
        return Ok(path);
    }
    let url = format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
        model_file_name(model)
    );
    download_to(&url, &path)?;
    Ok(path)
}

/// Settings button: download whatever is missing (engine and/or the
/// selected model). Runs on a worker thread; resolves when ready.
#[tauri::command]
pub async fn download_dictation(app: AppHandle) -> Result<DictationStatus, String> {
    let model = with_data(&app, |d| d.model.clone())?;
    let app2 = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        ensure_binary(&app2)?;
        ensure_model(&app2, &model)?;
        Ok::<(), String>(())
    })
    .await;
    match result {
        Ok(Ok(())) => {
            emit_state(&app);
            Ok(status(&app))
        }
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("Download task failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Recording + transcription
// ---------------------------------------------------------------------------

/// Pure: whisper-cli stdout → the sentence the user said. Non-speech
/// markers whisper prints for silence ("[BLANK_AUDIO]" etc.) become "".
pub fn clean_transcript(raw: &str) -> String {
    let joined = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let t = joined.trim().to_string();
    let lower = t.to_lowercase();
    if matches!(
        lower.as_str(),
        "[blank_audio]" | "(silence)" | "[silence]" | "(blank audio)" | "[inaudible]" | "(inaudible)"
    ) {
        return String::new();
    }
    t
}

/// Pure: linear-interpolation resample (dictation-grade; whisper is
/// robust to far worse). Same-rate input is returned as-is.
pub fn resample(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = ((samples.len() as f64) / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        let a = samples[idx.min(samples.len() - 1)];
        let b = samples[(idx + 1).min(samples.len() - 1)];
        out.push(a + (b - a) * frac);
    }
    out
}

/// Pure: 16-bit PCM WAV bytes for mono samples.
pub fn wav_bytes(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(windows)]
fn esc_pressed() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
    unsafe { (GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000) != 0 }
}
#[cfg(not(windows))]
fn esc_pressed() -> bool {
    false
}

/// Windows: synthesize Ctrl+V into the focused window (the pill never
/// takes focus, so focus is still on the user's target).
#[cfg(windows)]
fn send_ctrl_v() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_C, VK_CONTROL,
    };
    let make = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [
        make(VK_CONTROL, false),
        make(VK_C, false),
        make(VK_C, true),
        make(VK_CONTROL, true),
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Hotkey path (main.rs dispatch): fire-and-forget toggle.
pub fn toggle(app: &AppHandle) {
    let _ = toggle_inner(app.clone());
}

#[tauri::command]
pub fn toggle_dictation(app: AppHandle) -> Result<(), String> {
    toggle_inner(app)
}

fn toggle_inner(app: AppHandle) -> Result<(), String> {
    let state = app
        .try_state::<DictationState>()
        .ok_or("dictation state missing")?;
    // Stop path: flip the flag; the recorder thread finalizes.
    {
        let session = state.session.lock().unwrap();
        if let Some(s) = session.as_ref() {
            s.stop.store(true, Ordering::SeqCst);
            return Ok(());
        }
    }
    let st = status(&app);
    if !st.ready {
        let msg = "Dictation isn't set up yet — open Settings → Clipboard → Dictation and download the speech model.";
        crate::errors::record(&app, "dictation", msg, "", true);
        return Err(msg.to_string());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut session = state.session.lock().unwrap();
        *session = Some(Session { stop: stop.clone() });
    }
    emit_state(&app);
    let app2 = app.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("appmaka-dictation".to_string())
        .spawn(move || record_and_transcribe(app2, stop, cancel))
    {
        if let Some(state) = app.try_state::<DictationState>() {
            *state.session.lock().unwrap() = None;
        }
        return Err(format!("Couldn't start dictation: {e}"));
    }
    Ok(())
}

fn finish_session(app: &AppHandle) {
    if let Some(state) = app.try_state::<DictationState>() {
        *state.session.lock().unwrap() = None;
        *state.transcribing.lock().unwrap() = false;
    }
    emit_state(app);
}

fn record_and_transcribe(app: AppHandle, stop: Arc<AtomicBool>, cancel: Arc<AtomicBool>) {
    let result = run_session(&app, &stop, &cancel);
    #[cfg(windows)]
    pill::hide();
    if let Err(e) = result {
        crate::errors::record(&app, "dictation", &e, "", true);
    }
    finish_session(&app);
}

fn run_session(app: &AppHandle, stop: &AtomicBool, cancel: &AtomicBool) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "No microphone was found.".to_string())?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("Couldn't read the microphone: {e}"))?;
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.config();

    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = samples.clone();
    let err_fn = |err| eprintln!("dictation mic error: {err}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| push_mono(&sink, data, channels),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                let converted: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                push_mono(&sink, &converted, channels);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config,
            move |data: &[u16], _| {
                let converted: Vec<f32> =
                    data.iter().map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0).collect();
                push_mono(&sink, &converted, channels);
            },
            err_fn,
            None,
        ),
        other => return Err(format!("Unsupported microphone format: {other:?}")),
    }
    .map_err(|e| format!("Couldn't open the microphone: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("Couldn't start the microphone: {e}"))?;

    #[cfg(windows)]
    pill::show();

    let started = std::time::Instant::now();
    let mut cancelled = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if cancel.load(Ordering::SeqCst) || esc_pressed() {
            cancelled = true;
            break;
        }
        if started.elapsed().as_secs() >= MAX_RECORD_SECS {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    drop(stream);
    if cancelled {
        return Ok(());
    }

    let raw = samples.lock().unwrap().clone();
    // Under ~0.3 s of audio is a mis-tap, not dictation.
    if raw.len() < (sample_rate as usize) / 3 {
        return Ok(());
    }
    let pcm = resample(&raw, sample_rate, 16_000);
    let wav = dictation_dir(app)?.join("last.wav");
    fs::write(&wav, wav_bytes(&pcm, 16_000)).map_err(|e| format!("Couldn't save the recording: {e}"))?;

    if let Some(state) = app.try_state::<DictationState>() {
        *state.transcribing.lock().unwrap() = true;
    }
    emit_state(app);

    let model = with_data(app, |d| d.model.clone())?;
    let bin = ensure_binary(app)?;
    let model_bin = ensure_model(app, &model)?;
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("-m").arg(&model_bin)
        .arg("-f").arg(&wav)
        .arg("-l").arg("en")
        .arg("-nt")
        .arg("-t").arg("4");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .output()
        .map_err(|e| format!("Couldn't run the speech engine: {e}"))?;
    if !out.status.success() {
        return Err("The speech engine failed on that recording.".to_string());
    }
    let text = clean_transcript(&String::from_utf8_lossy(&out.stdout));
    if text.is_empty() {
        crate::errors::record(app, "dictation", "Didn't catch that — try again.", "", true);
        return Ok(());
    }
    deliver(app, &text)
}

fn push_mono(sink: &Arc<Mutex<Vec<f32>>>, data: &[f32], channels: usize) {
    if channels <= 1 {
        if let Ok(mut buf) = sink.lock() {
            buf.extend_from_slice(data);
        }
        return;
    }
    if let Ok(mut buf) = sink.lock() {
        for frame in data.chunks(channels) {
            let sum: f32 = frame.iter().sum();
            buf.push(sum / frame.len() as f32);
        }
    }
}

/// Save the transcript, put it on the clipboard, and paste it into the
/// window the user was in (Windows). The transcript stays on the
/// clipboard afterwards either way — that's part of the ask.
fn deliver(app: &AppHandle, text: &str) -> Result<(), String> {
    let transcript = Transcript {
        id: format!("d{}", now_ms()),
        ts: now_ms(),
        text: text.to_string(),
    };
    with_data(app, |d| {
        d.transcripts.insert(0, transcript);
        d.transcripts.truncate(MAX_TRANSCRIPTS);
    })?;
    save(app)?;
    app.clipboard()
        .write_text(text.to_string())
        .map_err(|e| format!("Couldn't write to the clipboard: {e}"))?;
    crate::clipboard::note_self_write(app, text);
    #[cfg(windows)]
    send_ctrl_v();
    let _ = app.emit("dictation-transcribed", text.to_string());
    Ok(())
}

// ---------------------------------------------------------------------------
// "Listening" pill (Windows): a tiny native overlay that never takes
// focus. It is feedback, not a webview — zero standing cost, destroyed
// when recording stops.
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod pill {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Foundation::{HINSTANCE, RECT};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
        Ellipse, EndPaint, FillRect, GetMonitorInfoW, MonitorFromPoint, SelectObject, SetBkMode,
        SetTextColor, SetWindowRgn, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
        DEFAULT_QUALITY, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_SEMIBOLD, HGDIOBJ,
        MONITORINFO, MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
        GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW, ShowWindow, TranslateMessage,
        CS_HREDRAW, CS_VREDRAW, HCURSOR, HICON, MSG, SW_SHOWNOACTIVATE, WM_CLOSE, WM_DESTROY,
        WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        WS_VISIBLE,
    };

    static PILL_HWND: AtomicIsize = AtomicIsize::new(0);
    const PILL_W: i32 = 380;
    const PILL_H: i32 = 44;

    pub fn show() {
        if PILL_HWND.load(Ordering::SeqCst) != 0 {
            return;
        }
        std::thread::Builder::new()
            .name("appmaka-dictation-pill".to_string())
            .spawn(|| unsafe { run() })
            .ok();
    }

    pub fn hide() {
        let hwnd = PILL_HWND.swap(0, Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }

    unsafe fn run() {
        let hmodule = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: HINSTANCE(hmodule.0),
            lpszClassName: w!("AppMakaDictationPill"),
            hCursor: HCURSOR::default(),
            hIcon: HICON::default(),
            ..Default::default()
        };
        let _ = RegisterClassW(&class);
        // Top-center of the monitor the cursor is on.
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut mi);
        let mon = mi.rcMonitor;
        let x = mon.left + (mon.right - mon.left - PILL_W) / 2;
        let y = mon.top + 14;
        let hwnd = match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("AppMakaDictationPill"),
            w!(""),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            PILL_W,
            PILL_H,
            None,
            None,
            None,
            None,
        ) {
            Ok(h) => h,
            Err(_) => return,
        };
        let rgn = CreateRoundRectRgn(0, 0, PILL_W, PILL_H, 16, 16);
        let _ = SetWindowRgn(hwnd, Some(rgn), true);
        PILL_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> LRESULT {
            match msg {
                WM_PAINT => {
                    paint(hwnd);
                    LRESULT(0)
                }
                WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }));
        result.unwrap_or_else(|_| DefWindowProcW(hwnd, msg, wparam, lparam))
    }

    unsafe fn paint(hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let rect = RECT { left: 0, top: 0, right: PILL_W, bottom: PILL_H };
        let bg = CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x001C1C22));
        FillRect(dc, &rect, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        // Recording dot.
        let dot = CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x003A3AFF));
        let old = SelectObject(dc, HGDIOBJ(dot.0));
        let cy = PILL_H / 2;
        let _ = Ellipse(dc, 16, cy - 5, 26, cy + 5);
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(dot.0));
        // Label.
        let font = CreateFontW(
            15,
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Segoe UI"),
        );
        let old_font = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00F2F2F5));
        let mut text: Vec<u16> = "Listening — hotkey stops, Esc cancels"
            .encode_utf16()
            .collect();
        let mut trect = RECT { left: 34, top: 0, right: PILL_W - 8, bottom: PILL_H };
        DrawTextW(dc, &mut text, &mut trect, DT_SINGLELINE | DT_VCENTER | DT_CENTER);
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = EndPaint(hwnd, &ps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_drops_silence_markers() {
        assert_eq!(clean_transcript("[BLANK_AUDIO]"), "");
        assert_eq!(clean_transcript("  (silence) \n"), "");
        assert_eq!(clean_transcript(""), "");
    }

    #[test]
    fn clean_joins_lines_and_trims() {
        assert_eq!(clean_transcript("  hello there \n world  "), "hello there world");
        assert_eq!(clean_transcript("one line"), "one line");
    }

    #[test]
    fn resample_halves_at_double_rate() {
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = resample(&input, 32_000, 16_000);
        assert_eq!(out.len(), 50);
        assert!((out[1] - 2.0).abs() < 0.001);
        assert_eq!(resample(&input, 16_000, 16_000).len(), 100);
    }

    #[test]
    fn wav_has_valid_header() {
        let bytes = wav_bytes(&[0.0, 0.5, -0.5], 16_000);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(bytes.len(), 44 + 6);
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        assert_eq!(rate, 16_000);
    }
}

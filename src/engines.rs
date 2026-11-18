//! Optional local/cloud translator engines bundled as Python scripts.
//!
//! RustMIC itself never requires Python: the PCM sockets work with any
//! external program. When Python is available, the GUI can start the bundled
//! ASR -> MT -> TTS pipeline and report which optional packages are installed.
//!
//! The scripts are embedded in the binary and extracted to
//! `<cache>/RustMIC/engines/`, so the portable Windows build stays one file.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::{
    self, AsrEngine, EnginePreset, MtEngine, Settings, TtsEngine,
};
use crate::{command, mixer};

const PROBE_PY: &str = include_str!("../tools/engines/probe.py");
const ENGINE_PY: &str = include_str!("../tools/engines/translate_engine.py");

static CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// Result of `tools/engines/probe.py`. Missing fields default to false so an
/// older/partial script cannot break the GUI.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EngineProbe {
    #[serde(default)]
    pub python: String,
    #[serde(default)]
    pub cuda: bool,
    #[serde(default)]
    pub faster_whisper: bool,
    #[serde(default)]
    pub groq: bool,
    #[serde(default)]
    pub argos: bool,
    #[serde(default)]
    pub openai: bool,
    #[serde(default)]
    pub chatterbox: bool,
    #[serde(default)]
    pub xtts: bool,
    #[serde(default)]
    pub kokoro: bool,
    #[serde(default)]
    pub tts_languages: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub error: String,
}

impl EngineProbe {
    pub fn has(&self, engine: &str) -> bool {
        match engine {
            "faster_whisper" => self.faster_whisper,
            "groq" => self.groq,
            "argos" => self.argos,
            "openai" => self.openai,
            "chatterbox" => self.chatterbox,
            "xtts" => self.xtts,
            "kokoro" => self.kokoro,
            _ => false,
        }
    }
}

/// Directory holding the extracted scripts and the engine log.
pub fn engines_dir() -> PathBuf {
    config::cache_dir().join("engines")
}

pub fn log_path() -> PathBuf {
    engines_dir().join("engines.log")
}

/// Write the embedded scripts if they are missing or changed.
pub fn ensure_scripts() -> Result<PathBuf, String> {
    let dir = engines_dir();
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    for (name, source) in [
        ("probe.py", PROBE_PY),
        ("translate_engine.py", ENGINE_PY),
    ] {
        let path = dir.join(name);
        let current = fs::read_to_string(&path).unwrap_or_default();
        if current != source {
            fs::write(&path, source).map_err(|err| err.to_string())?;
        }
    }
    Ok(dir)
}

fn python_program() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

fn child_slot() -> &'static Mutex<Option<Child>> {
    &CHILD
}

/// Is the bundled engine process still alive? Reaps it if it has exited.
pub fn running() -> bool {
    let Ok(mut slot) = child_slot().lock() else {
        return false;
    };
    if let Some(child) = slot.as_mut() {
        match child.try_wait() {
            Ok(None) => return true,
            _ => {
                *slot = None;
            }
        }
    }
    false
}

/// Terminate the bundled engine, if any. Safe to call repeatedly.
pub fn stop() {
    let Ok(mut slot) = child_slot().lock() else {
        return;
    };
    if let Some(child) = slot.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *slot = None;
}

fn log_file() -> Result<File, String> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| err.to_string())
}

fn absolute_sample(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return path.to_string();
    }
    // Relative paths are resolved against the settings directory first, then
    // the process working directory.
    let from_config = config::config_dir().join(candidate);
    if from_config.is_file() {
        return from_config.to_string_lossy().into_owned();
    }
    fs::canonicalize(candidate)
        .map(|full| full.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Start (or restart) the bundled engine with the current settings.
///
/// Returns Ok even when Python or a package is missing: the error ends up in
/// `engines.log` and `probe()` reports it to the GUI.
pub fn start(settings: &Settings) -> Result<(), String> {
    stop();
    if !settings.translate_enabled {
        return Ok(());
    }
    let dir = ensure_scripts()?;
    let (voice, translate) = mixer::endpoints();
    let mut cmd: Command = command(python_program());
    cmd.arg(dir.join("translate_engine.py"));
    cmd.env("RUSTMIC_ASR", settings.asr_engine.as_str())
        .env("RUSTMIC_MT", settings.mt_engine.as_str())
        .env("RUSTMIC_TTS", settings.tts_engine.as_str())
        .env("RUSTMIC_FROM", &settings.translate_from)
        .env("RUSTMIC_TO", &settings.translate_to)
        .env("RUSTMIC_GROQ_KEY", &settings.groq_api_key)
        .env("RUSTMIC_GROQ_MODEL", &settings.groq_model)
        .env("RUSTMIC_OPENAI_BASE", &settings.openai_base_url)
        .env("RUSTMIC_OPENAI_KEY", &settings.openai_api_key)
        .env("RUSTMIC_OPENAI_MODEL", &settings.openai_model)
        .env("RUSTMIC_VOICE_SAMPLE", absolute_sample(&settings.voice_sample))
        .env("RUSTMIC_CACHE", config::cache_dir().to_string_lossy().as_ref())
        .env("RUSTMIC_VOICE_SOCK", voice)
        .env("RUSTMIC_TRANSLATE_SOCK", translate);
    cmd.stdin(Stdio::null());
    let log = log_file()?;
    cmd.stdout(Stdio::from(
        log.try_clone().map_err(|err| err.to_string())?,
    ))
    .stderr(Stdio::from(log));
    let child = cmd.spawn().map_err(|err| {
        format!(
            "не удалось запустить {}: {}; установите Python 3",
            python_program(),
            err
        )
    })?;
    *child_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(child);
    Ok(())
}

/// Restart only when the engine should be running.
pub fn restart(settings: &Settings) -> Result<(), String> {
    start(settings)
}

/// Run the probe script and parse its single JSON line.
pub fn probe() -> EngineProbe {
    let mut result = EngineProbe::default();
    let dir = match ensure_scripts() {
        Ok(dir) => dir,
        Err(err) => {
            result.error = err;
            return result;
        }
    };
    match command(python_program())
        .arg(dir.join("probe.py"))
        .output()
    {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            match serde_json::from_str::<EngineProbe>(text.trim()) {
                Ok(parsed) => parsed,
                Err(err) => {
                    result.error = format!(
                        "{}: {} (stderr: {})",
                        err,
                        text.trim(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                    result
                }
            }
        }
        Err(err) => {
            result.error = format!("Python 3 не найден: {err}");
            result
        }
    }
}

/// Does the selected TTS support the target language?
pub fn language_supported(tts: TtsEngine, lang: &str, probe_result: &EngineProbe) -> bool {
    let key = tts.as_str();
    let lang = lang.trim().to_lowercase();
    if lang.is_empty() {
        return false;
    }
    match probe_result.tts_languages.get(key) {
        Some(list) => list.iter().any(|item| item.eq_ignore_ascii_case(&lang)),
        // If the probe did not run, do not block the user — the engine itself
        // will report an error in engines.log.
        None => true,
    }
}

/// Engines selected by a one-click preset. Without CUDA we never default to a
/// cloning TTS: it technically runs on CPU but is far too slow for live use.
pub fn preset_engines(
    preset: EnginePreset,
    cuda: bool,
) -> (AsrEngine, MtEngine, TtsEngine) {
    match preset {
        EnginePreset::FreeVoice => (
            AsrEngine::Groq,
            MtEngine::Argos,
            if cuda { TtsEngine::Chatterbox } else { TtsEngine::Kokoro },
        ),
        EnginePreset::Light | EnginePreset::Offline => (
            AsrEngine::FasterWhisper,
            MtEngine::Argos,
            TtsEngine::Kokoro,
        ),
        EnginePreset::Cloud => (
            AsrEngine::Groq,
            MtEngine::OpenAi,
            TtsEngine::Kokoro,
        ),
        EnginePreset::Custom => (
            AsrEngine::FasterWhisper,
            MtEngine::Argos,
            TtsEngine::Kokoro,
        ),
    }
}

/// Human-readable hint shown next to a preset before it is applied.
pub fn preset_hint(preset: EnginePreset, probe_result: &EngineProbe) -> String {
    let (asr, mt, tts) = preset_engines(preset, probe_result.cuda);
    let mut parts = vec![
        format!("ASR: {}", asr.as_str()),
        format!("MT: {}", mt.as_str()),
        format!("TTS: {}", tts.as_str()),
    ];
    if preset == EnginePreset::FreeVoice && !probe_result.cuda {
        parts.push("клонирование требует CUDA — выбран Kokoro".to_string());
    }
    if tts.clones_voice() {
        parts.push("нужен образец голоса".to_string());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_avoid_cloning_without_cuda() {
        let (_, _, tts) = preset_engines(EnginePreset::FreeVoice, false);
        assert_eq!(tts, TtsEngine::Kokoro);
        let (_, _, tts) = preset_engines(EnginePreset::FreeVoice, true);
        assert_eq!(tts, TtsEngine::Chatterbox);
    }

    #[test]
    fn language_support_uses_probe_table() {
        let mut probe_result = EngineProbe::default();
        probe_result.tts_languages.insert(
            "kokoro".to_string(),
            vec!["en".to_string(), "de".to_string()],
        );
        assert!(language_supported(TtsEngine::Kokoro, "de", &probe_result));
        assert!(!language_supported(TtsEngine::Kokoro, "ru", &probe_result));
    }

    #[test]
    fn scripts_are_embedded() {
        assert!(PROBE_PY.contains("tts_languages"));
        assert!(ENGINE_PY.contains("RUSTMIC_VOICE_SOCK"));
    }
}

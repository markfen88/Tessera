//! Compare CamillaDSP's ALSA capture rate with the mapped Tessera device.
//! Only the documented PCMs and readable block YAML are checked. Edits use
//! the boost editor's line reader, preserving every byte outside one scalar
//! or the new `capture_samplerate` line.
//!
//! Only the `devices` block is read. The rest of the file may use either
//! sequence style: the indented one, and the indentless one (`pipeline:` then
//! `- type: Filter` at the key's own column) that CamillaDSP's examples,
//! CamillaGUI and the packaged `Speaker.yml` write.

use std::sync::{Mutex, PoisonError};

use serde_json::Value;
use tessera_control_types::{DeviceStatus, Status};

use super::camilla::{
    Active, content, has_block, key_len, lines, replace_scalar, unquote, value_span,
};

/// The result of checking the active configuration against daemon status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRate {
    /// The capture and device rates agree.
    Ok,
    /// No reliable comparison is possible; logged once, never a warning card.
    Unchecked(String),
    /// The PCM cannot be opened at this capture rate.
    Mismatch {
        /// Friendly name of the Tessera device mapped to the capture PCM.
        device_name: String,
        /// Current device sample rate in Hz, required by the capture PCM.
        device_rate: u32,
        /// Effective capture sample rate in Hz from the EQ configuration.
        capture_rate: u32,
        /// Capture inherits `samplerate` instead of setting its own rate.
        from_samplerate: bool,
        /// EQ processing sample rate in Hz.
        samplerate: u32,
        /// Updating only the capture rate preserves a valid processing rate.
        fixable: bool,
        /// Recognised ALSA capture-rate failure reported by the EQ engine.
        stop_reason: Option<String>,
    },
}

/// Read the same active file the page resolved, using its current daemon
/// status. Every page render calls this, including after a device-rate change.
pub async fn read(
    active: &Result<Active, String>,
    status: &Result<Status, String>,
    stop_reason: Option<&str>,
) -> CaptureRate {
    let result = inspect(active, status, stop_reason).await;
    if first_report(&REPORTED, &result) {
        if let CaptureRate::Unchecked(reason) = &result {
            tracing::info!(reason, "EQ capture rate cannot be checked");
        }
    }
    result
}

/// The reason last logged. The Speaker page re-checks on every refresh, so a
/// reason is logged when it first appears and again only after the check
/// worked in between or the reason changed.
static REPORTED: Mutex<Option<String>> = Mutex::new(None);

/// Is `result` an unchecked reason that `reported` does not hold yet? Any
/// other result clears `reported`.
fn first_report(reported: &Mutex<Option<String>>, result: &CaptureRate) -> bool {
    let mut last = reported.lock().unwrap_or_else(PoisonError::into_inner);
    match result {
        CaptureRate::Unchecked(reason) if last.as_deref() != Some(reason.as_str()) => {
            *last = Some(reason.clone());
            true
        }
        CaptureRate::Unchecked(_) => false,
        _ => {
            *last = None;
            false
        }
    }
}

async fn inspect(
    active: &Result<Active, String>,
    status: &Result<Status, String>,
    stop_reason: Option<&str>,
) -> CaptureRate {
    let status = match status {
        Ok(s) => s,
        Err(e) => return CaptureRate::Unchecked(format!("daemon status unavailable: {e}")),
    };
    let path = match active {
        Ok(Active { path: Some(p), .. }) => p,
        Ok(_) => return CaptureRate::Unchecked("no active EQ configuration".into()),
        Err(e) => return CaptureRate::Unchecked(e.clone()),
    };
    match tokio::fs::read_to_string(path).await {
        Ok(yaml) => check(&yaml, &status.devices, stop_reason),
        Err(e) => CaptureRate::Unchecked(format!("cannot read {}: {e}", path.display())),
    }
}

/// Recognise the rate-setting capture failure in string and tagged replies.
/// Other stop reasons (including playback failures) do not diagnose capture.
#[must_use]
pub fn stop_reason(value: &Value) -> Option<String> {
    let reason = value
        .as_str()
        .or_else(|| value.get("CaptureError").and_then(Value::as_str))?;
    (reason.contains("snd_pcm_hw_params_set_rate") && reason.contains("Invalid argument"))
        .then(|| reason.to_owned())
}

/// An ALSA format the engine's playback or capture device refused, as the
/// engine reports it (`snd_pcm_hw_params_set_format … Invalid argument`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatFailure {
    /// The playback side refused the format (else the capture side).
    pub playback: bool,
    /// The engine's own words.
    pub reason: String,
}

/// Recognise the format-setting failure in string and tagged replies. A
/// tagged reply names the side; a plain string is taken for playback when it
/// says so. Other stop reasons do not diagnose the format.
#[must_use]
pub fn format_failure(value: &Value) -> Option<FormatFailure> {
    let (playback, reason) = if let Some(reason) = value.as_str() {
        (reason.to_ascii_lowercase().contains("playback"), reason)
    } else if let Some(reason) = value.get("PlaybackError").and_then(Value::as_str) {
        (true, reason)
    } else {
        (false, value.get("CaptureError").and_then(Value::as_str)?)
    };
    (reason.contains("snd_pcm_hw_params_set_format") && reason.contains("Invalid argument")).then(
        || FormatFailure {
            playback,
            reason: reason.to_owned(),
        },
    )
}

/// What the Speaker page shows of a format failure: the engine's words and the
/// device and format the active configuration asks for, when it can be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatNotice {
    /// The failure.
    pub failure: FormatFailure,
    /// The PCM the engine opens on the failed side.
    pub device: Option<String>,
    /// The format the configuration asks of it.
    pub format: Option<String>,
}

/// The notice for `failure` with the failed side's `device` and `format` read
/// from the active configuration. A configuration that cannot be read, or has
/// no such keys, still gives the notice, without them. No failure: no notice.
pub async fn format_notice(
    active: &Result<Active, String>,
    failure: Option<&FormatFailure>,
) -> Option<FormatNotice> {
    let failure = failure?;
    let yaml = match active {
        Ok(Active { path: Some(p), .. }) => tokio::fs::read_to_string(p).await.ok(),
        _ => None,
    };
    let (device, format) = yaml
        .as_deref()
        .map(|yaml| configured(yaml, failure.playback))
        .unwrap_or_default();
    Some(FormatNotice {
        failure: failure.clone(),
        device,
        format,
    })
}

/// `devices.<side>.device` and `.format` of readable block YAML.
fn configured(yaml: &str, playback: bool) -> (Option<String>, Option<String>) {
    let side = if playback { "playback" } else { "capture" };
    let read = || -> Result<(Option<String>, Option<String>), String> {
        let ls = lines(yaml);
        let root = children(&ls, None)?;
        let devices = block(&ls, required(&root, "devices")?, "devices")?;
        let fields = block(&ls, required(&devices, side)?, side)?;
        let get = |key: &str| {
            child(&fields, key)
                .map(|i| string(raw(&ls, i, key)?))
                .transpose()
        };
        Ok((get("device")?, get("format")?))
    };
    read().unwrap_or_default()
}

/// Check readable block-style device settings, without guessing a PCM's
/// device key from an arbitrary name.
#[must_use]
pub fn check(yaml: &str, devices: &[DeviceStatus], stop_reason: Option<&str>) -> CaptureRate {
    match Settings::read(yaml) {
        Ok(settings) => settings.check(devices, stop_reason),
        Err(e) => CaptureRate::Unchecked(e),
    }
}

#[derive(Debug)]
struct Settings {
    samplerate: u32,
    capture_samplerate: Option<u32>,
    resampler: bool,
    capture_type: String,
    pcm: String,
    samplerate_line: usize,
    capture_line: Option<usize>,
}

impl Settings {
    fn read(yaml: &str) -> Result<Self, String> {
        let ls = lines(yaml);
        let root = children(&ls, None)?;
        let devices = required(&root, "devices")?;
        let fields = block(&ls, devices, "devices")?;
        validate_block(&ls, &fields)?;
        let samplerate_line = required(&fields, "samplerate")?;
        let samplerate = rate(raw(&ls, samplerate_line, "samplerate")?)?;
        let capture_line = child(&fields, "capture_samplerate");
        let capture_samplerate = capture_line
            .map(|i| {
                let v = raw(&ls, i, "capture_samplerate")?;
                if null(v) { Ok(None) } else { rate(v).map(Some) }
            })
            .transpose()?
            .flatten();
        let resampler = child(&fields, "resampler")
            .map(|i| {
                let v = raw(&ls, i, "resampler")?;
                Ok::<bool, String>(has_block(&ls, i) || !null(v))
            })
            .transpose()?
            .unwrap_or(false);
        let capture = required(&fields, "capture")?;
        let capture = block(&ls, capture, "capture")?;
        let capture_type = string(raw(&ls, required(&capture, "type")?, "type")?)?;
        let pcm = string(raw(&ls, required(&capture, "device")?, "device")?)?;
        Ok(Self {
            samplerate,
            capture_samplerate,
            resampler,
            capture_type,
            pcm,
            samplerate_line,
            capture_line,
        })
    }

    fn check(&self, devices: &[DeviceStatus], stop_reason: Option<&str>) -> CaptureRate {
        if self.capture_type != "Alsa" {
            return CaptureRate::Unchecked("capture is not ALSA".into());
        }
        // These are the definitions in docs/audio-backends.md and its linked
        // full sample. Unknown custom PCMs need their own mapping, not a
        // prefix-based guess that they use Tessera.
        let pcm = self.pcm.strip_prefix("plug:").unwrap_or(&self.pcm);
        let key = match pcm {
            "tessera" | "tessera_plug" | "tessera_out_34" => None,
            "tessera_main_rx" => Some("main"),
            _ => {
                return CaptureRate::Unchecked(
                    "capture PCM is not a documented Tessera PCM".into(),
                );
            }
        };
        let device = key
            .and_then(|key| devices.iter().find(|d| d.key.0 == key))
            .or_else(|| devices.first());
        let Some(device) = device else {
            return CaptureRate::Unchecked("daemon reports no Tessera devices".into());
        };
        let device_rate = device.sample_rate.0;
        if device_rate == 0 {
            return CaptureRate::Unchecked("daemon reports no device sample rate".into());
        }
        let capture_rate = self.capture_samplerate.unwrap_or(self.samplerate);
        if capture_rate == device_rate {
            return CaptureRate::Ok;
        }
        CaptureRate::Mismatch {
            device_name: device.name.clone(),
            device_rate,
            capture_rate,
            from_samplerate: self.capture_samplerate.is_none(),
            samplerate: self.samplerate,
            fixable: self.resampler || self.samplerate == device_rate,
            stop_reason: stop_reason.map(str::to_owned),
        }
    }
}

/// Re-check before an edit, refusing a stale or unsafe fix.
pub(super) fn prepare(yaml: &str, devices: &[DeviceStatus]) -> Result<(String, u32), String> {
    match check(yaml, devices, None) {
        CaptureRate::Mismatch {
            device_rate,
            fixable: true,
            ..
        } => Ok((set_capture_samplerate(yaml, device_rate)?, device_rate)),
        CaptureRate::Mismatch { .. } => Err("The active EQ configuration has no resampler and processes at a different rate. Set samplerate to the device's rate (check coefficient files), or add an AsyncSinc resampler before fixing the capture rate.".into()),
        CaptureRate::Ok => Err("The active EQ configuration no longer has a capture-rate mismatch. Reload the Speaker page.".into()),
        CaptureRate::Unchecked(reason) => Err(format!(
            "The capture-rate fix no longer applies: {reason}. Reload the Speaker page."
        )),
    }
}

/// Replace only the capture scalar, or insert it directly after `samplerate`
/// with the same indentation and line ending.
///
/// # Errors
/// The device settings cannot be read safely as block YAML, or rate is zero.
pub fn set_capture_samplerate(yaml: &str, rate: u32) -> Result<String, String> {
    if rate == 0 {
        return Err("capture rate must be positive".into());
    }
    let settings = Settings::read(yaml)?;
    let ls = lines(yaml);
    if let Some(i) = settings.capture_line {
        return Ok(replace_scalar(
            yaml,
            &ls,
            i,
            "capture_samplerate",
            &rate.to_string(),
        ));
    }
    let (offset, line) = ls[settings.samplerate_line];
    let (indent, _) = content(line).ok_or("missing samplerate line")?;
    let end = ls
        .get(settings.samplerate_line + 1)
        .map_or(yaml.len(), |(offset, _)| *offset);
    let newline = if yaml[offset..end].ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut out = String::with_capacity(yaml.len() + 100);
    out.push_str(&yaml[..end]);
    if !out.ends_with('\n') {
        out.push_str(newline);
    }
    out.push_str(&line[..indent]);
    out.push_str(&format!(
        "capture_samplerate: {rate} # must equal the tessera device's sample_rate{newline}"
    ));
    out.push_str(&yaml[end..]);
    Ok(out)
}

// The existing reader supplies the line offsets and scalar spans. These
// stricter wrappers refuse ambiguous structures before using them for a rate
// diagnosis: duplicate/merged keys, aliases, flow style and multiline scalars.
fn children(ls: &[(usize, &str)], parent: Option<usize>) -> Result<Vec<(String, usize)>, String> {
    let parent_indent = parent.and_then(|i| content(ls[i].1)).map(|(i, _)| i);
    let mut indent = parent_indent.is_none().then_some(0);
    let mut fields: Vec<(String, usize)> = Vec::new();
    let mut started = false;
    let mut ended = false;
    for (i, (_, line)) in ls.iter().enumerate().skip(parent.map_or(0, |i| i + 1)) {
        let Some((n, t)) = content(line) else {
            continue;
        };
        if parent_indent.is_some_and(|p| n <= p) {
            break;
        }
        if line[..n].contains('\t') {
            return Err("tab indentation in EQ devices".into());
        }
        if parent.is_none() && n == 0 && (t == "---" || t == "...") {
            if t == "---" && (started || ended || !fields.is_empty()) {
                return Err("multiple YAML documents".into());
            }
            started = true;
            ended = t == "...";
            continue;
        }
        if ended {
            return Err("content after the YAML document end".into());
        }
        let direct = *indent.get_or_insert(n);
        if n < direct {
            return Err("inconsistent EQ device indentation".into());
        }
        if n != direct {
            continue;
        }
        // An indentless sequence belongs to the key before it (`pipeline:`
        // and its `- type: Filter`). Only the top level is read around such
        // keys; inside `devices` a sequence is not understood.
        if parent.is_none() && (t == "-" || t.starts_with("- ") || t.starts_with("-\t")) {
            continue;
        }
        let key = t.split_once(':').map(|(k, _)| k).unwrap_or("");
        let key = string(key)?;
        if key.is_empty() || key == "<<" || key_len(t, &key).is_none() {
            return Err("unreadable or merged EQ device mapping".into());
        }
        if fields.iter().any(|(k, _)| k == &key) {
            return Err(format!("duplicate EQ configuration key `{key}`"));
        }
        fields.push((key, i));
    }
    Ok(fields)
}

fn child(fields: &[(String, usize)], key: &str) -> Option<usize> {
    fields.iter().find(|(k, _)| k == key).map(|(_, i)| *i)
}

fn required(fields: &[(String, usize)], key: &str) -> Result<usize, String> {
    child(fields, key).ok_or_else(|| format!("no `{key}` in EQ device settings"))
}

fn raw<'a>(ls: &[(usize, &'a str)], i: usize, key: &str) -> Result<&'a str, String> {
    let (_, t) = content(ls[i].1).ok_or("missing EQ device field")?;
    let k = key_len(t, key).ok_or("unreadable EQ device field")?;
    let rest = &t[k..];
    let (start, end) = value_span(rest);
    let tail = rest[end..].trim_start();
    if !tail.is_empty() && !tail.starts_with('#') {
        return Err("unreadable text after EQ device scalar".into());
    }
    Ok(&rest[start..end])
}

fn block(ls: &[(usize, &str)], i: usize, key: &str) -> Result<Vec<(String, usize)>, String> {
    if !raw(ls, i, key)?.is_empty() || !has_block(ls, i) {
        return Err(format!("`{key}` is not a readable block mapping"));
    }
    children(ls, Some(i))
}

fn validate_block(ls: &[(usize, &str)], fields: &[(String, usize)]) -> Result<(), String> {
    for (key, i) in fields {
        if has_block(ls, *i) {
            validate_block(ls, &block(ls, *i, key)?)?;
        } else {
            string(raw(ls, *i, key)?)?;
        }
    }
    Ok(())
}

fn string(raw: &str) -> Result<String, String> {
    if raw.starts_with('"') {
        return serde_json::from_str(raw).map_err(|_| "unreadable quoted EQ scalar".into());
    }
    if raw.starts_with('\'') {
        let inner = raw
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .ok_or("unclosed quoted EQ scalar")?;
        if inner.replace("''", "").contains('\'') {
            return Err("unreadable quoted EQ scalar".into());
        }
        return Ok(unquote(raw));
    }
    if raw.starts_with(['{', '[', '&', '*', '!', '|', '>', '@', '%', '`', '?'])
        || raw.starts_with("- ")
        || raw.contains(": ")
        || raw.contains(":\t")
    {
        return Err("flow style, aliases or unsupported EQ YAML".into());
    }
    Ok(raw.to_owned())
}

fn null(raw: &str) -> bool {
    matches!(raw, "" | "null" | "Null" | "NULL" | "~")
}

fn rate(raw: &str) -> Result<u32, String> {
    string(raw)?
        .parse::<u32>()
        .ok()
        .filter(|r| *r > 0)
        .ok_or_else(|| "EQ sample rate is not a positive integer".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appliance::camilla::Source;
    use tessera_control_types::{DeviceKey, Encoding, LatencyUs, SampleRate};

    const CONFIG: &str = "---
devices:
  samplerate: 96000   # processing
  chunksize: 1024
  resampler:
    type: AsyncSinc
  capture:
    type: Alsa
    device: \"tessera\" # network input
    channels: 2
  playback:
    type: Alsa
    device: hw:CARD=DAC,DEV=0
filters:
  dashboard_signal_boost:
    type: Gain
    parameters:
      gain: 14 # leave this alone
pipeline: []
";

    fn device(key: &str, name: &str, rate: u32) -> DeviceStatus {
        DeviceStatus::new(
            DeviceKey(key.into()),
            name,
            SampleRate(rate),
            Encoding(24),
            LatencyUs(10_000),
        )
    }

    fn mismatch(from_samplerate: bool, samplerate: u32, fixable: bool) -> CaptureRate {
        CaptureRate::Mismatch {
            device_name: "Kitchen".into(),
            device_rate: 48_000,
            capture_rate: 96_000,
            from_samplerate,
            samplerate,
            fixable,
            stop_reason: None,
        }
    }

    #[test]
    fn detection_table_uses_the_effective_rate_and_safe_fix_rules() {
        let no_resampler = CONFIG.replace("  resampler:\n    type: AsyncSinc\n", "");
        let cases = [
            (
                "samplerate matches",
                CONFIG.replace("96000", "48000"),
                CaptureRate::Ok,
            ),
            (
                "samplerate mismatch with resampler",
                CONFIG.into(),
                mismatch(true, 96_000, true),
            ),
            (
                "samplerate mismatch without resampler",
                no_resampler.clone(),
                mismatch(true, 96_000, false),
            ),
            (
                "capture rate matches",
                CONFIG.replace("  chunksize:", "  capture_samplerate: 48000\n  chunksize:"),
                CaptureRate::Ok,
            ),
            (
                "capture rate mismatch",
                CONFIG.replace("  chunksize:", "  capture_samplerate: 96000\n  chunksize:"),
                mismatch(false, 96_000, true),
            ),
            (
                "processing already matches without resampler",
                no_resampler
                    .replace("96000", "48000")
                    .replace("  chunksize:", "  capture_samplerate: 96000\n  chunksize:"),
                mismatch(false, 48_000, true),
            ),
            (
                "null capture rate uses samplerate",
                CONFIG.replace("  chunksize:", "  capture_samplerate: null\n  chunksize:"),
                mismatch(true, 96_000, true),
            ),
            (
                "null resampler cannot fix processing",
                CONFIG.replace("  resampler:\n    type: AsyncSinc", "  resampler: null"),
                mismatch(true, 96_000, false),
            ),
            (
                "unset resampler cannot fix processing",
                CONFIG.replace("  resampler:\n    type: AsyncSinc", "  resampler:"),
                mismatch(true, 96_000, false),
            ),
            (
                "quoted scalars",
                CONFIG
                    .replace("samplerate: 96000", "'samplerate': '96000'")
                    .replace("type: Alsa", "type: \"Alsa\""),
                mismatch(true, 96_000, true),
            ),
        ];
        let devices = [device("main", "Kitchen", 48_000)];
        for (name, yaml, expected) in cases {
            assert_eq!(check(&yaml, &devices, None), expected, "{name}");
        }
        // A new status snapshot rechecks the same file after a rate change.
        assert_eq!(
            check(CONFIG, &[device("main", "Kitchen", 96_000)], None),
            CaptureRate::Ok
        );
    }

    #[test]
    fn unknown_or_unreadable_inputs_are_unchecked() {
        let cases = [
            CONFIG.replace("\"tessera\"", "hw:Loopback,1,0"),
            CONFIG.replace("\"tessera\"", "tessera_unknown"),
            CONFIG.replace("type: Alsa", "type: File"),
            "devices: {samplerate: 96000, capture: {type: Alsa, device: tessera}}\n".into(),
            CONFIG.replace(
                "  capture:\n    type: Alsa\n    device: \"tessera\" # network input\n    channels: 2",
                "  capture: {type: Alsa, device: tessera}",
            ),
            CONFIG.replace("  resampler:\n    type: AsyncSinc", "  resampler: {type: AsyncSinc}"),
            CONFIG.replace("samplerate: 96000", "samplerate: fast"),
            CONFIG.replace("samplerate: 96000", "samplerate: 0"),
            CONFIG.replace("samplerate: 96000", "samplerate: *rate"),
            CONFIG.replace("samplerate: 96000", "samplerate: !tag 96000"),
            CONFIG.replace("samplerate: 96000", "samplerate: \"96000"),
            CONFIG.replace("samplerate: 96000", "samplerate: '96000' junk"),
            CONFIG.replace("  chunksize:", "  samplerate: 48000\n  chunksize:"),
            CONFIG.replace("  chunksize:", "  <<: *defaults\n  chunksize:"),
            CONFIG.replace("  chunksize:", "    unexpected: 1\n  chunksize:"),
            CONFIG.replace("  chunksize:", "  capture_samplerate: invalid\n  chunksize:"),
            CONFIG.replace("  chunksize:", "\tchunksize:"),
            format!("{CONFIG}---\ndevices:\n  samplerate: 48000\n"),
            format!("---\n{CONFIG}"),
            format!("{CONFIG}...\nnot_yaml\n"),
            "".into(),
        ];
        for yaml in cases {
            assert!(
                matches!(
                    check(&yaml, &[device("main", "Kitchen", 48_000)], None),
                    CaptureRate::Unchecked(_)
                ),
                "{yaml}"
            );
        }
        assert!(matches!(
            check(CONFIG, &[], None),
            CaptureRate::Unchecked(_)
        ));
    }

    #[test]
    fn documented_pcms_map_to_the_right_device_or_the_named_first_fallback() {
        let devices = [
            device("other", "Hall", 44_100),
            device("main", "Kitchen", 48_000),
        ];
        for (pcm, name, rate) in [
            ("tessera", "Hall", 44_100),
            ("plug:tessera", "Hall", 44_100),
            ("tessera_plug", "Hall", 44_100),
            ("tessera_main_rx", "Kitchen", 48_000),
            ("plug:tessera_main_rx", "Kitchen", 48_000),
            ("tessera_out_34", "Hall", 44_100),
        ] {
            let yaml = CONFIG.replace("\"tessera\"", &format!("\"{pcm}\""));
            let result = check(&yaml, &devices, None);
            assert!(
                matches!(result, CaptureRate::Mismatch { device_name, device_rate, .. } if device_name == name && device_rate == rate),
                "{pcm}"
            );
        }
        let yaml = CONFIG.replace("\"tessera\"", "tessera_main_rx");
        assert!(
            matches!(check(&yaml, &devices[..1], None), CaptureRate::Mismatch { device_name, device_rate: 44_100, .. } if device_name == "Hall")
        );
    }

    #[tokio::test]
    async fn missing_active_file_is_unchecked_without_an_engine() {
        let active = Ok(Active {
            path: Some(std::env::temp_dir().join(format!(
                "tessera-web-capture-missing-{}/missing.yml",
                std::process::id()
            ))),
            label: Some("missing.yml".into()),
            boost_db: None,
            source: Source::Engine,
        });
        let mut status = Status::new("test");
        status.devices.push(device("main", "Kitchen", 48_000));
        assert!(
            matches!(read(&active, &Ok(status), None).await, CaptureRate::Unchecked(reason) if reason.contains("cannot read"))
        );
    }

    #[test]
    fn insert_keeps_indent_line_endings_and_every_other_byte() {
        let expected = CONFIG.replace(
            "  samplerate: 96000   # processing\n",
            "  samplerate: 96000   # processing\n  capture_samplerate: 48000 # must equal the tessera device's sample_rate\n",
        );
        for (yaml, expected) in [
            (CONFIG.to_owned(), expected.clone()),
            (CONFIG.replace('\n', "\r\n"), expected.replace('\n', "\r\n")),
            (
                CONFIG.replace("\n  ", "\n    "),
                expected.replace("\n  ", "\n    "),
            ),
            (
                CONFIG.trim_end_matches('\n').to_owned(),
                expected.trim_end_matches('\n').to_owned(),
            ),
        ] {
            let out = set_capture_samplerate(&yaml, 48_000).unwrap();
            assert_eq!(out, expected);
            assert_eq!(set_capture_samplerate(&out, 48_000).unwrap(), out);
        }
        // `samplerate` may be the last device field and lack a final newline.
        let yaml = "devices:\n  capture:\n    type: Alsa\n    device: tessera\n  samplerate: 96000";
        assert_eq!(
            set_capture_samplerate(yaml, 48_000).unwrap(),
            format!(
                "{yaml}\n  capture_samplerate: 48000 # must equal the tessera device's sample_rate\n"
            )
        );
    }

    #[test]
    fn replace_keeps_trailing_comments_and_every_other_byte() {
        for scalar in ["96000", "'96000'", "null", ""] {
            let yaml = CONFIG.replace(
                "  chunksize:",
                &format!("  capture_samplerate: {scalar}  # keep this\n  chunksize:"),
            );
            let expected = CONFIG.replace(
                "  chunksize:",
                "  capture_samplerate: 48000  # keep this\n  chunksize:",
            );
            let out = set_capture_samplerate(&yaml, 48_000).unwrap();
            assert_eq!(out, expected);
            assert_eq!(set_capture_samplerate(&out, 48_000).unwrap(), out);
        }
        let yaml = CONFIG
            .replace(
                "  chunksize:",
                "  'capture_samplerate':\t96000  # keep\n  chunksize:",
            )
            .replace('\n', "\r\n");
        assert_eq!(
            set_capture_samplerate(&yaml, 48_000).unwrap(),
            yaml.replace("\t96000", "\t48000")
        );
    }

    #[test]
    fn prepare_refuses_stale_uncheckable_and_unfixable_changes() {
        let devices = [device("main", "Kitchen", 48_000)];
        let (out, rate) = prepare(CONFIG, &devices).unwrap();
        assert_eq!(rate, 48_000);
        assert_eq!(check(&out, &devices, None), CaptureRate::Ok);
        assert!(prepare(&out, &devices).unwrap_err().contains("no longer"));
        assert!(
            prepare(
                &CONFIG.replace("  resampler:\n    type: AsyncSinc\n", ""),
                &devices
            )
            .unwrap_err()
            .contains("no resampler")
        );
        assert!(
            prepare(&CONFIG.replace("\"tessera\"", "hw:Loopback"), &devices)
                .unwrap_err()
                .contains("no longer applies")
        );
    }

    #[test]
    fn recognises_only_the_rate_setting_capture_error() {
        let error = "Capture error: ALSA function 'snd_pcm_hw_params_set_rate' failed with error 'Invalid argument (22)'";
        for value in [
            serde_json::json!(error),
            serde_json::json!({"CaptureError": error}),
        ] {
            assert_eq!(stop_reason(&value).as_deref(), Some(error));
        }
        for value in [
            Value::Null,
            serde_json::json!("None"),
            serde_json::json!({"PlaybackError": error}),
            serde_json::json!({"CaptureError": "snd_pcm_hw_params_set_rate: Device or resource busy"}),
            serde_json::json!({"CaptureError": "Invalid argument (22)"}),
        ] {
            assert_eq!(stop_reason(&value), None);
        }
        assert!(
            matches!(check(CONFIG, &[device("main", "Kitchen", 48_000)], Some(error)), CaptureRate::Mismatch { stop_reason: Some(reason), .. } if reason == error)
        );
    }

    /// The configuration the speaker profile seeds, rendered as the installer does.
    fn packaged_speaker(rate: u32) -> String {
        include_str!("../../../../packaging/dsp/dist/Speaker.yml.in")
            .replace("@RATE@", &rate.to_string())
            .replace("@PLAYBACK_DEVICE@", "hw:CARD=DAC,DEV=0")
            .replace("@PLAYBACK_FORMAT@", "S32_LE")
    }

    /// A configuration as CamillaGUI saves it: keys sorted, and every sequence
    /// indentless, at the top level and inside mappings.
    const GUI_SAVED: &str = "description: Saved by the GUI
devices:
  capture:
    channels: 2
    device: tessera
    format: S32_LE
    type: Alsa
  capture_samplerate: null
  chunksize: 1024
  enable_rate_adjust: true
  playback:
    channels: 2
    device: hw:CARD=DAC,DEV=0
    format: S32_LE
    type: Alsa
  queuelimit: 4
  resampler:
    profile: Balanced
    type: AsyncSinc
  samplerate: 96000
filters:
  Peak:
    parameters:
      freq: 1000.0
      gain: -3.0
      q: 1.5
      type: Peaking
    type: Biquad
mixers:
  Swap:
    channels:
      in: 2
      out: 2
    mapping:
    - dest: 0
      mute: false
      sources:
      - channel: 1
        gain: 0
        inverted: false
        scale: dB
pipeline:
- channels: null
  names:
  - Peak
  type: Filter
- name: Swap
  type: Mixer
title: null
";

    #[test]
    fn the_packaged_speaker_configuration_is_checked_and_fixed() {
        let yaml = packaged_speaker(48_000);
        assert_eq!(
            check(&yaml, &[device("main", "Kitchen", 48_000)], None),
            CaptureRate::Ok
        );
        let devices = [device("main", "Kitchen", 96_000)];
        assert_eq!(
            check(&yaml, &devices, None),
            CaptureRate::Mismatch {
                device_name: "Kitchen".into(),
                device_rate: 96_000,
                capture_rate: 48_000,
                from_samplerate: true,
                samplerate: 48_000,
                fixable: true,
                stop_reason: None,
            }
        );
        // The fix adds one line after `samplerate`; the indentless pipeline,
        // the comments and every other byte stay.
        let (fixed, rate) = prepare(&yaml, &devices).unwrap();
        assert_eq!(rate, 96_000);
        assert_eq!(
            fixed,
            yaml.replace(
                "  samplerate: 48000\n",
                "  samplerate: 48000\n  capture_samplerate: 96000 # must equal the tessera device's sample_rate\n"
            )
        );
        assert_eq!(check(&fixed, &devices, None), CaptureRate::Ok);
        // Other rates the installer seeds read the same way.
        for rate in [44_100, 96_000, 192_000] {
            assert_eq!(
                check(
                    &packaged_speaker(rate),
                    &[device("main", "Kitchen", rate)],
                    None
                ),
                CaptureRate::Ok,
                "{rate}"
            );
        }
    }

    #[test]
    fn a_gui_saved_configuration_with_indentless_sequences_is_checked_and_fixed() {
        let devices = [device("main", "Kitchen", 48_000)];
        assert_eq!(
            check(GUI_SAVED, &devices, None),
            CaptureRate::Mismatch {
                device_name: "Kitchen".into(),
                device_rate: 48_000,
                capture_rate: 96_000,
                from_samplerate: true,
                samplerate: 96_000,
                fixable: true,
                stop_reason: None,
            }
        );
        // `capture_samplerate: null` is replaced in place, nothing else moves.
        let (fixed, rate) = prepare(GUI_SAVED, &devices).unwrap();
        assert_eq!(rate, 48_000);
        assert_eq!(
            fixed,
            GUI_SAVED.replace("capture_samplerate: null", "capture_samplerate: 48000")
        );
        assert_eq!(check(&fixed, &devices, None), CaptureRate::Ok);
    }

    #[test]
    fn indentless_sequences_before_devices_and_bare_entries_are_skipped() {
        let devices = [device("main", "Kitchen", 96_000)];
        let body = CONFIG.trim_start_matches("---\n");
        let (head, tail) = body.split_once("pipeline: []\n").unwrap();
        for pipeline in [
            "pipeline:\n- type: Filter\n  names:\n  - a\n",
            "pipeline:\n-\n  type: Filter\n- type: Mixer\n",
            "pipeline:\n-\ttype: Filter\n",
        ] {
            assert_eq!(
                check(&format!("{pipeline}{head}{tail}"), &devices, None),
                CaptureRate::Ok,
                "{pipeline}"
            );
            assert_eq!(
                check(&format!("{head}{pipeline}{tail}"), &devices, None),
                CaptureRate::Ok,
                "{pipeline}"
            );
        }
        // Inside `devices` a sequence is still not understood.
        assert!(matches!(
            check(
                &CONFIG.replace("  chunksize:", "  - item\n  chunksize:"),
                &devices,
                None
            ),
            CaptureRate::Unchecked(_)
        ));
    }

    #[test]
    fn recognises_only_the_format_setting_failure_and_its_side() {
        let error = "ALSA function 'snd_pcm_hw_params_set_format' failed with error 'Invalid argument (22)'";
        let playback = |reason: &str| FormatFailure {
            playback: true,
            reason: reason.into(),
        };
        let capture = |reason: &str| FormatFailure {
            playback: false,
            reason: reason.into(),
        };
        for (value, expected) in [
            (
                serde_json::json!({"PlaybackError": error}),
                Some(playback(error)),
            ),
            (
                serde_json::json!({"CaptureError": error}),
                Some(capture(error)),
            ),
            // A plain string names its side in its words, else it is capture.
            (
                serde_json::json!(format!("Playback error: {error}")),
                Some(playback(&format!("Playback error: {error}"))),
            ),
            (serde_json::json!(error), Some(capture(error))),
            (serde_json::json!("None"), None),
            (Value::Null, None),
            (
                serde_json::json!({"PlaybackError": "Device or resource busy"}),
                None,
            ),
            // The rate error is the capture-rate notice's, not this one's.
            (
                serde_json::json!({"CaptureError": "snd_pcm_hw_params_set_rate: Invalid argument (22)"}),
                None,
            ),
            (
                serde_json::json!({"PlaybackError": "snd_pcm_hw_params_set_format: Device or resource busy"}),
                None,
            ),
        ] {
            assert_eq!(format_failure(&value), expected, "{value}");
        }
        // The two notices never both claim one stop reason.
        let rate = serde_json::json!({"CaptureError": "snd_pcm_hw_params_set_rate: Invalid argument (22)"});
        assert!(stop_reason(&rate).is_some() && format_failure(&rate).is_none());
        let format = serde_json::json!({"PlaybackError": error});
        assert!(stop_reason(&format).is_none() && format_failure(&format).is_some());
    }

    #[tokio::test]
    async fn the_format_notice_reads_the_failed_sides_device_and_format() {
        let dir =
            std::env::temp_dir().join(format!("tessera-web-format-notice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Speaker.yml");
        let write = |yaml: &str| std::fs::write(&path, yaml).unwrap();
        let active = Ok(Active {
            path: Some(path.clone()),
            label: Some("Speaker.yml".into()),
            boost_db: None,
            source: Source::Engine,
        });
        let failure = |playback| FormatFailure {
            playback,
            reason: "set_format Invalid argument".into(),
        };
        // The packaged template, as the installer seeds it for a card that plays S16_LE only.
        write(
            &packaged_speaker(48_000)
                .replace("format: S32_LE\n  enable", "format: S16_LE\n  enable"),
        );
        let notice = format_notice(&active, Some(&failure(true))).await.unwrap();
        assert_eq!(notice.device.as_deref(), Some("hw:CARD=DAC,DEV=0"));
        assert_eq!(notice.format.as_deref(), Some("S16_LE"));
        let notice = format_notice(&active, Some(&failure(false))).await.unwrap();
        assert_eq!(notice.device.as_deref(), Some("tessera"));
        assert_eq!(notice.format.as_deref(), Some("S32_LE"));
        // A file in CamillaGUI's style (sorted keys, quoted values) reads the same.
        write(&GUI_SAVED.replace("device: hw:CARD=DAC,DEV=0", "device: 'hw:CARD=DAC,DEV=0'"));
        let notice = format_notice(&active, Some(&failure(true))).await.unwrap();
        assert_eq!(notice.device.as_deref(), Some("hw:CARD=DAC,DEV=0"));
        assert_eq!(notice.format.as_deref(), Some("S32_LE"));
        // Unreadable, missing or keyless: the notice stays, without the details.
        for yaml in [
            "devices: {playback: {format: S16_LE}}\n",
            "pipeline: []\n",
            "",
        ] {
            write(yaml);
            let notice = format_notice(&active, Some(&failure(true))).await.unwrap();
            assert_eq!((notice.device, notice.format), (None, None), "{yaml:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
        let notice = format_notice(&active, Some(&failure(true))).await.unwrap();
        assert_eq!((notice.device, notice.format), (None, None));
        let none: Result<Active, String> = Err("no engine".into());
        assert_eq!(
            format_notice(&none, Some(&failure(true)))
                .await
                .unwrap()
                .failure,
            failure(true)
        );
        // No failure, no notice.
        assert_eq!(format_notice(&active, None).await, None);
    }

    #[test]
    fn an_unchecked_reason_is_reported_once_until_it_changes_or_the_check_works() {
        let reported = Mutex::new(None);
        let unchecked = |why: &str| CaptureRate::Unchecked(why.into());
        assert!(first_report(&reported, &unchecked("a")));
        assert!(!first_report(&reported, &unchecked("a")));
        assert!(first_report(&reported, &unchecked("b")));
        assert!(!first_report(&reported, &unchecked("b")));
        // A working check forgets the reason, so a later recurrence is logged.
        assert!(!first_report(&reported, &CaptureRate::Ok));
        assert!(first_report(&reported, &unchecked("b")));
        assert!(!first_report(&reported, &mismatch(true, 96_000, true)));
        assert!(first_report(&reported, &unchecked("b")));
    }
}

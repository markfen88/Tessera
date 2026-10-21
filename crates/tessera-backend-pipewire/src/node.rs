//! PipeWire streams for one endpoint: an `Audio/Source` (network RX) or an
//! `Audio/Sink` (network TX) node that can drive the graph.
//!
//! This module is glue: node properties, the format, and the process
//! callbacks that hand the graph's planar `f32` buffers to the real-time
//! engines in [`crate::rt`]. The callbacks run on PipeWire's data thread
//! (`RT_PROCESS`) and obey the real-time rules: no allocation, locks,
//! syscalls or logging.

use pipewire as pw;
use pw::properties::PropertiesBox;
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use spa::pod::Pod;
use zerocopy::FromBytes;

use crate::config::{Config, PositionMode};
use crate::device::{Dir, EndpointSpec};
use crate::error::Error;
use crate::rt::{EngineConfig, RtControl, RxEngine, TxEngine, rt_channel};

/// Bytes per sample of the graph format (`F32P`).
const SAMPLE_BYTES: usize = 4;

/// Characters allowed in a PipeWire node name derived from a device key.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The PipeWire node name of an endpoint.
pub fn node_name(cfg: &Config, spec: &EndpointSpec) -> String {
    let kind = match spec.dir {
        Dir::Rx => "source",
        Dir::Tx => "sink",
    };
    let mut n = format!(
        "{}.{}.{kind}",
        sanitize(&cfg.node_prefix),
        sanitize(&spec.device)
    );
    if spec.parts > 1 {
        n.push_str(&format!(".{}", spec.part + 1));
    }
    n
}

/// Sets a property; the PipeWire setters panic on NUL bytes, so none reaches
/// them.
fn set_prop(p: &mut PropertiesBox, k: &str, v: &str) {
    if v.contains('\0') {
        p.insert(k, v.replace('\0', "?"));
    } else {
        p.insert(k, v);
    }
}

/// The properties that follow the device description and may change on a live
/// node: description, nick and the channel names (port labels).
pub fn label_properties(spec: &EndpointSpec) -> PropertiesBox {
    let what = match spec.dir {
        Dir::Rx => "network receive",
        Dir::Tx => "network transmit",
    };
    let mut description = format!("{} ({what})", spec.device_name);
    if spec.parts > 1 {
        description.push_str(&format!(" {}/{}", spec.part + 1, spec.parts));
    }
    let mut p = PropertiesBox::new();
    set_prop(&mut p, "node.description", &description);
    set_prop(&mut p, "node.nick", &spec.device_name);
    set_prop(&mut p, "media.name", &description);
    set_prop(&mut p, "node.channel-names", &channel_names(&spec.labels));
    p
}

/// JSON array of channel names for `node.channel-names`.
fn channel_names(labels: &[String]) -> String {
    serde_json::to_string(labels).unwrap_or_else(|_| "[]".into())
}

/// Builds the node properties (ADR 0007: driver, suspend-on-idle disabled).
pub fn node_properties(cfg: &Config, spec: &EndpointSpec) -> PropertiesBox {
    let class = match spec.dir {
        Dir::Rx => "Audio/Source",
        Dir::Tx => "Audio/Sink",
    };
    let name = node_name(cfg, spec);
    let mut p = label_properties(spec);
    let mut set = |k: &str, v: &str| set_prop(&mut p, k, v);
    set("media.class", class);
    set("node.name", &name);
    set("application.name", &cfg.client_name);
    set("node.virtual", "false");
    set("node.network", "true");
    set("node.driver", "true");
    set("priority.driver", &cfg.priority_driver.to_string());
    set("priority.session", &cfg.priority_session.to_string());
    set("node.always-process", "true");
    set("node.pause-on-idle", "false");
    set("node.suspend-on-idle", "false");
    set("session.suspend-timeout-seconds", "0");
    set(
        "node.latency",
        &format!("{}/{}", cfg.period_frames, spec.rate),
    );
    set("node.rate", &format!("1/{}", spec.rate));
    set("tessera.device", &spec.device);
    set("tessera.first-channel", &spec.first_channel.to_string());
    p
}

/// SPA channel positions for `channels` channels.
fn positions(
    mode: PositionMode,
    channels: usize,
) -> [u32; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize] {
    let mut pos = [0u32; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    match (mode, channels) {
        (PositionMode::Auto, 1) => pos[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO,
        (PositionMode::Auto, 2) => {
            pos[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
            pos[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        }
        _ => {
            for (i, p) in pos.iter_mut().enumerate().take(channels.min(64)) {
                *p = spa::sys::SPA_AUDIO_CHANNEL_AUX0 + i as u32;
            }
        }
    }
    pos
}

fn format_pod(cfg: &Config, spec: &EndpointSpec) -> Result<Vec<u8>, Error> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32P);
    info.set_rate(spec.rate);
    info.set_channels(spec.channels());
    info.set_position(positions(cfg.positions, spec.labels.len()));
    let obj = spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map(|(c, _)| c.into_inner())
    .map_err(|e| Error::Config(format!("cannot build the format: {e:?}")))
}

/// Which engine the stream owns.
enum Engine {
    Rx(RxEngine),
    Tx(TxEngine),
}

/// A created PipeWire node (control-thread handle).
pub struct PwNode {
    /// The stream (its node id is `stream.node_id()`).
    pub stream: StreamRc,
    _listener: StreamListener<Engine>,
}

impl std::fmt::Debug for PwNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PwNode").finish_non_exhaustive()
    }
}

/// Reinterprets a byte region of a graph buffer as `f32` samples.
#[inline]
fn as_f32_mut(bytes: &mut [u8]) -> Option<&mut [f32]> {
    let n = bytes.len() / SAMPLE_BYTES * SAMPLE_BYTES;
    <[f32]>::mut_from_bytes(bytes.get_mut(..n)?).ok()
}

#[inline]
fn as_f32(bytes: &[u8]) -> Option<&[f32]> {
    let n = bytes.len() / SAMPLE_BYTES * SAMPLE_BYTES;
    <[f32]>::ref_from_bytes(bytes.get(..n)?).ok()
}

/// RX process callback: fills the graph's output buffer.
fn process_source(stream: &pw::stream::Stream, eng: &mut RxEngine) {
    let _rt = crate::alloc_check::RtGuard::enter();
    let Some(mut buf) = stream.dequeue_buffer() else {
        return;
    };
    let requested = buf.requested() as usize;
    let driving = stream.is_driving();
    let datas = buf.datas_mut();
    let capacity = datas
        .first()
        .map_or(0, |d| d.as_raw().maxsize as usize / SAMPLE_BYTES);
    let requested = if requested == 0 {
        capacity
    } else {
        requested.min(capacity)
    };
    let frames = eng.begin(driving, requested);
    for (ch, d) in datas.iter_mut().enumerate() {
        let written = match d.data().and_then(as_f32_mut) {
            Some(samples) => match samples.get_mut(..frames) {
                Some(out) => {
                    eng.channel(ch, out);
                    frames
                }
                None => 0,
            },
            None => 0,
        };
        let chunk = d.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = SAMPLE_BYTES as i32;
        *chunk.size_mut() = (written * SAMPLE_BYTES) as u32;
    }
    eng.end();
}

/// TX process callback: hands the graph's input buffer to the ring.
fn process_sink(stream: &pw::stream::Stream, eng: &mut TxEngine) {
    let _rt = crate::alloc_check::RtGuard::enter();
    let Some(mut buf) = stream.dequeue_buffer() else {
        return;
    };
    let driving = stream.is_driving();
    let datas = buf.datas_mut();
    let frames_in = datas
        .first()
        .map_or(0, |d| d.chunk().size() as usize / SAMPLE_BYTES);
    let frames = eng.begin(driving, frames_in);
    for (ch, d) in datas.iter_mut().enumerate() {
        let offset = d.chunk().offset() as usize;
        let size = d.chunk().size() as usize;
        let Some(bytes) = d.data() else { continue };
        let Some(region) = bytes.get(offset..offset.saturating_add(size).min(bytes.len())) else {
            continue;
        };
        if let Some(samples) = as_f32(region) {
            eng.channel(ch, samples.get(..frames.min(samples.len())).unwrap_or(&[]));
        }
    }
    eng.end();
}

fn resampler_error(e: tessera_media::ResampleError) -> Error {
    Error::Config(format!("cannot build the follower resampler: {e:?}"))
}

/// Applies a changed device description (labels, display name) to a live node
/// without recreating it, so PipeWire links to the node survive.
///
/// # Errors
/// PipeWire refused the update.
#[allow(unsafe_code)] // one FFI call, justified below
pub fn update_labels(node: &PwNode, spec: &EndpointSpec) -> Result<(), Error> {
    let props = label_properties(spec);
    // SAFETY: `pw_stream_update_properties` copies the key/value pairs of the
    // dictionary before returning; the stream pointer is valid for the life of
    // `node.stream` and the dictionary for the life of `props`. It is called
    // on the main-loop thread, like every other control call.
    let rc = unsafe {
        pw::sys::pw_stream_update_properties(node.stream.as_raw_ptr(), props.dict().as_raw_ptr())
    };
    if rc < 0 {
        return Err(Error::Config(format!(
            "cannot update the node properties ({rc})"
        )));
    }
    Ok(())
}

/// Creates and connects the node for `spec`. The stream starts as a driver;
/// nothing is exposed to the daemon until a session is attached.
pub fn create_node(
    core: &pw::core::CoreRc,
    cfg: &Config,
    spec: &EndpointSpec,
) -> Result<(PwNode, RtControl), Error> {
    let stream = StreamRc::new(
        core.clone(),
        &node_name(cfg, spec),
        node_properties(cfg, spec),
    )
    .map_err(|e| Error::Config(format!("cannot create the stream: {e}")))?;
    let (ctl, port) = rt_channel();
    let mut ecfg = EngineConfig::new(spec.labels.len(), spec.rate);
    ecfg.max_period = cfg.max_period_frames as usize;
    ecfg.chunk = cfg.follower_chunk_frames as usize;
    ecfg.rx_offset = u64::from(cfg.rx_offset_frames);
    ecfg.tx_ahead_periods = cfg.tx_ahead_periods;
    ecfg.tx_ahead_margin = cfg.tx_ahead_margin_frames;
    ecfg.resampler = cfg.resampler;
    let name = node_name(cfg, spec);
    let (engine, direction) = match spec.dir {
        Dir::Rx => (
            Engine::Rx(RxEngine::new(ecfg, port).map_err(resampler_error)?),
            spa::utils::Direction::Output,
        ),
        Dir::Tx => (
            Engine::Tx(TxEngine::new(ecfg, port).map_err(resampler_error)?),
            spa::utils::Direction::Input,
        ),
    };
    let listener = stream
        .add_local_listener_with_user_data(engine)
        .state_changed(move |_s, _e, old, new| {
            if let StreamState::Error(msg) = &new {
                tracing::warn!(node = %name, "stream error: {msg}");
            } else {
                tracing::debug!(node = %name, "stream state {old:?} -> {new:?}");
            }
        })
        .process(|stream, engine| match engine {
            Engine::Rx(e) => process_source(stream, e),
            Engine::Tx(e) => process_sink(stream, e),
        })
        .register()
        .map_err(|e| Error::Config(format!("cannot register the stream listener: {e}")))?;
    let bytes = format_pod(cfg, spec)?;
    let pod = Pod::from_bytes(&bytes).ok_or_else(|| Error::Config("invalid format pod".into()))?;
    let mut params = [pod];
    stream
        .connect(
            direction,
            None,
            StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| Error::Config(format!("cannot connect the stream: {e}")))?;
    Ok((
        PwNode {
            stream,
            _listener: listener,
        },
        ctl,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, labels: &[&str]) -> EndpointSpec {
        EndpointSpec {
            device: "d".into(),
            device_name: name.into(),
            dir: Dir::Rx,
            part: 0,
            parts: 1,
            first_channel: 1,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            rate: 48_000,
        }
    }

    #[test]
    fn property_setters_never_see_a_nul_byte() {
        pw::init();
        let mut p = PropertiesBox::new();
        set_prop(&mut p, "k", "a\0b");
        assert_eq!(p.get("k"), Some("a?b"));
        // Even a description that bypassed the device-level cleaning is safe.
        let props = label_properties(&spec("na\0me", &["l\0abel"]));
        assert_eq!(props.get("node.nick"), Some("na?me"));
        assert!(
            props
                .get("node.channel-names")
                .is_some_and(|v| !v.contains('\0'))
        );
    }

    #[test]
    fn node_names_are_plain() {
        let cfg = Config::default();
        let mut s = spec("n", &["a"]);
        s.device = "we ird/key\u{7}".into();
        assert_eq!(node_name(&cfg, &s), "tessera.we_ird_key_.source");
        s.parts = 3;
        s.part = 1;
        assert_eq!(node_name(&cfg, &s), "tessera.we_ird_key_.source.2");
    }

    #[test]
    fn positions_follow_the_mode() {
        let p = positions(PositionMode::Auto, 2);
        assert_eq!(p[0], spa::sys::SPA_AUDIO_CHANNEL_FL);
        assert_eq!(p[1], spa::sys::SPA_AUDIO_CHANNEL_FR);
        let p = positions(PositionMode::Aux, 2);
        assert_eq!(p[1], spa::sys::SPA_AUDIO_CHANNEL_AUX0 + 1);
        let p = positions(PositionMode::Auto, 1);
        assert_eq!(p[0], spa::sys::SPA_AUDIO_CHANNEL_MONO);
    }
}

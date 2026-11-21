//! Volume `Props` of our two sinks, when the engine owns the sink volume.
//!
//! Our sinks are `pw_stream`s, so PipeWire wraps each in an adapter whose
//! `audioconvert` applies the node's volume in `f32` before the engine sees a
//! sample. A desktop slider, a media key, `wpctl set-volume` or pavucontrol
//! writes `channelVolumes` (and `mute`); `audioconvert` copies them into its
//! software volume (`softVolumes`, `softMute`) and scales the samples.
//!
//! With `sink_volume: follow` the engine keeps that software volume at unity
//! and applies the requested level itself, the same split an ALSA card with a
//! hardware mixer uses (`channelVolumes` = what the user asked for,
//! `softVolumes` = what is left for PipeWire to do in software, here nothing).
//!
//! The adapter forwards every `Props` write to the stream as well, which
//! reports it through `param_changed`; a write the stream makes itself
//! (`pw_stream_set_param`) is not reported back. So each external change is
//! seen exactly once, our own writes never echo, and each external change is
//! answered with a complete write (the requested level, PipeWire's scaling at
//! unity); the engine's answer (e.g. capped at the ceiling) follows when it
//! differs.
//!
//! The desktop's 100 % is the volume ceiling (`max_volume_db`, 0 dB without
//! one): `channelVolumes` carry the level relative to the ceiling, both ways
//! ([`channel_volumes_for`], [`request_from_props`]). Desktops show the cube
//! root of the linear value as the percent, so below the ceiling the percent
//! falls the same cubic way as from 0 dB.
//!
//! Everything here is pure (pods and the dB mapping), so it is unit-tested
//! without PipeWire.

use audivane_protocol::ipc::{BALANCE_MAX_DB, SinkChoice, VOLUME_MIN_DB};
use pipewire as pw;
use pw::spa;
use spa::pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};

/// `node` property WirePlumber checks before restoring (and storing) a
/// node's volume. Off while we own the volume: the engine's persisted volume
/// is the truth, and a restore arriving after our first write would look like
/// a user change.
pub const RESTORE_PROPS_KEY: &str = "state.restore-props";

/// The volume fields of one `Props` write, as far as it set them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SinkProps {
    pub volume: Option<f32>,
    pub channel_volumes: Option<Vec<f32>>,
    pub mute: Option<bool>,
    pub soft_volumes: Option<Vec<f32>>,
    pub soft_mute: Option<bool>,
    /// `channelmix.max-volume` from `params`, when set.
    pub max_volume: Option<f32>,
}

impl SinkProps {
    /// True when the write touched the user-facing volume or mute.
    pub fn touches_volume(&self) -> bool {
        self.volume.is_some() || self.channel_volumes.is_some() || self.mute.is_some()
    }
}

/// A sink volume change requested from outside the engine (desktop, media
/// keys, `wpctl`), already mapped to the engine's terms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SinkVolumeRequest {
    pub sink: SinkChoice,
    /// `None` when the write only changed the mute flag.
    pub volume_db: Option<f64>,
    pub balance_db: Option<f64>,
    pub muted: Option<bool>,
}

/// What the engine wants a sink to show (and PipeWire to leave alone).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SinkVolumeView {
    pub volume_db: f64,
    pub balance_db: f64,
    pub muted: bool,
    /// The level the desktop shows as 100 %: the volume ceiling, 0 dB
    /// without one (and on Direct).
    pub ceiling_db: f64,
}

impl SinkVolumeView {
    pub const UNITY: Self = Self {
        volume_db: 0.0,
        balance_db: 0.0,
        muted: false,
        ceiling_db: 0.0,
    };
}

/// Parse a `Props` object pod. `None` when it is not an object.
pub fn parse_props(pod: &Pod) -> Option<SinkProps> {
    let (_, value) =
        spa::pod::deserialize::PodDeserializer::deserialize_from::<Value>(pod.as_bytes()).ok()?;
    parse_props_value(&value)
}

fn parse_props_value(value: &Value) -> Option<SinkProps> {
    let Value::Object(object) = value else {
        return None;
    };
    let mut out = SinkProps::default();
    for property in &object.properties {
        match (property.key, &property.value) {
            (spa::sys::SPA_PROP_volume, Value::Float(v)) => out.volume = Some(*v),
            (spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(v))) => {
                out.channel_volumes = Some(v.clone())
            }
            (spa::sys::SPA_PROP_mute, Value::Bool(v)) => out.mute = Some(*v),
            (spa::sys::SPA_PROP_softVolumes, Value::ValueArray(ValueArray::Float(v))) => {
                out.soft_volumes = Some(v.clone())
            }
            (spa::sys::SPA_PROP_softMute, Value::Bool(v)) => out.soft_mute = Some(*v),
            (spa::sys::SPA_PROP_params, Value::Struct(fields)) => {
                for pair in fields.chunks(2) {
                    if let [Value::String(key), Value::Float(v)] = pair
                        && key == "channelmix.max-volume"
                    {
                        out.max_volume = Some(*v);
                    }
                }
            }
            _ => {}
        }
    }
    Some(out)
}

/// Requests are rounded to this many dB: an `f32` round trip through
/// `channelVolumes` must not turn -20 dB into -19.999 998 dB, or a centred
/// balance into a tiny one (which would count as DSP active).
pub const REQUEST_RESOLUTION_DB: f64 = 0.001;

fn round_db(db: f64) -> f64 {
    // `+ 0.0` turns -0.0 into 0.0.
    (db / REQUEST_RESOLUTION_DB).round() * REQUEST_RESOLUTION_DB + 0.0
}

/// Linear amplitude to dB, with silence mapped to the engine's floor.
fn amp_to_db(amp: f64) -> f64 {
    if !amp.is_finite() || amp <= 0.0 {
        VOLUME_MIN_DB
    } else {
        (20.0 * amp.log10()).max(VOLUME_MIN_DB)
    }
}

/// Map a `Props` write to a request. Master `volume` multiplies every channel
/// volume, as in `audioconvert`. The loudest channel is the volume; a quieter
/// left (right) channel is a positive (negative) balance, the engine's
/// convention (`balance_db > 0` attenuates the left channel). Only the first
/// two channels carry balance; more channels use the loudest one.
///
/// The volume is relative to `ceiling_db` (the desktop's 100 %, see
/// [`SinkVolumeView::ceiling_db`]) and is not clamped to the engine's range
/// above it (a desktop can ask for +10 dB); the control plane does that.
pub fn request_from_props(
    sink: SinkChoice,
    props: &SinkProps,
    ceiling_db: f64,
) -> Option<SinkVolumeRequest> {
    if !props.touches_volume() {
        return None;
    }
    let absolute = |db: f64| round_db((db + ceiling_db).max(VOLUME_MIN_DB));
    let master = f64::from(props.volume.unwrap_or(1.0));
    let (volume_db, balance_db) = match (&props.channel_volumes, props.volume) {
        (Some(ch), _) if !ch.is_empty() => {
            let scaled: Vec<f64> = ch.iter().map(|&v| f64::from(v) * master).collect();
            let loudest = scaled.iter().copied().fold(0.0_f64, f64::max);
            let volume_db = amp_to_db(loudest);
            let balance_db = if scaled.len() >= 2 && loudest > 0.0 {
                let left = amp_to_db(scaled[0]) - volume_db;
                let right = amp_to_db(scaled[1]) - volume_db;
                // One of them is 0 dB (the loudest of the pair, or both
                // below a louder third channel: then the quieter wins).
                let b = if left < right {
                    right - left
                } else {
                    -(left - right)
                };
                Some(round_db(b.clamp(-BALANCE_MAX_DB, BALANCE_MAX_DB)))
            } else {
                Some(0.0)
            };
            (Some(absolute(volume_db)), balance_db)
        }
        (_, Some(_)) => (Some(absolute(amp_to_db(master))), None),
        _ => (None, None),
    };
    Some(SinkVolumeRequest {
        sink,
        volume_db,
        balance_db,
        muted: props.mute,
    })
}

/// Per-channel linear volumes that show `view` on a sink with `channels`
/// channels (`[L, R, rest...]`), relative to the view's ceiling (at most 1:
/// the ceiling is 100 %).
pub fn channel_volumes_for(view: &SinkVolumeView, channels: usize) -> Vec<f32> {
    let master = 10f64.powf((view.volume_db - view.ceiling_db).min(0.0) / 20.0);
    let (l, r) = if view.balance_db > 0.0 {
        (10f64.powf(-view.balance_db / 20.0), 1.0)
    } else if view.balance_db < 0.0 {
        (1.0, 10f64.powf(view.balance_db / 20.0))
    } else {
        (1.0, 1.0)
    };
    (0..channels.max(1))
        .map(|c| {
            let g = match c {
                0 => l,
                1 => r,
                _ => 1.0,
            };
            (master * g) as f32
        })
        .collect()
}

/// The views are equal to the request resolution (an `f32` round trip).
fn same_view(a: &SinkVolumeView, b: &SinkVolumeView) -> bool {
    let tol = REQUEST_RESOLUTION_DB * 1.5;
    a.muted == b.muted
        && (a.volume_db - b.volume_db).abs() <= tol
        && (a.ceiling_db - b.ceiling_db).abs() <= tol
        && (a.balance_db - b.balance_db).abs() <= tol
}

fn float_array(values: Vec<f32>) -> Value {
    Value::ValueArray(ValueArray::Float(values))
}

fn prop(key: u32, value: Value) -> Property {
    Property {
        key,
        flags: PropertyFlags::empty(),
        value,
    }
}

fn serialize_props(properties: Vec<Property>) -> Vec<u8> {
    let object = Object {
        type_: spa::utils::SpaTypes::ObjectParamProps.as_raw(),
        id: spa::param::ParamType::Props.as_raw(),
        properties,
    };
    spa::pod::serialize::PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|(cursor, _)| cursor.into_inner())
        .unwrap_or_default()
}

/// A `Props` pod setting only an app stream's `channelVolumes` (its own
/// master `volume` is left alone).
pub fn stream_volume_pod(volumes: &[f32]) -> Vec<u8> {
    serialize_props(vec![prop(
        spa::sys::SPA_PROP_channelVolumes,
        float_array(volumes.to_vec()),
    )])
}

/// A `Props` pod setting an app stream's `channelVolumes` and/or `mute`,
/// whichever is given (its own master `volume` is left alone).
pub fn stream_props_pod(volumes: Option<&[f32]>, mute: Option<bool>) -> Vec<u8> {
    let mut properties = Vec::new();
    if let Some(mute) = mute {
        properties.push(prop(spa::sys::SPA_PROP_mute, Value::Bool(mute)));
    }
    if let Some(volumes) = volumes {
        properties.push(prop(
            spa::sys::SPA_PROP_channelVolumes,
            float_array(volumes.to_vec()),
        ));
    }
    serialize_props(properties)
}

/// `audioconvert`'s `channelmix.max-volume` while we own the volume: it
/// clamps each of `volume`, `channelVolumes` and `softVolumes` at this, so
/// between an external write and our answer PipeWire never scales above
/// unity (measured: `channelVolumes` 2.0 or `softVolumes` 2.0 play at 0 dB,
/// 0.5 still at -6 dB).
pub const OWNED_MAX_VOLUME: f32 = 1.0;
/// PipeWire's default `channelmix.max-volume`, restored on release.
pub const DEFAULT_MAX_VOLUME: f32 = 10.0;

/// `params` entry setting `channelmix.max-volume`.
fn max_volume_param(max: f32) -> Property {
    prop(
        spa::sys::SPA_PROP_params,
        Value::Struct(vec![
            Value::String("channelmix.max-volume".into()),
            Value::Float(max),
        ]),
    )
}

/// A `Props` pod showing `view` while PipeWire's own scaling stays at unity:
/// `volume` 1, `channelVolumes` from `view`, `mute`, `softVolumes` 1,
/// `softMute` false, and `channelmix.max-volume` [`OWNED_MAX_VOLUME`].
pub fn owned_props_pod(view: &SinkVolumeView, channels: usize) -> Vec<u8> {
    owned_props_pod_with_max(view, channels, OWNED_MAX_VOLUME)
}

/// [`owned_props_pod`] with another `channelmix.max-volume` (release).
pub fn owned_props_pod_with_max(view: &SinkVolumeView, channels: usize, max: f32) -> Vec<u8> {
    let n = channels.max(1);
    serialize_props(vec![
        max_volume_param(max),
        prop(spa::sys::SPA_PROP_volume, Value::Float(1.0)),
        prop(spa::sys::SPA_PROP_mute, Value::Bool(view.muted)),
        prop(
            spa::sys::SPA_PROP_channelVolumes,
            float_array(channel_volumes_for(view, n)),
        ),
        prop(spa::sys::SPA_PROP_softVolumes, float_array(vec![1.0; n])),
        prop(spa::sys::SPA_PROP_softMute, Value::Bool(false)),
    ])
}

/// Only PipeWire's own scaling back to unity (`softVolumes` 1, `softMute`
/// false, plus `volume` 1 when the write set the master volume). Not enough
/// on its own for our sinks (see [`on_props_changed`]); kept for tools and
/// tests.
pub fn neutral_props_pod(channels: usize, reset_master: bool) -> Vec<u8> {
    let n = channels.max(1);
    let mut properties = vec![
        prop(spa::sys::SPA_PROP_softVolumes, float_array(vec![1.0; n])),
        prop(spa::sys::SPA_PROP_softMute, Value::Bool(false)),
    ];
    if reset_master {
        properties.push(prop(spa::sys::SPA_PROP_volume, Value::Float(1.0)));
    }
    serialize_props(properties)
}

// ---------------------------------------------------------------- driver side

/// Shared between the engine's control threads and its PipeWire thread.
#[derive(Debug)]
pub struct SinkVolumeShared {
    follow: AtomicBool,
    /// What the DSP sink should show (the engine's effective volume).
    view: Mutex<SinkVolumeView>,
    /// What each sink shows right now, as far as we know (`[dsp, direct]`).
    shown: Mutex<[Option<SinkVolumeView>; 2]>,
    /// Each sink's own `channelmix.max-volume` (from its config, or set by
    /// someone else), restored when we stop owning the volume.
    prior_max: Mutex<[Option<f32>; 2]>,
    tx: Mutex<Option<mpsc::Sender<SinkVolumeRequest>>>,
}

fn slot(sink: SinkChoice) -> usize {
    usize::from(sink == SinkChoice::Direct)
}

impl SinkVolumeShared {
    /// `initial`: `Some(view)` to own the sink volume from the start.
    pub fn new(initial: Option<SinkVolumeView>) -> Self {
        Self {
            follow: AtomicBool::new(initial.is_some()),
            view: Mutex::new(initial.unwrap_or(SinkVolumeView::UNITY)),
            shown: Mutex::new([None, None]),
            prior_max: Mutex::new([None, None]),
            tx: Mutex::new(None),
        }
    }

    pub fn follow(&self) -> bool {
        self.follow.load(Ordering::Acquire)
    }

    pub fn view(&self) -> SinkVolumeView {
        self.view
            .lock()
            .map(|v| *v)
            .unwrap_or(SinkVolumeView::UNITY)
    }

    /// Returns whether `follow` changed.
    pub fn set(&self, follow: bool, view: SinkVolumeView) -> bool {
        if let Ok(mut v) = self.view.lock() {
            *v = view;
        }
        self.follow.swap(follow, Ordering::AcqRel) != follow
    }

    /// A fresh request channel; replaces any earlier subscriber.
    pub fn subscribe(&self) -> mpsc::Receiver<SinkVolumeRequest> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut slot) = self.tx.lock() {
            *slot = Some(tx);
        }
        rx
    }

    fn send(&self, request: SinkVolumeRequest) {
        if let Ok(tx) = self.tx.lock()
            && let Some(tx) = tx.as_ref()
        {
            let _ = tx.send(request);
        }
    }

    /// What a sink should show: the engine's view on the DSP sink, unity
    /// volume (with the engine's mute) on Direct.
    pub fn wanted(&self, sink: SinkChoice) -> SinkVolumeView {
        let view = self.view();
        match sink {
            SinkChoice::Direct => SinkVolumeView {
                muted: view.muted,
                ..SinkVolumeView::UNITY
            },
            _ => view,
        }
    }

    fn note_shown(&self, sink: SinkChoice, update: impl FnOnce(&mut SinkVolumeView)) {
        if let Ok(mut shown) = self.shown.lock() {
            let entry = shown[slot(sink)].get_or_insert(SinkVolumeView::UNITY);
            update(entry);
        }
    }

    /// Note a sink's reported `channelmix.max-volume`. While we own the
    /// volume the node reports our own value; anything else is someone
    /// else's and is what to restore later.
    pub fn note_max_volume(&self, sink: SinkChoice, max: f32) {
        let ours = self.follow() && (max - OWNED_MAX_VOLUME).abs() < 1e-6;
        if !ours && let Ok(mut prior) = self.prior_max.lock() {
            prior[slot(sink)] = Some(max);
        }
    }

    /// The `channelmix.max-volume` to put back on release.
    pub fn prior_max_volume(&self, sink: SinkChoice) -> f32 {
        self.prior_max
            .lock()
            .ok()
            .and_then(|p| p[slot(sink)])
            .unwrap_or(DEFAULT_MAX_VOLUME)
    }

    /// A new stream starts unknown.
    pub fn forget_shown(&self, sink: SinkChoice) {
        if let Ok(mut shown) = self.shown.lock() {
            shown[slot(sink)] = None;
        }
    }

    fn shown(&self, sink: SinkChoice) -> Option<SinkVolumeView> {
        self.shown.lock().ok().and_then(|shown| shown[slot(sink)])
    }

    fn shows(&self, sink: SinkChoice, view: &SinkVolumeView) -> bool {
        self.shown
            .lock()
            .map(|shown| shown[slot(sink)].is_some_and(|s| same_view(&s, view)))
            .unwrap_or(false)
    }
}

/// Write a `Props` pod on our own stream. The stream does not report its own
/// writes back through `param_changed`.
fn set_stream_props(stream: &pw::stream::Stream, pod: &[u8]) -> bool {
    let Some(pod) = Pod::from_bytes(pod) else {
        return false;
    };
    // Safety: called on the PipeWire main-loop thread that owns `stream`;
    // `pod` is a valid serialized pod that outlives the call (PipeWire
    // copies it).
    let res = unsafe {
        pw::sys::pw_stream_set_param(
            stream.as_raw_ptr(),
            spa::sys::SPA_PARAM_Props,
            pod.as_raw_ptr(),
        )
    };
    if res < 0 {
        log::warn!("cannot set the sink volume (pw_stream_set_param(Props): {res})");
    }
    res >= 0
}

/// `param_changed(Props)` on one of our sinks (PipeWire thread). With
/// `follow`, PipeWire's own scaling is put back to unity at once and the
/// request goes to the engine; on Direct the volume is put back to unity too.
pub fn on_props_changed(
    stream: &pw::stream::Stream,
    shared: &SinkVolumeShared,
    sink: SinkChoice,
    channels: usize,
    pod: &Pod,
) {
    let Some(props) = parse_props(pod) else {
        return;
    };
    if let Some(max) = props.max_volume {
        shared.note_max_volume(sink, max);
    }
    if !shared.follow() {
        return;
    }
    let ceiling_db = shared.wanted(sink).ceiling_db;
    let Some(request) = request_from_props(sink, &props, ceiling_db) else {
        // Someone set only PipeWire's own scaling: put it back.
        if props.soft_volumes.is_some() || props.soft_mute.is_some() {
            let view = shared.shown(sink).unwrap_or_else(|| shared.wanted(sink));
            if set_stream_props(stream, &owned_props_pod(&view, channels)) {
                shared.note_shown(sink, |s| *s = view);
            }
        }
        return;
    };
    // Write the whole owned state right away: the sink reports the
    // stream's last own write, so only resetting `softVolumes` would make
    // the desktop slider jump back to the previous level.
    let view = match sink {
        SinkChoice::Direct => SinkVolumeView {
            muted: request.muted.unwrap_or(shared.view().muted),
            ..SinkVolumeView::UNITY
        },
        _ => {
            let mut view = shared.shown(sink).unwrap_or_else(|| shared.view());
            if let Some(v) = request.volume_db {
                view.volume_db = v;
            }
            if let Some(b) = request.balance_db {
                view.balance_db = b;
            }
            if let Some(m) = request.muted {
                view.muted = m;
            }
            view.ceiling_db = ceiling_db;
            view
        }
    };
    if set_stream_props(stream, &owned_props_pod(&view, channels)) {
        shared.note_shown(sink, |s| *s = view);
    }
    log::debug!("sink volume request on {sink:?}: {request:?}");
    shared.send(request);
}

/// Make a sink show what the engine wants (PipeWire thread). With `follow`
/// off, `release` puts it back to plain unity once (PipeWire scales nothing
/// and the desktop shows 100 %).
pub fn assert_owned(
    stream: &pw::stream::Stream,
    shared: &SinkVolumeShared,
    sink: SinkChoice,
    channels: usize,
    release: bool,
) {
    if shared.follow() {
        let view = shared.wanted(sink);
        if shared.shows(sink, &view) {
            return;
        }
        if set_stream_props(stream, &owned_props_pod(&view, channels)) {
            shared.note_shown(sink, |s| *s = view);
        }
    } else if release {
        set_stream_props(
            stream,
            &owned_props_pod_with_max(
                &SinkVolumeView::UNITY,
                channels,
                shared.prior_max_volume(sink),
            ),
        );
        shared.forget_shown(sink);
    }
}

/// `state.restore-props` for the current mode.
pub fn update_restore_props(stream: &pw::stream::Stream, follow: bool) {
    let mut props = pw::properties::PropertiesBox::new();
    props.insert(RESTORE_PROPS_KEY, if follow { "false" } else { "true" });
    // Safety: PipeWire main-loop thread; `props` is a valid `pw_properties`
    // whose first member is its `spa_dict`.
    unsafe {
        let dict = &(*props.as_raw_ptr()).dict as *const spa::sys::spa_dict;
        pw::sys::pw_stream_update_properties(stream.as_raw_ptr(), dict);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::ipc::SinkChoice;

    fn reparse(bytes: &[u8]) -> SinkProps {
        let pod = Pod::from_bytes(bytes).expect("pod");
        parse_props(pod).expect("object")
    }

    #[test]
    fn stream_props_pod_writes_only_what_is_given() {
        let both = reparse(&stream_props_pod(Some(&[0.5, 0.25]), Some(true)));
        assert_eq!(both.channel_volumes, Some(vec![0.5, 0.25]));
        assert_eq!(both.mute, Some(true));
        assert_eq!(both.volume, None, "the app's master is left alone");
        let mute = reparse(&stream_props_pod(None, Some(false)));
        assert_eq!(mute.channel_volumes, None);
        assert_eq!(mute.mute, Some(false));
        let level = reparse(&stream_props_pod(Some(&[1.0]), None));
        assert_eq!(level.mute, None);
        assert_eq!(level.channel_volumes, Some(vec![1.0]));
    }

    #[test]
    fn owned_pod_round_trips_and_keeps_pipewire_at_unity() {
        let view = SinkVolumeView {
            volume_db: -20.0,
            balance_db: 6.0,
            muted: true,
            ceiling_db: 0.0,
        };
        let props = reparse(&owned_props_pod(&view, 2));
        assert_eq!(props.volume, Some(1.0));
        assert_eq!(props.mute, Some(true));
        assert_eq!(props.soft_volumes, Some(vec![1.0, 1.0]));
        assert_eq!(props.soft_mute, Some(false));
        // PipeWire may never scale above unity, even for a moment.
        assert_eq!(props.max_volume, Some(OWNED_MAX_VOLUME));
        let released = reparse(&owned_props_pod_with_max(
            &SinkVolumeView::UNITY,
            2,
            DEFAULT_MAX_VOLUME,
        ));
        assert_eq!(released.max_volume, Some(DEFAULT_MAX_VOLUME));
        assert_eq!(released.channel_volumes, Some(vec![1.0, 1.0]));
        let ch = props.channel_volumes.clone().unwrap();
        assert!((f64::from(ch[1]) - 0.1).abs() < 1e-7);
        assert!((f64::from(ch[0]) - 0.1 * 10f64.powf(-6.0 / 20.0)).abs() < 1e-7);

        // And maps back to the same view.
        let req = request_from_props(SinkChoice::Dsp, &props, 0.0).unwrap();
        assert_eq!(req.volume_db, Some(-20.0));
        assert_eq!(req.balance_db, Some(6.0));
        assert_eq!(req.muted, Some(true));
    }

    #[test]
    fn request_mapping_volume_balance_and_mute_only() {
        // wpctl set-volume 0.5 (cubic) writes 0.125 linear: -18.06 dB.
        let props = SinkProps {
            channel_volumes: Some(vec![0.125, 0.125]),
            ..Default::default()
        };
        let req = request_from_props(SinkChoice::Dsp, &props, 0.0).unwrap();
        assert!((req.volume_db.unwrap() - 20.0 * 0.125f64.log10()).abs() < 1e-3);
        assert_eq!(req.balance_db, Some(0.0));
        assert!(req.balance_db.unwrap().is_sign_positive());
        assert_eq!(req.muted, None);

        // Right quieter: negative balance.
        let props = SinkProps {
            channel_volumes: Some(vec![1.0, 0.5]),
            ..Default::default()
        };
        let req = request_from_props(SinkChoice::Dsp, &props, 0.0).unwrap();
        assert_eq!(req.volume_db, Some(0.0));
        assert!((req.balance_db.unwrap() - 20.0 * 0.5f64.log10()).abs() < 1e-3);

        // Above unity is reported as asked (the control plane clamps).
        let props = SinkProps {
            channel_volumes: Some(vec![3.375, 3.375]),
            ..Default::default()
        };
        let req = request_from_props(SinkChoice::Dsp, &props, 0.0).unwrap();
        assert!((req.volume_db.unwrap() - 20.0 * 3.375f64.log10()).abs() < 1e-3);

        // Zero is the floor, mute only carries no volume.
        let props = SinkProps {
            channel_volumes: Some(vec![0.0, 0.0]),
            ..Default::default()
        };
        assert_eq!(
            request_from_props(SinkChoice::Dsp, &props, 0.0)
                .unwrap()
                .volume_db,
            Some(VOLUME_MIN_DB)
        );
        let props = SinkProps {
            mute: Some(true),
            ..Default::default()
        };
        let req = request_from_props(SinkChoice::Dsp, &props, 0.0).unwrap();
        assert_eq!(req.volume_db, None);
        assert_eq!(req.muted, Some(true));

        // Our own neutralising fields alone are not a request.
        let props = SinkProps {
            soft_volumes: Some(vec![1.0, 1.0]),
            soft_mute: Some(false),
            ..Default::default()
        };
        assert_eq!(request_from_props(SinkChoice::Dsp, &props, 0.0), None);
    }

    #[test]
    fn neutral_pod_only_resets_pipewire_scaling() {
        let props = reparse(&neutral_props_pod(2, false));
        assert_eq!(props.soft_volumes, Some(vec![1.0, 1.0]));
        assert_eq!(props.soft_mute, Some(false));
        assert!(!props.touches_volume());
        let props = reparse(&neutral_props_pod(2, true));
        assert_eq!(props.volume, Some(1.0));
    }

    #[test]
    fn shared_state_tracks_follow_view_and_what_is_shown() {
        let shared = SinkVolumeShared::new(None);
        assert!(!shared.follow());
        let view = SinkVolumeView {
            volume_db: -12.0,
            balance_db: 0.0,
            muted: false,
            ceiling_db: -6.0,
        };
        assert!(shared.set(true, view));
        assert!(!shared.set(true, view));
        assert_eq!(shared.wanted(SinkChoice::Dsp), view);
        assert_eq!(shared.wanted(SinkChoice::Direct), SinkVolumeView::UNITY);
        assert!(!shared.shows(SinkChoice::Dsp, &view));
        shared.note_shown(SinkChoice::Dsp, |s| *s = view);
        let nearly = SinkVolumeView {
            volume_db: -12.0004,
            ..view
        };
        assert!(shared.shows(SinkChoice::Dsp, &nearly));
        assert!(!shared.shows(SinkChoice::Direct, &view));

        let rx = shared.subscribe();
        shared.send(SinkVolumeRequest {
            sink: SinkChoice::Dsp,
            volume_db: Some(-3.0),
            balance_db: Some(0.0),
            muted: None,
        });
        assert_eq!(rx.try_recv().unwrap().volume_db, Some(-3.0));
    }

    /// Leaving follow puts back the sink's own max-volume, not a guess;
    /// our own value reported back while owning is not taken for it.
    #[test]
    fn release_restores_the_sinks_own_max_volume() {
        let shared = SinkVolumeShared::new(None);
        assert_eq!(shared.prior_max_volume(SinkChoice::Dsp), DEFAULT_MAX_VOLUME);
        shared.note_max_volume(SinkChoice::Dsp, 4.0);
        shared.set(true, SinkVolumeView::UNITY);
        shared.note_max_volume(SinkChoice::Dsp, OWNED_MAX_VOLUME);
        assert_eq!(shared.prior_max_volume(SinkChoice::Dsp), 4.0);
        assert_eq!(
            shared.prior_max_volume(SinkChoice::Direct),
            DEFAULT_MAX_VOLUME
        );
        // Someone else changing it while we own the volume: that is theirs.
        shared.note_max_volume(SinkChoice::Dsp, 2.5);
        assert_eq!(shared.prior_max_volume(SinkChoice::Dsp), 2.5);
    }

    #[test]
    fn master_volume_scales_channels() {
        let props = SinkProps {
            volume: Some(0.5),
            channel_volumes: Some(vec![1.0, 1.0]),
            ..Default::default()
        };
        let req = request_from_props(SinkChoice::Dsp, &props, 0.0).unwrap();
        assert!((req.volume_db.unwrap() - 20.0 * 0.5f64.log10()).abs() < 1e-3);
        let props = SinkProps {
            volume: Some(0.5),
            ..Default::default()
        };
        let req = request_from_props(SinkChoice::Dsp, &props, 0.0).unwrap();
        assert!((req.volume_db.unwrap() - 20.0 * 0.5f64.log10()).abs() < 1e-3);
        assert_eq!(req.balance_db, None);
    }

    /// The desktop's percent (the cube root of the linear volume).
    fn percent(linear: f32) -> f64 {
        f64::from(linear).cbrt() * 100.0
    }

    fn view_at(volume_db: f64, ceiling_db: f64) -> SinkVolumeView {
        SinkVolumeView {
            volume_db,
            ceiling_db,
            ..SinkVolumeView::UNITY
        }
    }

    fn desktop_write(linear: f32) -> SinkProps {
        SinkProps {
            channel_volumes: Some(vec![linear, linear]),
            ..Default::default()
        }
    }

    #[test]
    fn the_ceiling_is_the_desktops_full_scale() {
        for ceiling in [0.0, -3.0, -6.0, -20.0] {
            // The engine at the ceiling shows 100 %.
            let ch = channel_volumes_for(&view_at(ceiling, ceiling), 2);
            assert_eq!(ch, vec![1.0, 1.0], "ceiling {ceiling}");
            // A desktop 100 % lands exactly at the ceiling.
            let req = request_from_props(SinkChoice::Dsp, &desktop_write(1.0), ceiling).unwrap();
            assert_eq!(req.volume_db, Some(ceiling), "ceiling {ceiling}");
            assert_eq!(req.balance_db, Some(0.0));
            // A key up at 100 % asks for more; the sink still shows 100 %
            // (the control plane caps the level at the ceiling).
            let up = request_from_props(SinkChoice::Dsp, &desktop_write(1.05f32.powi(3)), ceiling)
                .unwrap();
            assert!(up.volume_db.unwrap() > ceiling);
            let shown = view_at(up.volume_db.unwrap(), ceiling);
            assert_eq!(channel_volumes_for(&shown, 2), vec![1.0, 1.0]);
            // Below it the percent scales the cubic way: 50 % is 1/8 linear,
            // -18.06 dB under the ceiling.
            let half = request_from_props(SinkChoice::Dsp, &desktop_write(0.125), ceiling).unwrap();
            assert!((half.volume_db.unwrap() - (ceiling - 18.062)).abs() < 2e-3);
        }
        // Without a ceiling a -3 dB level reads 89 %.
        let ch = channel_volumes_for(&view_at(-3.0, 0.0), 2);
        assert!((percent(ch[0]) - 89.1).abs() < 0.1);
        // With a -3 dB ceiling the same level is 100 %, and -9 dB reads 79 %.
        let ch = channel_volumes_for(&view_at(-9.0, -3.0), 2);
        assert!((percent(ch[0]) - 79.4).abs() < 0.1);
    }

    #[test]
    fn mapping_round_trips_at_several_ceilings() {
        for ceiling in [0.0, -1.5, -3.0, -12.0, -40.0] {
            for volume in [0.0, -0.5, -3.0, -10.0, -30.0, -60.0, -100.0, -120.0] {
                let volume = f64::min(volume, ceiling);
                for balance in [0.0, 6.0, -3.5] {
                    let view = SinkVolumeView {
                        volume_db: volume,
                        balance_db: balance,
                        muted: false,
                        ceiling_db: ceiling,
                    };
                    let props = SinkProps {
                        channel_volumes: Some(channel_volumes_for(&view, 2)),
                        ..Default::default()
                    };
                    let req = request_from_props(SinkChoice::Dsp, &props, ceiling).unwrap();
                    let got = req.volume_db.unwrap();
                    assert!(
                        (got - volume).abs() <= REQUEST_RESOLUTION_DB,
                        "ceiling {ceiling}, volume {volume}: {got}"
                    );
                    // (At the floor the quieter channel has nowhere to go.)
                    let b = req.balance_db.unwrap();
                    let floored = volume - f64::abs(balance) < VOLUME_MIN_DB;
                    assert!(
                        floored || (b - balance).abs() <= REQUEST_RESOLUTION_DB,
                        "ceiling {ceiling}, balance {balance}: {b}"
                    );
                }
            }
        }
        // Silence is the engine's floor whatever the ceiling.
        let req = request_from_props(SinkChoice::Dsp, &desktop_write(0.0), -6.0).unwrap();
        assert_eq!(req.volume_db, Some(VOLUME_MIN_DB));
    }

    #[test]
    fn a_ceiling_change_moves_the_slider_not_the_level() {
        let shared = SinkVolumeShared::new(Some(view_at(-6.0, -3.0)));
        shared.note_shown(SinkChoice::Dsp, |s| *s = view_at(-6.0, -3.0));
        assert!(shared.shows(SinkChoice::Dsp, &shared.wanted(SinkChoice::Dsp)));
        let before = channel_volumes_for(&shared.wanted(SinkChoice::Dsp), 2);
        // Same level, ceiling down to -6 dB: the sink must be rewritten,
        // and it reads 100 %.
        shared.set(true, view_at(-6.0, -6.0));
        let wanted = shared.wanted(SinkChoice::Dsp);
        assert!(!shared.shows(SinkChoice::Dsp, &wanted));
        assert_eq!(wanted.volume_db, -6.0);
        let after = channel_volumes_for(&wanted, 2);
        assert!(after[0] > before[0]);
        assert_eq!(after, vec![1.0, 1.0]);
        // Direct stays at 100 % whatever the ceiling.
        let direct = shared.wanted(SinkChoice::Direct);
        assert_eq!(direct.ceiling_db, 0.0);
        assert_eq!(channel_volumes_for(&direct, 2), vec![1.0, 1.0]);
    }
}

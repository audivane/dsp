//! Direct ALSA `hw:` output, DAC clock source, `ReserveDevice1` D-Bus
//! ownership, and `rtkit` real-time thread promotion.

pub mod caps;
pub mod error;
pub mod mixer;
pub mod pcm;
pub mod reserve;
pub mod rt;

pub use caps::{PROBE_FORMATS, plan_open, probe_pcm, probe_rates, ratio_ladder};
pub use error::{AlsaError, Result};
pub use mixer::{
    MixerApplyOutcome, MixerElementCandidate, PREFERRED_PLAYBACK_CONTROLS, apply_playback_volume,
    select_playback_element,
};
pub use pcm::{
    AlsaConfig, AlsaOutput, FORMAT_PREFERENCE, NegotiatedParams, OpenPcm, bytes_per_sample,
    pack_left_justified_i32,
};
pub use reserve::{DEFAULT_RESERVE_PRIORITY, DeviceReservation, card_index_from_device};
pub use rt::{promote_current_thread_rt, try_promote_current_thread_rt};

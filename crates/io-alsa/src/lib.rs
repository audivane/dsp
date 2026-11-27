//! Direct ALSA `hw:` output, DAC clock source, `ReserveDevice1` D-Bus
//! ownership, and `rtkit` real-time thread promotion.

pub mod caps;
pub mod error;
pub mod identity;
pub mod mixer;
pub mod pcm;
pub mod reserve;
pub mod rt;

pub use caps::{PROBE_FORMATS, plan_open, probe_pcm, probe_rates, ratio_ladder};
pub use error::{AlsaError, Result};
pub use mixer::{
    MixerApplyOutcome, MixerElementCandidate, PREFERRED_PLAYBACK_CONTROLS, PlaybackChannel,
    apply_playback_volume, select_playback_element,
};
pub use pcm::{
    AlsaConfig, AlsaOutput, FORMAT_PREFERENCE, NegotiatedParams, OpenPcm, RatioOpen,
    bytes_per_sample, channel_map_error, min_device_channels, pack_left_justified_i32,
    pack_mapped_i32, plan_modes,
};
pub use reserve::{
    DEFAULT_RESERVE_PRIORITY, DeviceReservation, card_index_from_device, device_present,
    device_present_gated_in, device_present_in,
};
pub use rt::{
    Demotion, MAX_HELPER_THREADS, RunGuard, alsa_lib_errors, audio_thread_demotion_failures,
    audio_thread_demotions, audio_thread_tid, current_tid, demote_current_thread, demote_thread,
    helper_thread_demotions, install_overload_guard, last_overload_tid, overload_signals,
    promote_current_thread_rt, promote_thread_rt, register_audio_thread, register_helper_thread,
    restore_rttime_limits, rtkit_rttime_max_usec, rttime_limits_usec, set_rttime_limits,
    silence_alsa_lib_on_this_thread, thread_is_realtime, thread_policy_name,
    try_promote_current_thread_rt, unregister_audio_thread, unregister_helper_thread,
};

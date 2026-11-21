// The types the UI works with: the protocol types generated from
// crates/protocol by ts-rs (`npm run gen:types`). Nothing here is hand-written
// except the names the UI uses for a few unions and call parameters.
//
// Older daemons still send a few legacy shapes:
// `chain.convolution` as `null` or one object, and no `volume`/`preset` in
// `Status`. Code that reads them goes through `lib/chain.ts::convList` and
// optional chaining, so those daemons keep working with features disabled.

import type * as G from "./generated";

export type {
  Accept,
  ArithmeticPath,
  BlockSize,
  CascadeStage,
  Decimation,
  DesignCheck,
  DesignPlan,
  Fallback,
  FallbackWhen,
  FieldError,
  FlattopSet,
  GridCutoff,
  InterpMethod,
  Numerics,
  OddRule,
  Overlap,
  PhaseMethod,
  RatePlan,
  Rates,
  Sampling,
  Span,
  Stepped,
  AbB,
  AbConfig,
  AbMeasuring,
  AbSide,
  AbStatus,
  AppRule,
  Band,
  BandSpec,
  BlockedVolumeChange,
  ChainConfig,
  ChannelEq,
  ConvolutionConfig,
  CrossfeedConfig,
  CrossfeedLevel,
  CrossfeedMode,
  Cutoff,
  CutoffMode,
  DcBlockConfig,
  DeviceCapabilities,
  DeviceInfo,
  DeviceLostAction,
  DitherKind,
  EngineConfig,
  EngineLimits,
  EngineState,
  EqProfile,
  FilterDesign,
  FilterErrorBudget,
  FilterPhase,
  FilterPreset,
  FilterType,
  FollowerConfig,
  FollowerState,
  FollowerStatus,
  FormatCapability,
  GuardMode,
  HarmonicsCharacter,
  HarmonicsConfig,
  Length,
  LevelMatch,
  LimiterConfig,
  LimiterMode,
  LimiterStatus,
  Meters,
  NowPlaying,
  OutputConfig,
  OutputsConfig,
  OutputsStatus,
  OverloadAction,
  OverloadReport,
  Passthrough,
  PassthroughBreak,
  PassthroughCaveat,
  Phase,
  PitchConfig,
  PitchQuality,
  Preset,
  PresetInfo,
  PresetState,
  RateCapability,
  RateConflict,
  RatePolicy,
  BypassResampler,
  BypassResamplerStatus,
  RateRatio,
  ResponseCrossing,
  ResponseWindow,
  ResponseWindowRequest,
  RateTarget,
  RouteTarget,
  RoutingMode,
  RpcError,
  SampleFormat,
  SelfTestResult,
  SinkChoice,
  SinkVolumePolicy,
  Spectrum,
  Status,
  StreamInfo,
  StreamVolume,
  StreamVolumeLimit,
  TapsUnit,
  TiltConfig,
  TiltPivot,
  TiltStatus,
  Topic,
  UpsamplerConfig,
  UpsamplerResponse,
  VarispeedConfig,
  VarispeedStatus,
  VolumeConfig,
  VolumeGuard,
  VolumeMode,
  VolumeStatus,
  Window,
  WindowKind,
} from "./generated";

/** `set_varispeed` params: each field optional, omitted ones keep their value. */
export type SetVarispeedParams = Extract<G.Call, { method: "set_varispeed" }>["params"];
/** `set_stream_volume` params: one stream's level and/or mute. */
export type SetStreamVolumeParams = Extract<G.Call, { method: "set_stream_volume" }>["params"];
/** `set_volume` params: level, balance, mute (unconditional, the CLI's call). */
export type SetVolumeParams = Extract<G.Call, { method: "set_volume" }>["params"];
/** `set_volume_guarded` params: the same, conditional on `if_instance` and `if_revision`. */
export type SetVolumeGuardedParams = Extract<G.Call, { method: "set_volume_guarded" }>["params"];

/** Every daemon event (`event` + `data`), as pushed after `subscribe`. */
export type DaemonEvent = G.Event;

/** Mirrors `VOLUME_MIN_DB` / `VOLUME_MAX_DB` / `BALANCE_MAX_DB` in crates/protocol/src/ipc.rs (`set_volume` validation). */
export const VOLUME_MIN_DB = -120;
export const VOLUME_MAX_DB = 0;
export const BALANCE_MAX_DB = 24;

/** Mirrors `DC_BLOCK_CUTOFF_MIN_HZ` / `DC_BLOCK_CUTOFF_MAX_HZ` in crates/protocol/src/lib.rs (`dc_block` validation). */
export const DC_BLOCK_CUTOFF_MIN_HZ = 1;
export const DC_BLOCK_CUTOFF_MAX_HZ = 20;

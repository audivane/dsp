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
  AbConfig,
  AbSide,
  AbStatus,
  AppRule,
  Band,
  ChainConfig,
  ChannelEq,
  ConvolutionConfig,
  CutoffMode,
  DeviceInfo,
  DitherKind,
  EngineConfig,
  EngineState,
  EqProfile,
  FilterPhase,
  FilterType,
  LevelMatch,
  Meters,
  NowPlaying,
  OutputConfig,
  Passthrough,
  PassthroughBreak,
  PassthroughCaveat,
  Preset,
  PresetInfo,
  PresetState,
  RateConflict,
  RatePolicy,
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
  Spectrum,
  Status,
  StreamInfo,
  Topic,
  UpsamplerConfig,
  UpsamplerResponse,
  VolumeConfig,
  VolumeStatus,
} from "./generated";

/** `set_volume` params: the same three fields as the persisted `VolumeConfig`. */
export type SetVolumeParams = G.VolumeConfig;

/** Every daemon event (`event` + `data`), as pushed after `subscribe`. */
export type DaemonEvent = G.Event;

/** Mirrors `VOLUME_MIN_DB` / `VOLUME_MAX_DB` / `BALANCE_MAX_DB` in crates/protocol/src/ipc.rs (`set_volume` validation). */
export const VOLUME_MIN_DB = -120;
export const VOLUME_MAX_DB = 0;
export const BALANCE_MAX_DB = 24;

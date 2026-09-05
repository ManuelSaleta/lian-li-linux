# RGB Mode Capabilities and Per-Mode Option Exposure

Status: planned, not started
Date: 2026-09-04
Decided by: sgtaziz (maintainer)
Research: full codebase review plus decompiled L-Connect 3 analysis at `/home/aziz/Code/L-Connect-Linux/Decompiled/`

## How to use this document

This is a complete handoff for implementing the effort described in "The plan".
All file paths and line numbers refer to this repository unless prefixed with
`Decompiled/`, which refers to `/home/aziz/Code/L-Connect-Linux/Decompiled/`.
Line numbers were accurate at planning time, commit `85ae53e` plus merged PRs
#152 #175 #176 #178. Re-verify with grep before editing.

## Background

Triggered by issue #167 (wireless TL fans "lost Runway"). Investigation
conclusion, verified from the decompile: **wireless fans have no on-firmware
effect engine**. L-Connect renders every effect host-side into frame buffers
(`RgbEffect` fills `rgb_data = new byte[bpp * led_num * total_frame]`,
Decompiled/lianli.slv3/slv3/RgbEffect.cs:4211) and uploads them once per effect
change over RF (cmd 32 `RF_RgbSync`, header packet carries total_frame,
led_num, interval, sub_interval, then 220 byte payload chunks,
Decompiled/lianli.slv3/slv3/MasterDevice.cs:940-1043). The device loops the
uploaded frames autonomously. The firmware is a frame player, not an effect
engine. Our daemon already implements this transport
(`send_rgb_frames` / `SetRgbFrames` IPC, crates/lianli-devices/src/wireless/rgb.rs:35).
What is missing on our side is only the host-side effect renderer, and most
TL/CL/SL-INF frame content in L-Connect comes from the closed
`WitmodLightings.dll`, so byte parity with Windows is impossible anyway.
Issue #167 was therefore closed as not a regression: wireless caps have said
Static/Direct since commit f8d35c3 (2026-02-24) and the GUI has filtered by
caps since the architecture rewrite a8a9231 (2026-08-01), before v0.8.2.

## Decisions already made

1. Wireless host-rendered animated effects are OUT OF SCOPE. Wireless stays at
   `[Static, Direct]`. The dead `WirelessRgbDevice` adapter gets deleted. A
   future renderer would be procedural approximations feeding the existing
   `send_rgb_frames` pipeline.
2. Wired receiver Top/Bottom scopes: IMPLEMENT scope honoring (not just stop
   advertising).
3. Merge lighting GUI exposure: INCLUDE in this effort (wired ENE hub and
   Strimer only; the decompile's wireless cross-device merge is not implemented
   daemon-side and stays out).
4. Unknown or unsupported mode on apply: the driver `set_zone_effect` bails
   with a clear error; `apply_config` logs the per-zone error and continues
   with the remaining zones. Replaces today's silent degradation.
5. Color counts up to 6 are supported (ENE SL-V2/AL-V2 and Strimer Complete
   use 6 in L-Connect). `RgbEffect.colors` is already a variable-length Vec;
   the GUI palette cap moves from 4 to mode-driven up to 6.

## Current state catalog

### RgbDevice implementations (crates/lianli-devices/src/traits.rs:264 defines the trait)

| Driver | File | Modes | Zones | Flags |
|---|---|---|---|---|
| TlFanPortDevice | tl_fan/port_rgb.rs:56 | 29 listed, but `Direct` is missing from the list although `set_zone_effect` handles it | Fan N, 20 LEDs each | direction yes, mb_sync yes, scopes All/Top/Bottom per fan |
| WinUsbLedDevice (Universal Screen 8.8 ring) | winusb/led.rs:132 | Off, Static, Direct | Ring, 60 LEDs | direct yes |
| WiredReceiverController | winusb/wired_receiver.rs:597 | Off, Static, Direct | Fan N, 26/44/9/52/24 LEDs by PID | direct yes; scopes advertised as one `[All,Top,Bottom]` entry when `compresses_rgb` but `set_zone_effect` ignores scope entirely; `set_light_sync_mb` (0x14, wired_receiver.rs:326) exists with zero call sites |
| Hs2OledLedController | winusb/hs2_oled_led.rs:284 | Off, Static, Direct | Ring, 45 LEDs | direct yes |
| H2AioController (HydroShift II ring) | winusb/h2_aio.rs:589 | Off, Static, Direct | Ring, 24 LEDs | direct yes; ignores effect.speed (fixed 100ms) |
| StrimerPlusController | strimer_plus.rs:301 | 25 modes | Port N, up to 27 LEDs, 12 zones | mb_sync yes, merge yes; unknown modes silently map to byte 1 Static in `map_mode` (strimer_plus.rs:271) |
| Ene6k77GroupDevice | ene6k77/group_rgb.rs:30 | per model: base 20, AlV2Fan 24, SlInfinity 27, single-ring 14, v2 18 | Fan N, 16 or 20 LEDs | mb_sync yes, merge yes, dual-ring scopes All/Inner/Outer; `set_zone_effect` ignores the zone argument (group-wide) |
| Galahad2TrinityController | galahad2_trinity.rs:457 | 17 | Pump Head 24 + Fans 24 | mb_sync yes, pump scopes All/Inner/Outer; unknown modes fall back via `to_hydroshift_lcd_mode_byte().unwrap_or(...)` |
| AioLcdRgbController (HydroShift LCD family) | hydroshift_lcd/rgb.rs:97 | Galahad2Vision 7, others 18 | Pump Head 24 (some variants) + Fans 24 | mb_sync yes; unknown modes fall back to mode byte 3 |
| WirelessRgbDevice | wireless/adapters/mod.rs:202 | 24 modes listed | unnamed single zone | DEAD CODE, never constructed; `set_zone_effect` is a stub whose comment describes a nonexistent "wireless effect-rendering thread" |

### Capabilities aggregation (crates/lianli-daemon/src/controllers/rgb/mod.rs)

`capabilities()` at mod.rs:465. Wired devices pass through the trait fields.
Wireless devices are built from `wireless_state` with
`supported_modes: vec![RgbMode::Static, RgbMode::Direct]` hardcoded at
mod.rs:514 (line number as of the planning commit; grep for
`vec![RgbMode::Static, RgbMode::Direct]`). Wireless zones: total_led_count_override
types get Case Ring / Screen Ring / LED Strip single zones, AIOs get Pump Head
plus Fan N zones, otherwise Fan N zones.
`RgbDeviceCapabilities` lives in crates/lianli-shared/src/rgb.rs:681 with
fields: device_id, device_name, supported_modes, zones, supports_direct,
supports_mb_rgb_sync, total_led_count, supported_scopes (Vec<Vec<RgbScope>>,
per zone), supports_direction, supports_merge_lighting (both serde default).

### Wireless effect path today

`set_effect` (mod.rs:242, wireless branch) renders solid color only via
`render_zone_color` (mod.rs:743) then `send_rgb_direct`. Animations reach
wireless devices only through the separate `SetRgbFrames` IPC (ipc/rgb.rs:54,
mod.rs:439). Drift detection re-sends on n_index change
(controllers/fan.rs:248 polls `rgb_drifted`, service/mod.rs:681 ResyncWirelessRgb).

### GUI data flow (crates/lianli-gui/src)

GetRgbCapabilities IPC (ipc/rgb.rs:13) → stores/config.ts `rgbCaps` ref (line 49),
`rgbCapsFor` (line 123) → views/RgbPage.vue → components/rgb/RgbDeviceCard.vue →
components/rgb/RgbZoneEditor.vue. Mode dropdown is already fully capability
driven: `cap.supported_modes.map(modeLabel)` (RgbZoneEditor.vue:105), labels in
constants.ts `RGB_MODES` (lines 104-181) with raw-string fallback. Write path:
`patchEffect` mutates the config mirror only, AppHeader Save → SetConfig →
daemon `RgbController::apply_config` (mod.rs:154). A live IPC path exists
(stores/rgb.ts `sendEffect`/`scheduleEffect` → SetRgbEffect) but no component
calls it.

Per-effect controls today are mode-agnostic: up to 4 colors (user managed),
speed slider always 0-4, brightness ladder always (Off plus 0-4,
constants.ts RGB_BRIGHTNESS:204), direction select with all 6 options when
`cap.supports_direction`, scope select from `cap.supported_scopes[zoneIndex]`.
Special cases keyed on mode identity: `Direct` hides palette, Off/Static/Direct
plus All scope skips the propagate-to-all-zones prompt (RgbZoneEditor.vue:73),
MB sync forces Static (RgbDeviceCard.vue:74).

There is NO per-mode capability metadata in the protocol today. Adding a mode
requires: Rust enum plus display_name plus from_display_name plus per-driver
mode byte tables (lianli-shared/src/rgb.rs), and a constants.ts RGB_MODES entry
(optional, fallback shows raw variant name).

### Known mismatches and dead code (from the review)

1. Wireless caps hardcode Static/Direct while the dead adapter claims 24 modes.
2. WirelessRgbDevice never instantiated; its comment misleads (contributed to
   the confusion in issue #167).
3. Wired receivers hide MB ARGB sync: hardware command exists, trait flag
   never set, method never called.
4. Receiver `supported_scopes` returns one entry instead of per-zone and
   `set_zone_effect`/`set_direct_colors` never honor `effect.scope`.
5. TL ports handle Direct but do not advertise it.
6. Merge lighting: `MergeLightingConfig` (lianli-shared/src/rgb.rs:703, fields
   device_order, directions, effect, disabled_devices) is persisted via
   SetMergeLightingConfig/GetMergeLightingConfig (ipc.rs:152, server.rs:232)
   but nothing ever applies it; ENE `start_merge`/`set_merge_order([u8;4])`/
   `send_merge_command` (ene6k77/controller.rs:484-507) and Strimer
   start/stop (strimer_plus.rs:405) are unreachable; GUI never shows
   supports_merge_lighting and the TS RgbAppConfig omits merge_lighting.
7. Unknown modes silently degrade (Strimer map_mode to 1, HydroShift and
   Galahad2 unwrap_or fallbacks to byte 3).
8. ENE set_zone_effect ignores the zone argument (group-wide effect). Existing
   accepted behavior, GUI already propagates zone 0. Out of scope.
9. H2AioController ignores effect.speed. Out of scope unless trivial.
10. Decompile note: L-Connect's sync engine treats the HS2 OLED ring as 35
    LEDs while our driver uses 45 (RgbEffect.GetHS2Data). Worth re-verifying
    against hardware someday, not part of this effort.

## Decompiled per-effect option matrices

Speed semantics everywhere: 5 levels. ENE UI 0-100 in 5 stops mapping to
Lowest(2)/Lower(1)/Normal(0)/Faster(255)/Fastest(254)
(Decompiled/L-Connect.Core Ene6K77FanController.cs:149). TL, Galahad2,
HydroShift LCD use byte 0-4 (value/25). Frame receivers use L1-L5
(0/25/50/75/100). Brightness: 5 levels everywhere, applied host-side by
scaling colors at render time for wireless frames. Direction enums: ENE
Left/Right only, TL and GA2 and HS LCD six-way, frame receivers CW/CCW only,
8.8 and HS2 OLED Forward/Reverse only.

Color count 0 means the effect ignores user colors (fixed palette, hide the
pickers). "No speed" means the effect ignores speed (hide the slider, Static
family). Direction listed as none means no direction control.

### ENE6K77 wired hub (from Decompiled L-Connect 3 product profiles)

SL base (SLFanProfile.cs, also Redragon identical). Merge modes Runway,
Meteor. Single ring, no scopes.

| Effect | Colors | Speed | Direction |
|---|---|---|---|
| Rainbow | 0 | yes | Right |
| RainbowMorph | 0 | yes | none |
| StaticColor | 4 | no | none |
| Breathing | 4 | yes | none |
| ColorCycle | 3 | yes | Left |
| Runway | 2 | yes | none |
| Staggered | 2 | yes | none |
| Tide | 2 | yes | none |
| Meteor | 2 | yes | none |
| Mixing | 2 | yes | none |
| Stack | 1 | yes | Left |
| StackMulti | 0 | yes | Left |
| Neon | 0 | yes | none |

SL-V2 (SLV2FanProfile.cs): same as SL plus Voice 0, Groove 1 Right, Render 4
Right, Tunnel 4; StaticColor and Breathing use 6 colors. Merge: Runway, Tide,
Meteor, Mixing, StackMulti.

SL-Infinity (SLInfinityProfile.cs), scopes All/Inner/Outer with independent
lists. Main 19: Rainbow 0 CW, RainbowMorph 0, StaticColor 4 no-speed,
Breathing 4, BreathingRainbow 0, Runway 2 no-dir, MopUp 4, Meteor 4 CW,
Warning 4, Voice 2 CW, Mixing 2, Stack 2 CW, Tide 4, Scan 2, Door 4,
HeartBeat 1, HeartBeatRunway 1 CW, Disco 4 CW, ElectricCurrent 4. Inner adds
Taichi 2 CW, ColorCycle_Inner 4 CW, MeteorRainbow 0 CW, Lottery 2 CW,
DoubleMeteor 4, MeteorContest 2 CW, MeteorMix 2, ReturnArc 4 CW, DoubleArc 4
CW, Scan_Inner 1. Outer adds ColorCycle_Outer 4 CW, MeteorRainbow_Outer 0 CW,
ColorfulMeteor_Outer 0 CW, Lottery_Outer 2 CW, Reflect_Outer 1.

AL (ALFanProfile.cs), merge Scan and Contest. Main 18: Rainbow 0 CW,
RainbowMorph 0, StaticColor 4, Breathing 4, Taichi 2 Left, ColorCycle 4 Right,
Runway 2 no-dir, Meteor 4 CW, Warning 4, Voice 4 CW, SpanningTeacups 4 Left,
Tornado 4 CW, Mixing 2, Stack 2 Left, Staggered 4, Tide 4, Scan 2, Contest 3
CW. Inner adds PacMan 2 CW, MeteorRainbow 0 CW, Lottery 2 CW, Wave 1, Spring 4
CW, TailChasing 4 CW. Outer adds StaticColorful 4, BreathingColorful 4, plus
the same Outer additions as SL-Infinity.

AL-V2 (ALV2FanProfile.cs): AL plus Wave 1, Spring 4 CW, TailChasing 4 CW,
ColorfulCity 0, Render 4 CW, ElectricCurrent 4, Twinkle 0. StaticColor and
Breathing 6 colors, ColorCycle 4. Merge: Runway, MopUp, Wave, Spring,
TailChasing, Mixing, Tide, Scan, Contest, ElectricCurrent.

### TL fans wired (TLFanProfile.cs), scopes All/Top/Bottom

Full ring 14: Rainbow 0 CW, StaticColor 2 no-speed, Runway 2 no-dir, Meteor 4
CW, ColorCycle 3 CW, Render 4 CW, TailChasing 4 CW, Stack 4 CW, CoverCycle 4
CW, Wave 4 no-dir, Racing 2, Lottery 2 CW, Intertwine 2, MeteorShower 0 CW.
Our driver implements the full-ring path (0xB0 group light and 0xA3 per fan).

### Strimer Plus wired (StrimerPlusProfile.cs), scope Complete/Individual

Individual per port 13: Rainbow 0 Right, Wave 6 Right, StaticColor 1 no-speed,
Breathing 1, RainbowMorph 0, Snooker 6, Mixing 2, PingPong 6, Paint 6, Runway
2 no-dir, Tide 6, BlowUp 6, Meteor 6 CW. Our driver is per-port zones, so use
the Individual table. Complete (per channel) 11 effects use 6 colors each
except BulletStack 0 and Twinkle 0.

### Galahad II Trinity (Galahad2TrinityProfile.cs), pump scopes Inner/Outer/All, fans no scope

Fan 16: Rainbow 0 CW, RainbowMorph 0, StaticColor 1 no-speed, Breathing 1,
Runway 2 no-dir, Meteor 4 CW, Vortex 4 CW, CrossingOver 4 CW, TaiChi 2 CW,
ColorfulStarryNight 0, StaticStarryNight 1, Voice 1 CW, BigBang 4, Pump 1 CW,
ColorsMorph 0 CW, Bounce 4. Pump All 15: same but StaticColor, Breathing,
Voice, Pump are 2 colors, no Bounce. Pump Inner/Outer: those four are 1 color.

### HydroShift LCD family (HydroShiftLCDProfile.cs), no scopes

Rainbow 0 CW, RainbowMorph 0, StaticColor 1 no-speed, Breathing 1, Runway 2
no-dir, Meteor 4 CW, TickerTape 4 CW, Fluctuation 4, Transmit 2 CW,
ColorfulStarryNight 0, StaticStarryNight 1, Voice 1, BigBang 4, Burst 1,
ColorsMorph 0, Bounce 4. Galahad2Vision variant only supports 7 modes (Off,
Static, Rainbow, RainbowMorph, Breathing, Runway, Meteor).

### HydroShift II wireless ring (H2Effects), for reference only, wireless out of scope

11 effects, see HydroShiftIISubProfile.cs:149-245.

### Wired frame receivers (USB WinUSB receivers driving wireless fan hardware)

Shared UI default matrix in Decompiled LWirelessProfile.cs:362-542. Per
receiver differences from the defaults, relevant to our driver tables
(Static and Direct only today, so this mostly informs future work):
TL Flex StaticColor 4 no-speed, ColorCycle 3, Runway no-dir, Voice 0,
Kaleidoscope 0, Twinkle 0. SL-INF and CL and P28: ColorCycle 4, StaticColor 4
no-speed, Scan 1, HeartBeat 1, CandyBox 0. SL V4: Runway HAS CW here,
ColorCycle 3, Pioneer 1, GradientRibbon 0.

### Universal Screen 8.8 and HS2 OLED Curve

19 modes each (Rainbow, Wave, StaticColor, Breathing, RainbowMorph, Paint,
Runway, Tide, BlowUp, Meteor, Snooker, Mixing, PingPong, BulletStack, Twinkle,
River, Hourglass, ElectricCurrent, RainbowWave). L-Connect reads per-effect
options from the device at runtime. Our drivers implement only frame-based
Off/Static/Direct, so tables are trivial for this effort; the 19-mode list is
input for a possible future firmware-effect integration.

## The plan

Phase 1, shared schema, crates/lianli-shared/src/rgb.rs. Add
`RgbModeOptions { mode: RgbMode, color_count: u8, supports_speed: bool,
allowed_directions: Vec<RgbDirection>, allowed_scopes: Vec<RgbScope> }`.
Empty allowed lists mean none. Add `mode_options: Vec<RgbModeOptions>` to
`RgbDeviceCapabilities` with serde default. Keep `supported_modes` for
OpenRGB and old-GUI compatibility. Audit color normalization paths for the 4
to 6 raise: preset parsing, OpenRGB controller, strimer_plus normalize_colors.

Phase 2, drivers, crates/lianli-devices. Add trait method
`fn mode_options(&self) -> Vec<RgbModeOptions>` defaulting to a generic
table in shared, overridden per driver with the matrices above. Verify during
implementation that the ENE wire path can actually carry 6 colors for SL-V2
and AL-V2 before advertising 6. Truth fixes: add Direct to TL ports
supported_modes (tl_fan/port_rgb.rs:61), make unknown modes bail in strimer
map_mode (strimer_plus.rs:271) and the HydroShift and Galahad2 unwrap_or
fallbacks, delete WirelessRgbDevice (wireless/adapters/mod.rs:202). Receiver
scope honoring (wired_receiver.rs): per-model scope sets, TL Flex 0x0101 and
0x0102 get All/Top/Bottom (26 LEDs = 13 plus 13), SL-INF Flex 0x0103 and 0x0104
and SL V4 0x0106 get All/Inner/Outer (44 or 52 split halves), P28 V2 0x0105
and CL V2 0x0107 get none. Fix supported_scopes to one entry per zone. Honor
effect.scope in set_zone_effect and set_direct_colors by painting the
matching half of each fan slice. HARDWARE CHECK NEEDED: which half of the
buffer is Top or Inner. Receiver MB sync: implement the trait
set_mb_rgb_sync on top of the existing dead set_light_sync_mb (0x14) and set
supports_mb_rgb_sync true.

Phase 3, daemon, crates/lianli-daemon. capabilities() passes wired
mode_options through and gives wireless explicit options (Static: 1 color, no
speed. Direct: no palette, no speed). apply_config: per-zone errors logged,
apply continues. Merge lighting orchestration: add
`RgbController::apply_merge_lighting(&MergeLightingConfig)` that starts or
stops merge on participating ENE groups (start_merge plus set_merge_order
from device_order, group_rgb.rs:189 and controller.rs:484) and Strimer ports
(strimer_plus.rs:405), applies the shared effect to participants, respects
disabled_devices and directions where the drivers support them. Call it from
apply_config and on startup when the config contains merge_lighting.

Phase 4, GUI, crates/lianli-gui. types/index.ts: add RgbModeOptions, extend
RgbDeviceCapabilities, add MergeLightingConfig and RgbAppConfig.merge_lighting
(currently missing from the TS type). RgbZoneEditor.vue: palette count from
color_count (0 hides pickers, add button capped at color_count, maximum 6),
speed hidden when not supports_speed, direction options filtered to
allowed_directions, scope options as per-zone scopes intersected with
per-mode allowed_scopes. Fall back to today's generic controls when
mode_options is empty (old daemon). New merge lighting section in RgbPage.vue:
enable toggle, device order, shared effect editor reusing RgbZoneEditor,
per-device direction, wired to Set/GetMergeLightingConfig and saved through
the normal config flow.

Phase 5, verification. Rust unit tests: every supported_modes entry has a
mode_options entry per driver; receiver scope paint tests; apply_config
continues after an unknown mode. cargo check, test, clippy, fmt. GUI
typecheck and build. Manual hardware matrix where hardware is available.
Explicit hardware checks: TL Flex half order, ENE 6 color capacity, receiver
MB sync on a real receiver.

## Suggested PR split

1. PR A: Phase 1 plus Phase 2 tables and truth fixes. No behavior change
   beyond honest caps.
2. PR B: receiver scope honoring plus receiver MB sync (hardware sensitive).
3. PR C: Phase 3 daemon work including merge orchestration.
4. PR D: Phase 4 GUI, depends on PR A.

## Explicitly out of scope (future work)

- Wireless host-rendered animated effects (procedural renderers feeding
  send_rgb_frames, no Windows parity possible due to closed WitmodLightings).
- Wireless cross-device merge lighting (decompile UIEffects path).
- ENE per-fan addressing (hardware is group-wide).
- Firmware effect modes on the 8.8 inch and HS2 OLED rings (19-mode lists
  captured above as reference).
- HS2 OLED LED count discrepancy (45 vs L-Connect 35).

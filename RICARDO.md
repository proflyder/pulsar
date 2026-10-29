# Ricardo profile of the Rust Pulsar fork

This fork is the Ricardo Rust profile of Pulsar for the Station Midi
installation. The station currently runs the earlier Rust build
`v0.5.0-1-g908a027`; the upstream `v0.5.2` merge in this checkout has only
passed host-side checks and is not installed until live acceptance.

Base: `Morvex885/pulsar` `v0.5.2` (`4cba44a`), merged into `feat/ricardo-rust`
(previous tip `908a027` kept as a parent), branch `feat/ricardo-rust`.

## What this profile keeps

- ESPHome Native API satellite and Sendspin / Music Assistant endpoint.
- Spotify Connect and phone → speaker transfer through the existing HA/MA
  routing.
- Native Yandex VQE, stock command spotter (`стоп`, `громче`, `тише`,
  `пауза`, `продолжи`, `дальше`, `назад`).
- The stock «Алиса» / «Яндекс» spotters as a rollback path; the HA API still
  advertises them, but the default is `ricardo`.
- Hardware mute, action button, ring, clock, volume curve, timer/alarm
  announcements and duck/resume.

## What is changed for this installation

| Area | Change |
|---|---|
| Wake word | New on-device «Рикардо» detector, same microWakeWord model as the deployed fork build; `wake_word=ricardo` is the default. |
| Pre-roll | The wake event carries 0.5 s (16 chunks, 512 ms) of VQE-cleaned audio taken from the shared 1.024 s ring (upstream `wake_preroll` rule `Fixed(16)` for the `ricardo` id), so «Рикардо, включи свет» does not lose its first syllables. The upstream `WakePreroll` switch and `wake_preroll=` key control it. |
| Silent cancel | `stt-no-text-recognized` is treated as a cancel, not as an error ring. |
| `стоп` | The local command spotter is off during LISTENING/PROCESSING/arbitration. «Рикардо, стоп» reaches HA, which stops the alarm and restores the music. |
| Local commands | The command spotter runs only while something plays and outside a voice request, matching the deployed fork profile rather than upstream's always-on behavior. |
| Nearest station | Default `nearest_answers=false`: this installation has one station and the 250 ms arbitration window only adds latency. It can be turned back on in HA. |
| Zigbee | Runtime initialization is disabled (`ENABLE_ZIGBEE=false`); the radio and the `zigbee-*.ybl` assets are left untouched. |
| Updates | `UPDATE_REPO=proflyder/pulsar`, `auto_update=false` by default. The upstream repository is never an update source. |
| Hardware | Midi remains the tested target. Lite 2 / Mini 2 code is still compiled by upstream but is not part of this profile's acceptance. |

The custom detector lives in `satellite/crates/pulsar-audio/ricardo/` and is
linked through a small C ABI. It uses the firmware's `libtflite.so` and the
vendored TensorFlow Lite microfrontend; the model is not committed (see
`satellite/wakewords/README.md`).

## Build

Linux or WSL2, Zig 0.14.1, `cargo-zigbuild` 0.24.0:

```bash
cd satellite
make check
make ARCH=armhf ZIG=/path/to/zig-0.14.1 binary
```

The active Midi uses the armhf userland. The output is
`satellite/build/armhf/pulsar` (stripped) and `pulsar.debug`.

Put the model here before an install:

```bash
cp /path/to/ru_ricardo_mww.tflite satellite/wakewords/
```

`pulsar-install` copies it to `/data/pulsar/wakewords/` with a SHA-256 check.
Without the model the daemon starts but logs that the Ricardo detector is
missing; it does not silently fall back to «Алиса».

## Verification performed

All checks were host-side; no binary was installed and no station was touched.

- `cargo fmt --all --check` (macOS host, Rust 1.99.1): pass.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` in
  `rust:1.99.1-bookworm`, `linux/arm64`: pass.
- `cargo test --workspace --exclude pulsar-update` in the same container: all
  suites pass, including the new Ricardo pre-roll tests
  (`ricardo_preroll_rule_uses_the_custom_id`,
  `ricardo_preroll_is_the_last_512_ms_of_the_ring`,
  `preroll_off_is_empty_but_the_wake_hold_is_separate`).
- `cargo test -p pulsar-update`: 19 passed, 7 failed on arm64. The tests in
  upstream hard-code the x86_64 asset name `pulsar-host.bin` while the binary
  reports `pulsar-aarch64.bin`; an export of the unmodified upstream `v0.5.2`
  (`4cba44a`) shows the same 7 failures on arm64, so this is a pre-existing
  upstream test limitation, not a merge regression.
- `make ARCH=armhf ZIG=…/zig-0.14.1 binary`: the daemon compiles and links as
  an ARM EABI5 armhf ELF (`build/armhf/pulsar.debug`); the last
  `zig objcopy --strip-all` step fails on the macOS host (undefined symbols in
  zig's objcopy), so no stripped `build/armhf/pulsar` was produced here. The
  Linux CI job performs the same strip step.

Not performed: live microphone/wake-word acceptance, real `libtflite.so`
loading on the station, ESPHome entity regression, Spotify/MA playback, or a
rollback test. Those remain a separate deployment gate.

## Rollback

The previous Rust binary and its model are untouched. If this merge is ever
installed, keep:

```text
/data/pulsar/pulsar.prev
/data/pulsar/state.conf
/data/pulsar/wakewords/ru_ricardo_mww.tflite
```

and restore `pulsar.prev` with `ctl.sh restart`. The previous Rust build also
accepts the same `wake_word=ricardo` state and model path.

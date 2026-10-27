# Ricardo profile of the Rust Pulsar fork

This fork is the **not-yet-deployed** Rust version of Pulsar for the
Station Midi installation. The active station still runs the C++ build
`v0.4.1-ricardo6`; nothing here is installed on it.

Base: `Morvex885/pulsar` `v0.5.0` (`1934b68`), branch `feat/ricardo-rust`.

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
| Wake word | New on-device «Рикардо» detector, same microWakeWord model as `v0.4.1-ricardo6`; `wake_word=ricardo` is the default. |
| Pre-roll | 0.5 s of VQE-cleaned audio is held and sent with the wake event, so «Рикардо, включи свет» does not lose its first syllables. |
| Silent cancel | `stt-no-text-recognized` is treated as a cancel, not as an error ring. |
| `стоп` | The local command spotter is off during LISTENING/PROCESSING/arbitration. «Рикардо, стоп» reaches HA, which stops the alarm and restores the music. |
| Local commands | The command spotter runs only while something plays and outside a voice request, matching `v0.4.1-ricardo6` rather than upstream's always-on behavior. |
| Nearest station | Default `nearest_answers=false`: this installation has one station and the 250 ms arbitration window only adds latency. It can be turned back on in HA. |
| Zigbee | Runtime initialization is disabled (`ENABLE_ZIGBEE=false`); the radio and the `zigbee-*.ybl` assets are left untouched. |
| Updates | `UPDATE_REPO=proflyder/pulsar`, `auto_update=false` by default. The upstream repository is never an update source. |
| Hardware | Midi remains the tested target. Lite 2 / Mini 2 code is still compiled by upstream but is not part of this profile's acceptance. |

The custom detector lives in `satellite/crates/pulsar-audio/ricardo/` and is
linked through a small C ABI. It uses the firmware's `libtflite.so` and the
vendored TensorFlow Lite microfrontend, exactly like the C++ build; the model
is not committed (see `satellite/wakewords/README.md`).

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

- `cargo fmt --all --check`
- `cargo-zigbuild clippy --workspace --all-targets --target aarch64-unknown-linux-gnu.2.36 -- -D warnings`
- Cross-build for `aarch64-unknown-linux-gnu.2.36` and
  `armv7-unknown-linux-gnueabihf.2.30`; both binaries contain the C ABI
  symbols `mww_create` / `mww_feed` / `mww_destroy`.
- `cargo test -p pulsar --bin pulsar` (30 tests) in a Linux container.
- `cargo test -p pulsar-audio` (host) including the missing-model error path.

Not performed: live microphone/wake-word acceptance, real `libtflite.so`
loading on the station, ESPHome entity regression, Spotify/MA playback, or a
rollback test. Those remain a separate deployment gate.

## Rollback

The active C++ binary and its model are untouched. If this fork is ever
installed, keep:

```text
/data/pulsar/pulsar.prev
/data/pulsar/state.conf
/data/pulsar/wakewords/ru_ricardo_mww.tflite
```

and restore `pulsar.prev` with `ctl.sh restart`. The current C++ build also
accepts the same `wake_word=ricardo` state and model path.

# Ricardo profile of the Rust Pulsar fork

This fork is the Ricardo Rust profile of Pulsar for the Station Midi
installation. The station currently runs the earlier Rust build
`v0.5.0-1-g908a027`; the upstream `v0.5.2` / `v0.5.3` merges in this checkout
have only passed host-side checks and are not installed until live acceptance.

Base: `Morvex885/pulsar` `v0.5.3` (`504830d`), merged into `feat/ricardo-rust`
(the upstream `v0.5.2` merge stays in history), branch `feat/ricardo-rust`.
The upstream `v0.5.3` merge itself is on `feat/merge-upstream-v0.5.3` and has not
been installed or accepted on the station yet.

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
| Repeated wake | A wake word heard during LISTENING / PROCESSING ends that dialog (`voice_request_stop`) and starts a new one with the wake earcon; during SPEAKING the answer is cut and the new dialog starts. A station that loses wake arbitration does not cut its answer. Late events of a replaced started run are dropped (its RUN_END is treated as stale). |
| `стоп` | The local command spotter is off during LISTENING/PROCESSING/arbitration. «Рикардо, стоп» reaches HA, which stops the alarm and restores the music. |
| `отмена` | Recognized after the wake word as the request, it is an HA custom intent (`StationCancel`) with no speech and no side effects: no TTS answer and no Hermes fallback. The stock command model has no «отмена», so a mid-call «отмена» without a repeated wake word is not detected locally. |
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

## Релизы и деплой форка

Теги форка продолжают нумерацию upstream, чтобы `releases/latest` GitHub всегда указывал на
сборку форка: `v<upstream major.minor>.<1000+N>` — например `v0.5.1001`, `v0.5.1002`; после
merge upstream `v0.6.x` — `v0.6.1001`. Патч-номер 1000+ больше любого upstream-патча, поэтому
сравнение версий (`vMAJOR.MINOR.PATCH`) считает релиз форка новее.

1. Собрать и проверить ветку, затем поставить аннотированный тег и отправить его:

   ```bash
   git tag -a v0.5.1001 -m 'pulsar v0.5.1001'
   git push origin v0.5.1001
   ```

2. `build.yml` на тег `v*` собирает оба бинарника, dropbear, radio и установщик, публикует
   GitHub release (`--latest`) и запускает `deploy-station.yml`.
3. В Actions запуск деплоя ждёт одобрения в Environment `Station` (required reviewer).
4. После одобрения job поднимает VPN (группа `ci`), затем `hashicorp/vault-action` по
   GitHub OIDC (роль `deploy-station`) читает `secret/data/ci/deploy-station` и вызывает
   `tools/ha_firmware_deploy.py --tag <tag>`: HA `homeassistant.update_entity` → сущность
   «Firmware» видит тег → `update.install` → колонка скачивает, проверяет `SHA256SUMS` и
   `--version`, меняет бинарник (старый остаётся `.prev`) и перезапускается. Скрипт ждёт
   `installed_version == tag` и ещё 90 с следит за стабильностью; откат `supervise.sh` на
   `.prev` или недоступность сущности дольше 30 с валят job.
5. Проверить в HA: «Firmware» показывает установленную версию, колонка онлайн; при сомнениях —
   `ctl.sh log` на станции.

Повторный деплой уже опубликованного релиза — вручную: Actions → `deploy-station` → Run
workflow, `tag` = существующий стабильный тег; в «Use workflow from» выбрать этот же тег
(`gh workflow run deploy-station.yml --ref v0.5.1001 -f tag=v0.5.1001`): Environment пускает
только теги `v*`. Если версия уже стоит, скрипт печатает
`already installed` и выходит успешно.

Откат через dispatch **невозможен**: обновлятор предлагает только более новые версии, поэтому
старый тег не появится в `latest_version`. Откат делается на станции: `.prev` (или резервная
копия бинарника) и `ctl.sh restart`, подробности и команды — в `docs/OPERATIONS.md`
монорепозитория (раздел про станцию). После отката автообновление не поставит сломанный тег
повторно (маркер `update_attempt`).

Что должно быть настроено в GitHub (значения не хранятся в репозитории):

- Environment `Station`: required reviewer, deployment tag rule `v*`;
- его secrets: `VPN_SERVER`, `VPN_CERT_P12_B64`, `VPN_CERT_PASSWORD`, `OPENBAO_ADDR`,
  при необходимости `OPENBAO_ROLE` (по умолчанию `deploy-station`);
- в OpenBao роль `deploy-station` для `auth/github` и секрет `secret/data/ci/deploy-station`
  с ключами `ha_url`, `ha_token`, `entity_id` (сущность `update.<station>_firmware`).

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

//! pulsar: Home Assistant voice satellite for Yandex Stations (Midi, Lite 2).
//! usage: pulsar [--version | state_file]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod controller;
#[allow(unsafe_code)]
mod crash;
mod settings;
#[allow(unsafe_code)]
mod sys;
mod version;

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use log::{error, info};
use pulsar_core::actors;

use controller::Controller;
use version::VERSION;

const DEFAULT_STATE: &str = "/data/pulsar/state.conf";

// Teardown (Sendspin threads, GStreamer, ALSA) has hung before: give it a few seconds, then
// leave with the status main was about to return (0 = deliberate exit for supervise.sh).
const TEARDOWN_SECS: u64 = 5;

fn arm_teardown_watchdog(status: i32) {
    let spawned = std::thread::Builder::new()
        .name("teardown-wd".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(TEARDOWN_SECS));
            sys::write_stderr(b"teardown did not finish in 5 s, _exit\n");
            sys::exit_now(status);
        });
    if let Err(e) = spawned {
        error!("teardown watchdog: {e}");
    }
}

fn main() -> ExitCode {
    let arg = std::env::args_os().nth(1);
    if arg.as_deref() == Some(OsStr::new("--version")) {
        // println! would panic (abort) on a closed pipe.
        let _ = writeln!(std::io::stdout(), "pulsar {VERSION}");
        return ExitCode::SUCCESS;
    }
    // The controller persists its settings there.
    let state = arg.map_or_else(|| PathBuf::from(DEFAULT_STATE), PathBuf::from);

    pulsar_core::init_logger();
    crash::install();
    let Some(model) = pulsar_core::model::detect_model() else {
        return ExitCode::from(1);
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let local = tokio::task::LocalSet::new();
    let status = local.block_on(&rt, async {
        // Sampled at exit, not here: `ctl.sh disable` removes the marker before stopping us.
        let restart_marker = state
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("enabled");
        let mut ctl = match Controller::start(model, state) {
            Ok(c) => c,
            Err(e) => {
                error!("{e}");
                arm_teardown_watchdog(1);
                return 1;
            }
        };
        info!(
            "pulsar {VERSION} running on {} ({})",
            model.model_name, model.codename
        );
        ctl.run().await;
        // `enabled` is what supervise.sh restarts on: promise MA a reconnect only when this is
        // a restart, not a deliberate ctl.sh disable (it removes enabled first) or a manual run.
        if supervisor_restarts(&restart_marker) {
            ctl.promise_sendspin_restart();
        }
        arm_teardown_watchdog(0);
        // Nothing changes the settings after run: a change still pending is written once, here.
        // Inside the watchdog, so a hung fsync cannot keep the daemon from exiting (a write cut
        // short leaves the previous state.conf).
        ctl.flush_settings();
        // The services stop as their handles go (the front-end joins its threads here).
        drop(ctl);
        0
    });

    local.block_on(&rt, actors::join_all(Duration::from_secs(3)));
    {
        // Actors still running are dropped inside the runtime, which their resources belong to.
        let _rt = rt.enter();
        drop(local);
    }
    // A plain drop would wait for spawn_blocking work without limit.
    rt.shutdown_timeout(Duration::from_secs(1));
    ExitCode::from(status)
}

/// Whether supervise.sh will start this daemon again (`<state dir>/enabled` exists at exit).
fn supervisor_restarts(enabled: &Path) -> bool {
    enabled.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_restart_follows_the_enabled_file_next_to_the_state() {
        let dir = std::env::temp_dir().join(format!("pulsar-restart-test-{}", std::process::id()));
        let state = dir.join("state.conf");
        let marker = dir.join("enabled");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!supervisor_restarts(&marker));
        std::fs::write(&marker, b"").unwrap();
        assert!(supervisor_restarts(&marker));
        // `ctl.sh disable`: enabled removed before the signal, so the daemon sends `shutdown`.
        std::fs::remove_file(&marker).unwrap();
        assert!(!supervisor_restarts(&marker));
        // The same state path derives the marker the supervisor reads.
        assert_eq!(
            state.parent().unwrap().join("enabled"),
            marker,
            "marker next to state.conf"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

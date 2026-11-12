//! Where akshually keeps its state, socket and log on disk.

use std::path::PathBuf;

/// Where akshually keeps its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// Persistent state (Sessions, the app log). Overridden by `AKSHUALLY_STATE_DIR`.
    pub state: PathBuf,
    /// The socket of the running app, private to the user. Overridden by
    /// `AKSHUALLY_RUNTIME_DIR`, and defaults to `state` when only that is overridden.
    pub runtime: PathBuf,
}

impl Paths {
    pub fn new(state: impl Into<PathBuf>, runtime: impl Into<PathBuf>) -> Self {
        Paths { state: state.into(), runtime: runtime.into() }
    }

    /// The platform's directories (XDG on Linux), unless the environment overrides them.
    pub fn from_env() -> Self {
        let state_override = std::env::var_os("AKSHUALLY_STATE_DIR").map(PathBuf::from);
        let runtime_override = std::env::var_os("AKSHUALLY_RUNTIME_DIR").map(PathBuf::from);
        let state = state_override.clone().unwrap_or_else(|| {
            // macOS has no state dir; it keeps app state under Application Support.
            dirs::state_dir().or_else(dirs::data_local_dir).unwrap_or_else(std::env::temp_dir).join("akshually")
        });
        let runtime = match (runtime_override, state_override) {
            (Some(runtime), _) => runtime,
            (None, Some(_)) => state.clone(),
            (None, None) => dirs::runtime_dir().map_or_else(|| state.clone(), |d| d.join("akshually")),
        };
        Paths { state, runtime }
    }

    /// The local channel's socket.
    pub fn socket(&self) -> PathBuf {
        self.runtime.join("akshually.sock")
    }

    /// Held by the running app, so only one instance serves the Sessions saved in `state`.
    /// It lives beside them, not in `runtime`: two launches that disagree on the runtime dir
    /// (one without `XDG_RUNTIME_DIR`) still find each other.
    pub fn lock(&self) -> PathBuf {
        self.state.join("app.lock")
    }

    /// Where a launched app's stdout and stderr go.
    pub fn app_log(&self) -> PathBuf {
        self.state.join("app.log")
    }
}

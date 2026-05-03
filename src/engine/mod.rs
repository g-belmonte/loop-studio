pub mod command;
pub mod state;
pub mod worker;

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use crossbeam_channel::Sender;

pub use command::Command;
pub use state::SharedState;

/// GUI-side handle to the audio engine. Cheap to clone the `Arc<SharedState>`
/// out of; commands are sent over a `crossbeam-channel`.
pub struct Engine {
    tx: Sender<Command>,
    state: Arc<SharedState>,
    _join: JoinHandle<()>,
}

impl Engine {
    pub fn spawn() -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let state = Arc::new(SharedState::default());
        let state_for_worker = state.clone();
        let join = thread::Builder::new()
            .name("loop-studio-engine".into())
            .spawn(move || worker::run(rx, state_for_worker))
            .context("spawning engine thread")?;
        Ok(Self {
            tx,
            state,
            _join: join,
        })
    }

    pub fn send(&self, cmd: Command) {
        if let Err(e) = self.tx.send(cmd) {
            log::warn!("engine receiver dropped: {e}");
        }
    }

    pub fn state(&self) -> &SharedState {
        &self.state
    }
}

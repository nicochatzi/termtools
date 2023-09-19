use crossbeam::channel::Receiver;
use notify::Watcher;
use std::path::{Path, PathBuf};

pub struct FsWatcher {
    _watcher: notify::RecommendedWatcher,
    events: Receiver<notify::Result<notify::Event>>,
}

impl FsWatcher {
    pub fn run(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let (tx, rx) = crossbeam::channel::bounded(100);
        let mut watcher = notify::RecommendedWatcher::new(tx, notify::Config::default())?;
        watcher.watch(path.as_ref(), notify::RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            events: rx,
        })
    }

    pub fn events(&self) -> Receiver<notify::Result<notify::Event>> {
        self.events.clone()
    }
}

/// Default locations stored in `~/.aud`
///
/// .
/// ├── api
/// │  ├── aud/
/// │  ├── examples/
/// │  └── midimon/
/// ├── bin
/// │  └── aud
/// └── log
///    └── aud.log
///
pub mod locations {
    use super::*;

    pub fn aud() -> Option<PathBuf> {
        Some(dirs::home_dir()?.join(".aud"))
    }

    pub fn bin() -> Option<PathBuf> {
        Some(aud()?.join("bin"))
    }

    pub fn api() -> Option<PathBuf> {
        Some(aud()?.join("api"))
    }

    pub fn log() -> Option<PathBuf> {
        Some(aud()?.join("log"))
    }
}

pub fn log() -> Option<PathBuf> {
    Some(locations::log()?.join("aud.log"))
}
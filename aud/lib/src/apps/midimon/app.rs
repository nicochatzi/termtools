use super::lua::*;
use crate::{
    files,
    lua::{traits::api::*, LuaEngineEvent, LuaEngineHandle},
    midi::{MidiData, MidiReceiving},
};
use crossbeam::channel::{Receiver, Sender};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum AppEvent {
    Continue,
    Stopping,
    ScriptCrash,
    ScriptLoaded,
}

pub struct App {
    host_tx: Sender<HostEvent>,
    script_rx: Receiver<ScriptEvent>,
    lua_handle: LuaEngineHandle,
    midi_in: Box<dyn MidiReceiving>,

    port_names: Vec<String>,
    selected_port_name: Option<String>,
    selected_script_name: Option<String>,

    alert_message: Option<String>,
    messages: Vec<MidiData>,

    script_path: Option<PathBuf>,
    file_watcher: Option<files::FsWatcher>,
}

impl App {
    pub fn new(midi_in: Box<dyn MidiReceiving>) -> Self {
        let (host_tx, host_rx) = crossbeam::channel::bounded::<HostEvent>(1_000);
        let (script_tx, script_rx) = crossbeam::channel::bounded::<ScriptEvent>(1_000);

        Self {
            host_tx,
            script_rx,
            lua_handle: crate::lua::start_engine(ScriptController::new(script_tx, host_rx)),
            selected_port_name: None,
            port_names: midi_in.list_midi_devices().unwrap(),
            midi_in,
            selected_script_name: None,
            alert_message: None,
            messages: vec![],
            script_path: None,
            file_watcher: None,
        }
    }

    pub fn running(&self) -> bool {
        self.midi_in.is_midi_stream_active()
    }

    pub fn set_running(&mut self, should_run: bool) {
        self.midi_in.set_midi_stream_active(should_run)
    }

    pub fn ports(&self) -> &[String] {
        self.port_names.as_slice()
    }

    pub fn take_alert(&mut self) -> Option<String> {
        self.alert_message.take()
    }

    pub fn selected_port(&self) -> Option<&str> {
        self.selected_port_name.as_deref()
    }

    pub fn selected_script(&self) -> Option<&str> {
        self.selected_script_name.as_deref()
    }

    pub fn loaded_script_path(&self) -> Option<&PathBuf> {
        self.script_path.as_ref()
    }

    pub fn take_messages(&mut self) -> Vec<MidiData> {
        std::mem::take(&mut self.messages)
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
    }

    pub fn connect_to_midi_input_by_index(&mut self, port_index: usize) -> anyhow::Result<()> {
        if self.port_names.get(port_index).is_none() {
            return Ok(());
        }

        {
            let port_name = &self.port_names[port_index];
            self.midi_in.connect_to_midi_device(port_name)?;
            self.selected_port_name = Some(port_name.into());
            let port_name = port_name.to_owned();

            if let Err(e) = self.host_tx.try_send(HostEvent::Connect(port_name)) {
                log::error!("Failed to send device connected event to runtime : {e}");
            }
        }

        self.clear_messages();

        Ok(())
    }

    pub fn connect_to_midi_input(&mut self, port_name: &str) -> anyhow::Result<()> {
        match self.port_names.iter().position(|name| name == port_name) {
            Some(index) => self.connect_to_midi_input_by_index(index),
            None => Ok(()),
        }
    }

    /// Send a script to be loaded by the scripting engine. This function does not block.
    pub fn load_script(&mut self, script: impl AsRef<Path>) -> anyhow::Result<AppEvent> {
        let script_path = script.as_ref();
        if !script_path.exists() || !script_path.is_file() {
            anyhow::bail!("Invalid script path or type");
        }

        self.script_path = Some(script_path.into());
        self.selected_script_name = Some(
            script_path
                .file_name()
                .unwrap_or_default()
                .to_str()
                .unwrap_or_default()
                .to_owned(),
        );

        if let Err(e) = self.host_tx.try_send(HostEvent::Stop) {
            log::error!("failed to send stop event : {e}");
        }

        let event = HostEvent::LoadScript {
            name: self.selected_script_name.as_ref().unwrap().to_owned(),
            chunk: std::fs::read_to_string(script_path)?,
        };

        self.file_watcher = files::FsWatcher::run(script_path).ok();

        if let Err(e) = self.host_tx.try_send(event) {
            log::error!("failed to send load script event : {e}");
        }

        if let Err(e) = self
            .host_tx
            .try_send(HostEvent::Discover(self.port_names.clone()))
        {
            log::error!("failed to send discovery event : {e}");
        }

        if self.selected_port_name.is_some() {
            let port = self.selected_port_name.as_ref().unwrap().clone();
            self.connect_to_midi_input(&port)?;
        }

        Ok(AppEvent::Continue)
    }

    /// Load a script and block until the script has been loaded by the engine.
    pub fn load_script_sync(
        &mut self,
        script: impl AsRef<Path>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<()> {
        self.load_script(script)?;
        let start = std::time::Instant::now();
        while self.process_script_events()? != AppEvent::ScriptLoaded {
            if start.elapsed() > timeout {
                anyhow::bail!("Failed to load script in time");
            }
        }
        Ok(())
    }

    /// Block while waiting for the script to push an alert back to the app.
    pub fn wait_for_alert(
        &mut self,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Option<String>> {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            let _ = self.process_script_events()?;
            if self.alert_message.is_some() {
                return Ok(self.take_alert());
            }
        }
        Ok(self.take_alert())
    }

    /// Transfer all received MIDI messages to the engine.
    pub fn process_midi_messages(&mut self) {
        for msg in self.midi_in.produce_midi_messages() {
            if let Err(e) = self.host_tx.send(HostEvent::Midi(msg)) {
                log::error!("Failed to send midi to Lua Runtime : {e}");
            }
        }
    }

    /// Process all the available script events without blocking.
    /// This processes all the available events unless the engine:
    /// - requests to stop the application
    /// - has just loaded a script
    pub fn process_script_events(&mut self) -> anyhow::Result<AppEvent> {
        while let Ok(script_event) = self.script_rx.try_recv() {
            match script_event {
                ScriptEvent::Loaded => return Ok(AppEvent::ScriptLoaded),
                ScriptEvent::Log(request) => self.handle_lua_log_request(request),
                ScriptEvent::Midi(midi) => self.messages.push(midi),
                ScriptEvent::Connect(request) => self.handle_lua_connect_request(request)?,
                ScriptEvent::Control(request) => {
                    if self.handle_lua_control_request(request) == AppEvent::Stopping {
                        return Ok(AppEvent::Stopping);
                    }
                }
            }
        }

        Ok(AppEvent::Continue)
    }

    /// Process all the available file watcher events without blocking.
    pub fn process_file_events(&mut self) -> anyhow::Result<AppEvent> {
        let Some(ref watcher) = self.file_watcher else {
            return Ok(AppEvent::Continue);
        };

        for event in watcher.events().try_iter().collect::<Vec<_>>() {
            if self.has_file_changed(event) {
                if let Some(script) = self.script_path.clone() {
                    log::trace!("Loaded script has changed on filesystem");
                    return self.load_script(script);
                }

                break;
            }
        }

        Ok(AppEvent::Continue)
    }

    /// Process all the available engine events without blocking.
    pub fn process_engine_events(&mut self) -> anyhow::Result<AppEvent> {
        while let Ok(event) = self.lua_handle.events().try_recv() {
            match event {
                LuaEngineEvent::Panicked => return Ok(AppEvent::ScriptCrash),
                LuaEngineEvent::Terminated => log::info!("Lua Engine terminated"),
            }
        }

        Ok(AppEvent::Continue)
    }

    fn has_file_changed(&mut self, event: notify::Result<notify::Event>) -> bool {
        match event {
            Ok(event) => matches!(event.kind, notify::EventKind::Modify(_)),
            Err(e) => {
                log::error!("Script reload failed : {e}");
                false
            }
        }
    }

    fn handle_lua_connect_request(&mut self, request: ConnectionApiEvent) -> anyhow::Result<()> {
        let ConnectionApiEvent { ref device } = request;

        if self.port_names.iter().any(|name| name == device) {
            self.connect_to_midi_input(device)?;
        }

        Ok(())
    }

    fn handle_lua_control_request(&mut self, request: ControlFlowApiEvent) -> AppEvent {
        match request {
            ControlFlowApiEvent::Pause => self.set_running(false),
            ControlFlowApiEvent::Resume => self.set_running(true),
            ControlFlowApiEvent::Stop => return AppEvent::Stopping,
        }

        AppEvent::Continue
    }

    fn handle_lua_log_request(&mut self, request: LogApiEvent) {
        match request {
            LogApiEvent::Log(msg) => log::info!("{msg}"),
            LogApiEvent::Alert(msg) => self.alert_message = Some(msg),
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let Some(handle) = self.lua_handle.take_handle() else {
            return;
        };

        if let Err(e) = self.host_tx.try_send(HostEvent::Terminate) {
            log::error!("Failed to send termination message to Lua runtime : {e}");
            return;
        };

        if handle.join().is_err() {
            log::error!("Failed to join on Lua runtime thread handle");
        }
    }
}
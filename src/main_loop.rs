#[cfg(feature = "dbus_mpris")]
use crate::dbus_mpris::DbusServer;
use crate::player_event_handler::run_program_on_events;
use futures::{self, Async, Future, Poll, Stream};
use librespot::{
    connect::{
        discovery::DiscoveryStream,
        spirc::{Spirc, SpircTask},
    },
    core::{
        cache::Cache,
        config::{ConnectConfig, DeviceType, SessionConfig},
        session::Session,
    },
    playback::{
        audio_backend::Sink,
        config::PlayerConfig,
        mixer::Mixer,
        player::{Player, PlayerEvent},
    },
};
use std::{io, process::Child, rc::Rc};
use tokio_core::reactor::Handle;
use tokio_io::IoStream;

pub struct LibreSpotConnection {
    connection: Box<Future<Item = Session, Error = io::Error>>,
    spirc_task: Option<SpircTask>,
    spirc: Option<Rc<Spirc>>,
    discovery_stream: DiscoveryStream,
}

impl LibreSpotConnection {
    pub fn new(
        connection: Box<Future<Item = Session, Error = io::Error>>,
        discovery_stream: DiscoveryStream,
    ) -> LibreSpotConnection {
        LibreSpotConnection {
            connection,
            spirc_task: None,
            spirc: None,
            discovery_stream,
        }
    }
}

pub struct AudioSetup {
    pub mixer: Box<FnMut() -> Box<Mixer>>,
    pub backend: fn(Option<String>) -> Box<Sink>,
    pub audio_device: Option<String>,
}

pub struct SpotifydState {
    pub ctrl_c_stream: IoStream<()>,
    pub shutting_down: bool,
    pub cache: Option<Cache>,
    pub device_name: String,
    pub player_event_channel: Option<futures::sync::mpsc::UnboundedReceiver<PlayerEvent>>,
    pub player_event_program: Option<String>,
    pub dbus_mpris_server: Option<Box<Future<Item = (), Error = ()>>>,
}

#[cfg(feature = "dbus_mpris")]
fn new_dbus_server(
    session: Session,
    handle: Handle,
    spirc: Rc<Spirc>,
    device_name: String,
) -> Option<Box<Future<Item = (), Error = ()>>> {
    Some(Box::new(DbusServer::new(
        session,
        handle,
        spirc,
        device_name,
    )))
}

#[cfg(not(feature = "dbus_mpris"))]
fn new_dbus_server(
    _: Session,
    _: Handle,
    _: Rc<Spirc>,
    _: String,
) -> Option<Box<Future<Item = (), Error = ()>>> {
    None
}

pub struct MainLoopState {
    pub librespot_connection: LibreSpotConnection,
    pub audio_setup: AudioSetup,
    pub spotifyd_state: SpotifydState,
    pub player_config: PlayerConfig,
    pub session_config: SessionConfig,
    pub handle: Handle,
    pub linear_volume: bool,
    pub running_event_program: Option<Child>,
}

impl Future for MainLoopState {
    type Error = ();
    type Item = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            if let Async::Ready(Some(creds)) =
                self.librespot_connection.discovery_stream.poll().unwrap()
            {
                if let Some(ref mut spirc) = self.librespot_connection.spirc {
                    spirc.shutdown();
                }
                let session_config = self.session_config.clone();
                let cache = self.spotifyd_state.cache.clone();
                let handle = self.handle.clone();
                self.librespot_connection.connection =
                    Session::connect(session_config, creds, cache, handle);
            }

            if let Some(mut child) = self.running_event_program.take() {
                if let Ok(None) = child.try_wait() {
                    self.running_event_program = Some(child);
                }
            }
            if self.running_event_program.is_none() {
                if let Some(ref mut player_event_channel) = self.spotifyd_state.player_event_channel
                {
                    if let Async::Ready(Some(event)) = player_event_channel.poll().unwrap() {
                        if let Some(ref program) = self.spotifyd_state.player_event_program {
                            let child = run_program_on_events(event, program);
                            self.running_event_program = Some(child);
                        }
                    }
                }
            }

            if let Some(ref mut fut) = self.spotifyd_state.dbus_mpris_server {
                let _ = fut.poll();
            }

            if let Async::Ready(session) = self.librespot_connection.connection.poll().unwrap() {
                let mixer = (self.audio_setup.mixer)();
                let audio_filter = mixer.get_audio_filter();
                self.librespot_connection.connection = Box::new(futures::future::empty());
                let backend = self.audio_setup.backend;
                let audio_device = self.audio_setup.audio_device.clone();
                let (player, event_channel) = Player::new(
                    self.player_config.clone(),
                    session.clone(),
                    audio_filter,
                    move || (backend)(audio_device),
                );

                self.spotifyd_state.player_event_channel = Some(event_channel);

                let (spirc, spirc_task) = Spirc::new(
                    ConnectConfig {
                        name: self.spotifyd_state.device_name.clone(),
                        device_type: DeviceType::default(),
                        volume: mixer.volume(),
                        linear_volume: self.linear_volume,
                    },
                    session.clone(),
                    player,
                    mixer,
                );
                self.librespot_connection.spirc_task = Some(spirc_task);
                let shared_spirc = Rc::new(spirc);
                self.librespot_connection.spirc = Some(shared_spirc.clone());

                self.spotifyd_state.dbus_mpris_server = new_dbus_server(
                    session,
                    self.handle.clone(),
                    shared_spirc,
                    self.spotifyd_state.device_name.clone(),
                );
            } else if let Async::Ready(_) = self.spotifyd_state.ctrl_c_stream.poll().unwrap() {
                if !self.spotifyd_state.shutting_down {
                    if let Some(ref spirc) = self.librespot_connection.spirc {
                        spirc.shutdown();
                        self.spotifyd_state.shutting_down = true;
                    } else {
                        return Ok(Async::Ready(()));
                    }
                }
            } else if let Some(Async::Ready(_)) = self
                .librespot_connection
                .spirc_task
                .as_mut()
                .map(|ref mut st| st.poll().unwrap())
            {
                return Ok(Async::Ready(()));
            } else {
                return Ok(Async::NotReady);
            }
        }
    }
}

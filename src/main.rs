#![allow(unused)]
use std::{ops::DerefMut, process::ExitCode, sync::{atomic::AtomicBool, Arc}, time::{Duration, Instant}};
use config::{ConfigPathChoice, ConfigRetrievalError};
use musicdb::MusicDB;
use status_backend::{BackendContext, Listened};
use tokio::sync::Mutex;
use tracing::Instrument;
use util::{ferror, OWN_PID};

mod status_backend;
mod debugging;
mod data_fetching;
mod service;
mod config;
mod cli;
mod util;

fn watch_for_termination() -> (
    Arc<std::sync::atomic::AtomicBool>,
    std::pin::Pin<Box<impl std::future::Future<Output = tokio::signal::unix::SignalKind>>>
) {
    use tokio::signal::unix::{SignalKind, signal};
    use std::sync::atomic::{AtomicBool, Ordering};
    let flag = Arc::new(AtomicBool::new(false));
    let mut set = tokio::task::JoinSet::new();
    for kind in [
        SignalKind::quit(),
        SignalKind::hangup(),
        SignalKind::interrupt(),
        SignalKind::terminate(),
    ] {
        let mut sig = signal(kind).unwrap();
        let sent = flag.clone();
        set.spawn(async move {
            sig.recv().await;
            sent.store(true, Ordering::Relaxed);
            kind
        });
    }
    (
        flag,
        Box::pin(async move { set.join_next().await.unwrap().unwrap() })
    )
}

#[tokio::main(worker_threads = 4)]
async fn main() -> ExitCode {
    let args = Box::leak(Box::new(<cli::Cli as clap::Parser>::parse()));
    let config = config::Config::get(args).await;
    let debugging = debugging::DebuggingSession::new(args);
    let (term, pending_term) = watch_for_termination();

    macro_rules! get_config_or_path {
        () => {
            match config {
                Ok(config) => Ok(config),
                Err(error) => match error {
                    ConfigRetrievalError::UnknownFs { inner, .. } => util::ferror!("could not read config: {inner}"),
                    ConfigRetrievalError::DeserializationFailure { inner, .. } => util::ferror!("could not read config: deserialization failure: {inner}"),
                    ConfigRetrievalError::PermissionDenied(path) => util::ferror!("could not read config: lacking permission to read {}", path.to_string_lossy()),
                    ConfigRetrievalError::NotFound(path) => { Err(path) }
                }
            }
        }
    }

    macro_rules! get_config_or_error {
        () => {
            get_config_or_path!().unwrap_or_else(|path| util::ferror!("no configuration file @ {}", path.to_string_lossy()))
        }
    }

    use cli::Command;
    match args.command {
        Command::Start => {
            let mut config = match get_config_or_path!() {
                Ok(config) => config,
                Err(path) => if config::wizard::io::prompt_bool(match path {
                    ConfigPathChoice::Automatic(..) => "No configuration has been set up! Would you like to use the wizard to build one?",
                    ConfigPathChoice::Explicit(..) => "No configuration exists at the provided file! Would you like to use the wizard to build it?",
                    ConfigPathChoice::Environmental(..) => "No configuration exists at the file specified in the environmental variable! Would you like to use the wizard to build it?",
                }) {
                    let config = config::Config::create_with_wizard(path).await;
                    config.save_to_disk().await;
                    println!("Configuration file has been saved.");
                    config
                } else {
                    println!("Proceeding with a temporary default configuration.");
                    config::Config::default()
                }
            };

            let context = Arc::new(Mutex::new(PollingContext::from_config(&config, Arc::clone(&term)).await));
            let config = Arc::new(Mutex::new(config));
            
            let listener = if args.running_as_service {
                Some(service::ipc::listen(
                    context.clone(),
                    config.clone()
                ).await)
            } else { None };

            // If we get stuck somewhere in the main loop, we still want a way to exit if the user/system desires.
            tokio::spawn(async {
                pending_term.await;
                drop(listener); // remove listener socket
                drop(debugging.guards); // flush logs
                tokio::time::sleep(Duration::new(1, 0)).await;
                std::process::exit(1);
            });

            while !term.load(std::sync::atomic::Ordering::Relaxed) {
                proc_once(context.clone()).await;
            }
        },
        Command::Service { ref action } => {
            use cli::ServiceAction;
            use service::*;

            let manager = service::ServiceController::new();
            // let config_path = std::ffi::OsString::from(&*match get_config_or_path!() {
            //     Ok(config) => config.path,
            //     Err(path) => path
            // }.to_string_lossy());

            match action {
                ServiceAction::Start => {
                    let config_path = std::ffi::OsString::from(&*match get_config_or_path!() {
                        Ok(config) => config.path,
                        Err(path) => path
                    }.to_string_lossy());
                    if let Err(err) = manager.start(config_path, false) {
                        ferror!("could not start service: {}", err)
                    }
                },
                ServiceAction::Stop => match manager.stop() {
                    Ok(killed) => match killed {
                        0 => eprintln!("No processes were killed. The daemon might not have been functioning correctly."),
                        1 => println!("The service was stopped and the process was killed."),
                        n => eprintln!("The service was stopped and {n} processes were killed. Expected one process to be killed; this is likely a bug."),
                    },
                    Err(error) => ferror!("could not stop service: {}", error)
                },
                ServiceAction::Restart => {
                    let path = config.unwrap().socket_path;
                    let mut sender = service::ipc::PacketConnection::from_path(path).await.unwrap();
                    sender.send(ipc::Packet::Hello(ipc::packets::Hello {
                        version: 0,
                        process: *OWN_PID,
                    })).await.unwrap();
                    sender.send(ipc::Packet::ReloadConfiguration).await.unwrap();
                    // if let Err(error) = manager.restart(config) {
                    //     // ferror!("could not restart service: {}", error)       
                    // }
                }
            };
        },
        Command::Configure { ref action } => {
            tokio::spawn(async {
                pending_term.await;
                std::process::exit(1);
            });

            use cli::{ConfigurationAction, DiscordConfigurationAction};

            match action {
                ConfigurationAction::Where => {
                    match config {
                        Ok(config) => {
                            println!("{}", config.path.to_string_lossy());
                            println!("this path was {}", config.path.describe_for_choice_reasoning_suffix());
                        },
                        Err(err) => {
                            use std::borrow::Cow;
                            let path = err.path();
                            println!("{}", path.to_string_lossy());
                            eprintln!("this path was {} but {}", path.describe_for_choice_reasoning_suffix(), match err {
                                ConfigRetrievalError::DeserializationFailure { .. } => Cow::Borrowed("it couldn't be successfully deserialized"),
                                ConfigRetrievalError::NotFound { .. } => Cow::Borrowed(if path.was_auto() { "it currently doesn't exist" } else { "it couldn't be found" }),
                                ConfigRetrievalError::PermissionDenied(_) => Cow::Borrowed("the required permissions to read it are not available"),
                                ConfigRetrievalError::UnknownFs { inner, .. } => Cow::Owned(format!("an unknown error occurred trying to read it ({})", inner))
                            })
                        },
                    }
                },
                ConfigurationAction::Wizard => {
                    match get_config_or_path!() {
                        Err(path) => {
                            println!("Creating configuration file @ {}", path.to_string_lossy());
                            let config = config::Config::create_with_wizard(path).await;
                            config.save_to_disk().await;
                            println!("Successfully saved changes!");
                        }
                        Ok(mut config) => {
                            println!("Modifying configuration file @ {}", config.path.to_string_lossy());
                            config.edit_with_wizard().await;
                            config.save_to_disk().await;
                            println!("Successfully saved changes!");
                        },
                    }
                },
                ConfigurationAction::Discord { action } => {
                    let mut config = get_config_or_error!();
                    match action {
                        DiscordConfigurationAction::Enable => config.backends.discord = true,
                        DiscordConfigurationAction::Disable => config.backends.discord = false
                    };
                    config.save_to_disk().await;
                }
            }
        }
    }

    ExitCode::SUCCESS
}

#[derive(Debug)]
struct PollingContext<'a> {
    terminating: Arc<AtomicBool>,
    backends: status_backend::StatusBackends,
    pub last_track: Option<Arc<apple_music::Track>>,
    pub listened: Arc<Mutex<Listened>>,
    custom_artwork_host: Option<Box<dyn data_fetching::services::custom_artwork_host::CustomArtworkHost>>,
    musicdb: Option<musicdb::MusicDB<'a>>,
    /// The number of polls.
    /// A value of one means the first poll is ongoing; it's not zero-based because it's incremented at the start of the poll function.
    polls: u64,
    /// Sequential `PlayerState::Paused` occurrences.
    /// Used to detect when the state is *actually* considered paused, since sometimes the paused state is returned during buffer.
    sequential_pause_states: u64
}
impl PollingContext<'_> {
    async fn from_config(config: &config::Config<'_>, terminating: Arc<AtomicBool>) -> Self {
        Self {
            terminating,
            backends: status_backend::StatusBackends::new(config).await,
            last_track: None,
            listened: Arc::new(Mutex::new(Listened::new())),
            custom_artwork_host: Some(Box::new(data_fetching::services::custom_artwork_host::catbox::CatboxHost::new())),
            musicdb: Some(tracing::trace_span!("musicdb read").in_scope(MusicDB::default)),
            polls: 0,
            sequential_pause_states: 0,
        }
    }

    async fn reload_from_config(&mut self, config: &config::Config<'_>) {
        for backend in self.backends.all() {
        }

        let backends = status_backend::StatusBackends::new(config).await;
        self.backends = backends;
    }   
}

#[tracing::instrument(skip(context), level = "trace")]
async fn proc_once(mut context: Arc<Mutex<PollingContext<'_>>>) {
    let mut guard = context.lock().await;
    let context = guard.deref_mut();
    
    use apple_music::{AppleMusic, PlayerState, Track};

    let app = match tracing::trace_span!("app status retrieval").in_scope(AppleMusic::get_application_data) {
        Ok(app) => Arc::new(app),
        Err(err) => {
            use apple_music::Error;
            match &err {
                Error::DeserializationFailed if context.terminating.load(std::sync::atomic::Ordering::Relaxed) => { return } // child killed before us
                Error::DeserializationFailed | Error::NoData | Error::AppCommandFailed => { tracing::error!("{:?}", &err); return },
                Error::NotPlaying => { return }
            }
        }
    };

    let state = app.player_state.as_ref().expect("could not retrieve player state");
    context.sequential_pause_states = if matches!(state, PlayerState::Paused) { 0 } else { context.sequential_pause_states + 1 };

    match state {
        PlayerState::FastForwarding | PlayerState::Rewinding => unimplemented!(),
        PlayerState::Stopped => {
            #[cfg(feature = "discord")]
            if let Some(presence) = context.backends.discord.clone() {
                if let Err(error) = presence.lock().await.clear().await {
                    tracing::error!(?error, "unable to clear discord status")
                }
            }
            
            context.listened.lock().await.flush_current();
            
            if let Some(previous) = context.last_track.clone() {
                let listened = context.listened.clone();
                context.listened = Arc::new(Mutex::new(Listened::new()));
                context.last_track = None;
                context.backends.dispatch_track_ended(BackendContext {
                    listened,
                    track: previous,
                    app: app.clone(),
                    data: ().into(),
                }).await;
            }
        }
        PlayerState::Paused => {
            // Three sequential pause states (including this one) are required to consider
            // the state to actually be paused, as opposed to just buffer.
            const THRESHOLD_CONSIDER_TRULY_PAUSED: u64 = 3;

            if context.sequential_pause_states >= THRESHOLD_CONSIDER_TRULY_PAUSED {
                #[cfg(feature = "discord")]
                if let Some(presence) = context.backends.discord.clone() {
                    if let Err(error) = presence.lock().await.clear().await {
                        tracing::error!(?error, "unable to clear discord status")
                    }
                }

                context.listened.lock().await.flush_current();
            }
        },

        PlayerState::Playing => {
            let track = match tracing::trace_span!("track retrieval").in_scope(AppleMusic::get_current_track) {
                Ok(track) => Arc::new(track),
                Err(err) => {
                    use apple_music::Error;
                    match &err {
                        Error::DeserializationFailed if context.terminating.load(std::sync::atomic::Ordering::Relaxed) => { return } // child killed before us
                        Error::DeserializationFailed | Error::NoData | Error::AppCommandFailed => { tracing::error!("{:?}", &err); return },
                        Error::NotPlaying => { return }
                    }
                }
            };
            
            let previous = context.last_track.as_ref().map(|v: &Arc<Track>| &v.persistent_id);
            if previous != Some(&track.persistent_id) {
                tracing::trace!("new track: {:?}", track);
                
                use data_fetching::AdditionalTrackData;
                let solicitation = context.backends.get_solicitations().await;
                let additional_data_pending = AdditionalTrackData::from_solicitation(solicitation, &track, context.musicdb.as_ref(), context.custom_artwork_host.as_mut());
                let additional_data = if let Some(previous) = context.last_track.clone() {
                    let pending_dispatch = context.backends.dispatch_track_ended(BackendContext {
                        app: app.clone(),
                        track: previous,
                        listened: context.listened.clone(),
                        data: ().into(),
                    }).instrument(tracing::trace_span!("song end dispatch"));

                    async move { 
                        // Run song-end dispatch concurrently while we fetch the additional data for the next
                        tokio::join!(
                            additional_data_pending,
                            pending_dispatch
                        )
                    }.await.0
                } else {
                    additional_data_pending.await
                };

                // We can't trust the `app.player_position` at this stage because there might've been a race condition.
                let listened = Arc::new(Mutex::new(Listened::new_with_current({
                    // If this isn't the first song, we can assume it's quite unlikely that the user performed
                    // a skip to another point in time in the few nanoseconds that occurred, so going with
                    // the adjusted start time is okay.
                    if context.polls != 1 {
                        track.start
                    } else {
                        // Otherwise, the user started the program in the middle of listening to a song.
                        // In that event, `app.player_position` is trustworthy enough.
                        app.player_position.unwrap_or(track.start)
                    }
                })));

                context.listened = listened.clone();
                context.last_track = Some(track.clone());
                context.backends.dispatch_track_started(BackendContext { app, listened, track, data: Arc::new(additional_data) }).await;
            } else {
                let mut listened = context.listened.lock().await;
                let position = app.player_position.expect("no position");
                match listened.current.as_ref() {
                    None => listened.set_new_current(position),
                    Some(current) => {
                        let expected = current.get_expected_song_position();
                        if (expected - position).abs() >= 2. {
                            listened.flush_current();
                            listened.set_new_current(position);
                            drop(listened); // give up lock
                            context.backends.dispatch_current_progress(BackendContext {
                                track: track.clone(),
                                app: app.clone(),
                                data: ().into(),
                                listened: context.listened.clone()
                            }).await;
                        }
                    }
                }
            }
        }
    }
}

use std::error::Error;
use std::fs::File;
use std::io::prelude::*;
use std::thread;
use std::process::{Command, Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Duration, Local};
use crossbeam::channel::{bounded, select, tick};
use discord::{Discord, State};
use discord::model::{ChannelId};
use log::*;
use serde_json::Value;

mod discord_commands;
mod server_log;

use server_log::{FromServerLog, server_log_thread};
use discord_commands::{FromDiscord, discord_thread};

// KIVANITT => #mc-server
const BOT_CHANNEL: u64 = include!("../server_id.txt");
const PREFIX: &'static str = "mc!";

static CONSOLE_ENABLED: AtomicBool = AtomicBool::new(false);

macro_rules! get_option {
    ($config:expr, $name:literal) => {
        $config.get($name).and_then(Value::as_str).ok_or(format!("No {} in config file", $name))?
    };
}

fn setup_logger(config: &Value) -> Result<(), Box<dyn Error>> { 
    let config_level = log::LevelFilter::Info;

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S]"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(config_level)
        .chain(std::io::stdout())
        .chain(fern::log_file("output.log")?)
        .apply()?;

    let console_enabled = config.get("console_enabled").map(|x| x.as_bool().unwrap_or_default()).unwrap_or_default();
    CONSOLE_ENABLED.store(console_enabled, Ordering::Relaxed);

    Ok(())
}

fn create_discord_client(config: &Value) -> Result<Discord, Box<dyn Error>> {
    let username = get_option!(config, "username");
    let password = get_option!(config, "password");

    #[allow(deprecated)]
    return Ok(Discord::new(username, password)?);
}

enum ServerStatus {
    Unknown,
    Offline,
    Starting {
        server: Child,
        start_time: DateTime<Local>
    },
    Running {
        server: Child,
    },
    Stopping {
        server: Option<Child>,
        rcon: Option<Child>,
    }
}

fn main_thread(config: &Value, bot: Discord) -> Result<(), Box<dyn Error>> {
    #[allow(non_snake_case)] let ERROR_TIMEOUT: Duration = Duration::seconds(15);
    #[allow(non_snake_case)] let MESSAGE_TIMEOUT: Duration = Duration::seconds(2);

    let mut server_status = ServerStatus::Unknown;
    let mut last_error_reported = Local::now();
    let mut last_chat_msg = Local::now();
    
    struct CachedChat { name: String, message: String };
    let mut chat_msg_cache = Vec::<CachedChat>::new();

    let (mut from_discord, _discord_handle) = {
        let config = config.clone();
        let (discord_send, from_discord) = bounded(5);
        
        let (connection, ready) = bot.connect()?;
        let state = State::new(ready);

        let discord_thread = thread::spawn(move || {
            discord_thread(config, connection, state, discord_send).unwrap();
        });

        (from_discord, discord_thread)
    };

    let (mut server_log_send, mut from_server_log) = bounded::<FromServerLog>(5);

    let timeout = tick(Duration::seconds(1).to_std().unwrap());
    
    if let Err(x) = bot.send_message(
        ChannelId(BOT_CHANNEL), 
        format!("Server maintainer started, ver {}", clap::crate_version!()).as_str(), 
        "", false
    ) {
        error!("Failed to send message! - {}", x);
    }

    loop {
        let send_discord = |msg: String| {
            if let Err(_) = bot.send_message(ChannelId(BOT_CHANNEL), msg.as_str(), "", false) {
                error!("Failed to send message!");
            }
        };

        macro_rules! send_or_queue {
            ($name:expr, $message:expr) => {
                let now = Local::now();
                if now - last_chat_msg > MESSAGE_TIMEOUT {
                    let mut message_str = String::new();
    
                    for CachedChat { name, message } in chat_msg_cache.iter() {
                        message_str += format!("\n<**{}**> {}", name, message).as_str();
                    }
                    
                    message_str += format!("\n<**{}**> {}", $name, $message).as_str();
                    send_discord(message_str);
    
                    last_chat_msg = now;
                    chat_msg_cache.clear();
                } else {
                    chat_msg_cache.push(CachedChat { name: $name, message: $message });
                }
            }
        }

        if let ServerStatus::Stopping{ rcon, server } = &mut server_status {
            if let Some(server) = server {
                let server_stopped = match server.try_wait() {
                    Ok(None) => false,
                    _ => true
                };
                let rcon_stopped = {
                    if let Some(rcon) = rcon {
                        match rcon.try_wait() {
                            Ok(None) => false,
                            _ => true
                        }
                    } else {
                        true
                    }
                };

                if server_stopped {
                    if !rcon_stopped {
                        if let Some(rcon) = rcon {
                            rcon.kill().ok();
                        }

                        send_discord("Server stopped before time.".to_string());
                        warn!("Server stopped before time.");
                    } else {
                        send_discord("Server stopped.".to_string());
                        info!("Server stopped.");
                    }

                    server_status = ServerStatus::Offline;
                }
            }
        } else if let ServerStatus::Running{server} = &mut server_status {
            if match server.try_wait() {
                Ok(None) => false,
                _ => true
            } {
                send_discord(format!("Server died for some reason, {prefix}start to restart", prefix = PREFIX));
                server_status = ServerStatus::Offline;
                error!("Server died!");
            }
        }
        

        select! {
            recv(from_discord) -> discord_msg => {
                match discord_msg {
                    Ok(FromDiscord::StartServerEvent) => {
                        match server_status {
                            ServerStatus::Running{..} |
                            ServerStatus::Stopping{..} => {
                                send_discord("Server already running".to_string());
                                continue;
                            },
                            ServerStatus::Starting{..} => {
                                send_discord("Server already starting".to_string());
                                continue;
                            },
                            _ => ()
                        }

                        let java_path = get_option!(config, "java-path");
                        let server_path = get_option!(config, "server-path");
                        let server_folder = get_option!(config, "server-folder");
                        let min_ram = format!("-Xms{}", get_option!(config, "min-ram"));
                        let max_ram = format!("-Xmx{}", get_option!(config, "max-ram"));
                        
                        let mut server = Command::new(java_path)
                            .current_dir(server_folder)
                            .args(&[min_ram.as_str(), max_ram.as_str(), "-d64", "-server",
                                "-XX:+AggressiveOpts", "-XX:+UseConcMarkSweepGC",
                                "-XX:+UnlockExperimentalVMOptions", "-XX:+UseParNewGC",
                                "-XX:+ExplicitGCInvokesConcurrent", "-XX:+UseFastAccessorMethods",
                                "-XX:+OptimizeStringConcat", "-XX:+UseAdaptiveGCBoundary",
                                "-jar", server_path,
                                "nogui",
                            ])
                            .stdout(Stdio::piped())
                            .spawn()?;

                        if let Some(stdout) = server.stdout.take() {
                            let thread_config = config.clone();
                            let thread_send = server_log_send.clone();
    
                            thread::spawn(move || {
                                server_log_thread(thread_config, stdout, thread_send).unwrap();
                            });
                        }

                        let start_time = Local::now();

                        server_status = ServerStatus::Starting{ server, start_time };
                        send_discord("Server starting now, ETA 3 minutes".to_string());
                        info!("Server started.");
                    },

                    Ok(FromDiscord::StopServerEvent) => {
                        let mut server_process = None;
                        match server_status {
                            ServerStatus::Offline
                            | ServerStatus::Starting{..} => {
                                send_discord("Server's not running (yet)".to_string());
                                continue;
                            },

                            ServerStatus::Running{ server } => {
                                server_process = Some(server);
                            }

                            ServerStatus::Stopping{..} => {
                                send_discord("Server's already stopping".to_string());
                                continue;
                            }
                            _ => ()
                        }
                        let rcon = Some(Command::new(get_option!(config, "mcrcon-path"))
                            .args(&["-P", "25564", "-p", get_option!(config, "rcon_password"), "-s",
                                "-w", "60",
                                "say Shutting down in 5 minutes",
                                "say Shutting down in 4 minutes",
                                "say Shutting down in 3 minutes",
                                "say Shutting down in 2 minutes",
                                "say Shutting down in 1 minute",
                                "shutdown",
                            ])
                            .stdin(Stdio::null())
                            .spawn()?);
                        server_status = ServerStatus::Stopping{ server: server_process, rcon };
                        send_discord("Server will be stopped in 5 minutes, type `mc!cancel` to cancel".to_string());
                        info!("Server stop started.");
                    },

                    Ok(FromDiscord::KillServerEvent) => {
                        let mut server_process = None;
                        match server_status {
                            ServerStatus::Offline
                            | ServerStatus::Starting{..} => {
                                send_discord("Server's not running (yet)".to_string());
                                continue;
                            },

                            ServerStatus::Running{ server } => {
                                server_process = Some(server);
                            }

                            ServerStatus::Stopping{..} => {
                                send_discord("Server's already stopping".to_string());
                                continue;
                            }
                            _ => ()
                        }
                        let rcon = Some(Command::new(get_option!(config, "mcrcon-path"))
                            .args(&["-P", "25564", "-p", get_option!(config, "rcon_password"), "-s",
                                "shutdown",
                            ])
                            .stdin(Stdio::null())
                            .spawn()?);
                        server_status = ServerStatus::Stopping{ server: server_process, rcon };
                        send_discord("Server is stopping now".to_string());
                        info!("Server killed.");
                    },

                    Ok(FromDiscord::ShutdownServerEvent(_h, _m)) => {
                        send_discord("Unimplemented, to be added later".to_string());
                    },

                    Ok(FromDiscord::CancelShutdownEvent) => {
                        match &mut server_status {
                            ServerStatus::Stopping{ rcon: Some(rcon), .. } => {
                                if let Err(_) = rcon.kill() {
                                    send_discord("Error while cancelling shutdown".to_string());
                                } else {
                                    send_discord("Shutdown cancelled".to_string());
                                }

                                let _rcon = Command::new(get_option!(config, "mcrcon-path"))
                                    .args(&["-P", "25564", "-p", get_option!(config, "rcon_password"), "-s",
                                        "say Shutdown cancelled",
                                    ])
                                    .stdin(Stdio::null())
                                    .spawn()?;
                                
                                if let ServerStatus::Stopping{ server: Some(server), .. } = server_status {
                                    server_status = ServerStatus::Running{server};
                                } else {
                                    server_status = ServerStatus::Unknown;
                                }
                            },

                            ServerStatus::Stopping{ rcon: None, .. } => {
                                send_discord("Shutdown cannot be cancelled".to_string());
                                continue;
                            },

                            ServerStatus::Offline => {
                                send_discord("Server's not running".to_string());
                                continue;
                            },

                            _ => {
                                send_discord("No shutdown in progress".to_string());
                                continue;
                            }
                        }
                        info!("Shutdown cancelled.");
                    },

                    Ok(FromDiscord::BackupEvent) => {
                        match server_status {
                            ServerStatus::Offline
                            | ServerStatus::Starting{..} => {
                                send_discord("Server's not running (yet)".to_string());
                                continue;
                            },

                            ServerStatus::Stopping{..} => {
                                send_discord("Server's stopping".to_string());
                                continue;
                            }
                            _ => ()
                        }
                        Command::new(get_option!(config, "mcrcon-path"))
                            .args(&["-P", "25564", "-p", get_option!(config, "rcon_password"), "-s",
                                "backup start",
                            ])
                            .stdin(Stdio::null())
                            .spawn()?;
                        info!("Backup started.");
                        send_discord("Backup started.".to_string());
                    },

                    Ok(FromDiscord::OpCommandEvent(user)) => {
                        if user == "" {
                            send_discord("Must provide a username to op".to_string());
                            continue;
                        }
                        match server_status {
                            ServerStatus::Offline
                            | ServerStatus::Starting{..} => {
                                send_discord("Server's not running (yet)".to_string());
                                continue;
                            },

                            ServerStatus::Stopping{..} => {
                                send_discord("Server's stopping".to_string());
                                continue;
                            }
                            _ => ()
                        }
                        let op_user = format!("op {}", user);
                        Command::new(get_option!(config, "mcrcon-path"))
                            .args(&["-P", "25564", "-p", get_option!(config, "rcon_password"), "-s",
                                "backup start",
                                op_user.as_str(),
                            ])
                            .stdin(Stdio::null())
                            .spawn()?;
                        warn!("Opped user {} by command", user);
                        send_discord(format!("Opped user {}. All ops are logged.\nDon't forget to de-op yourself after you're done!", user));
                    },

                    Ok(FromDiscord::StatusQueryEvent) => {
                        match server_status {
                            ServerStatus::Offline => {
                                send_discord("Server is offline.".to_string());
                            },
                            ServerStatus::Unknown => {
                                send_discord("Server is probably offline, but worth a try.".to_string());
                            },
                            ServerStatus::Starting{..} => {
                                send_discord("Server is starting, check back in a few mins.".to_string());
                            },
                            ServerStatus::Running{..} => {
                                send_discord("Server is running.".to_string());
                            },
                            ServerStatus::Stopping{..} => {
                                send_discord("Server is stopping.".to_string());
                            },
                        }
                    },

                    Ok(FromDiscord::HelpEvent) => {
                        send_discord(format!(
                            r#"Commands:
    `{prefix}start` - Starts the server
    `{prefix}stop` - Stops the server
    `{prefix}kill` - Stops the server without waiting 5 mins
    `{prefix}cancel` - Cancels server stop
    `{prefix}shutdown [hh:mm]` - Schedules a shutdown in CEST
    `{prefix}backup` - Starts a backup on the server (pls no spam)
    `{prefix}op` - Ops a user if an accident happens - all ops are logged
    `{prefix}status` - Displays server status
    `{prefix}help` - Displays this message"#,
                                prefix = PREFIX
                            ));
                    },
                    Ok(FromDiscord::UnknownCommand) |
                    Ok(FromDiscord::NoCommand) => {
                        send_discord(format!("Unknown command, try `{prefix}help` if you're stuck", prefix = PREFIX));
                    },
                    Ok(FromDiscord::ErrorEvent) => {
                        info!("Discord closed.");
                        return Err(Box::from("Discord closed"));
                    },
                    Err(_) | Ok(FromDiscord::ReconnectEvent) => {
                        // Handle the websocket connection being dropped
                        let config = config.clone();
                        let (discord_send, new_from_discord) = bounded(5);
    
                        let (connection, ready) = bot.connect()?;
                        let state = State::new(ready);
                        info!("Reconnected successfully.");
    
                        thread::spawn(move || {
                            discord_thread(config, connection, state, discord_send).unwrap();
                        });
    
                        from_discord = new_from_discord;
                    },
                }
            },
            recv(from_server_log) -> server_log_msg => {

                match server_log_msg {
                    Ok(FromServerLog::ServerStarted) => {
                        if let ServerStatus::Starting { server, start_time } = server_status {
                            server_status = ServerStatus::Running { server };

                            let elapsed_time = Local::now() - start_time;
                            send_discord(format!("Server's now running, startup: {}s", elapsed_time.num_seconds()));
                        } else {
                            error!("Server is running, but previous status was invalid");
                            server_status = ServerStatus::Unknown;
                        }
                    },
                    Ok(FromServerLog::ServerStopping) => {
                        send_discord("Server is now stopping...".to_string());
                        if let ServerStatus::Running { server } = server_status {
                            server_status = ServerStatus::Stopping {
                                server: Some(server),
                                rcon: None
                            }
                        }
                    },

                    Ok(FromServerLog::ServerError { exception, sender }) => {
                        if matches!(server_status, ServerStatus::Running{..} | ServerStatus::Stopping{..}) {
                            let now = Local::now();
                            if now - last_error_reported >= ERROR_TIMEOUT {
                                last_error_reported = now;
                                send_discord(format!("Server encountered an exception:```md\n{}: {}```", sender, exception));
                            }
                        }
                    },

                    Ok(FromServerLog::LagSpike { length, ticks }) => {
                        send_discord(format!("Lag spike - {}ms, skipped {} ticks\nIf the problem persists, restart the server", length.num_milliseconds(), ticks));
                    },

                    Ok(FromServerLog::BackupStarted) => {
                        send_or_queue!("Server".to_string(), format!("*Backup started*"));
                    },
                    Ok(FromServerLog::BackupFinished { time }) => {
                        send_or_queue!("Server".to_string(), format!("*Backup finished - {}s*", time.num_seconds()));
                    },

                    Ok(FromServerLog::UserLogin { name }) => {
                        send_or_queue!("Server".to_string(), format!("*{} joined the game*", name));
                    },
                    Ok(FromServerLog::UserLogout { name }) => {
                        send_or_queue!("Server".to_string(), format!("*{} left the game*", name));
                    },

                    Ok(FromServerLog::ChatMessage { name, message }) => {
                        send_or_queue!(name, message);
                    },

                    Err(_) => {
                        if matches!(server_status, ServerStatus::Unknown | ServerStatus::Offline) {
                            error!("Server log pipe died, but server is not running or unknown");
                        } else {
                            error!("Server log pipe died, no idea about server status!");
                            server_status = ServerStatus::Unknown;
                        }

                        let new_server_log = bounded::<FromServerLog>(5);
                        server_log_send = new_server_log.0;
                        from_server_log = new_server_log.1;
                    }
                }
            }
            recv(timeout) -> _ => { continue; }
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let config: Value = {
        let mut file = File::open("config.json")?;
        let mut config_str = String::new();
        file.read_to_string(&mut config_str)?;

        serde_json::from_str(config_str.as_str())?
    };

    setup_logger(&config)?;
    let bot = create_discord_client(&config)?;
    info!("Started");
    
    main_thread(&config, bot)?;
    
    info!("Stopping");
    Ok(())
}

use std::error::Error;

use crossbeam::channel::Sender;
use discord::{State, Connection};
use discord::model::{Event};
use log::*;
use serde_json::Value;

pub enum FromDiscord {
    ReconnectEvent,
    ErrorEvent,
    StartServerEvent,
    StopServerEvent,
    KillServerEvent,
    ShutdownServerEvent(u8, u8),
    CancelShutdownEvent,
    BackupEvent,
    OpCommandEvent(String),
    StatusQueryEvent,
    HelpEvent,
    UnknownCommand,
    NoCommand
}

pub fn discord_thread(_config: Value, mut connection: Connection, state: State, discord_send: Sender<FromDiscord>) -> Result<(), Box<dyn Error>> {
    info!("Discord thread now running.");

    loop {
        let event = match connection.recv_event() {
            Ok(event) => event,
            Err(err) => {
                error!("Receive error: {}", err);

                if let discord::Error::WebSocket(..) = err {
                    discord_send.send(FromDiscord::ReconnectEvent)?;
                    return Ok(());
                }

                if let discord::Error::Closed(..) = err {
                    discord_send.send(FromDiscord::ErrorEvent)?;
                    return Ok(());
                }
                continue;
            }
        };

        match event {
            Event::MessageCreate(message) => {
                if message.author.id == state.user().id {
                    continue;
                }
                
                if message.channel_id.0 != crate::BOT_CHANNEL {
                    continue;
                }

                if !message.content.starts_with(crate::PREFIX) {
                    continue;
                }

                let message_params: Vec<String> = message.content
                .split_at(crate::PREFIX.len()).1
                .split_ascii_whitespace()
                .map(String::from)
                .collect();

                discord_send.send(
                    match message_params.first().map(String::as_str) {
                        Some("start") => FromDiscord::StartServerEvent,
                        Some("stop") => FromDiscord::StopServerEvent,
                        Some("kill") => FromDiscord::KillServerEvent,
                        
                        Some("shutdown") => FromDiscord::ShutdownServerEvent(0, 0),
                        Some("cancel") => FromDiscord::CancelShutdownEvent,
                        Some("backup") => FromDiscord::BackupEvent,
                        Some("op") => FromDiscord::OpCommandEvent(message_params.get(1).cloned().unwrap_or_default()),
                        Some("status") => FromDiscord::StatusQueryEvent,

                        Some("help") => FromDiscord::HelpEvent,

                        Some(_x) => FromDiscord::UnknownCommand,
                        None => FromDiscord::NoCommand
                    }
                )?;
            },
            _ => ()
        }
    }
}
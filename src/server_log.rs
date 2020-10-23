use std::error::Error;
use std::io::prelude::*;
use std::io::BufReader;
use std::process::{ChildStdout};
use std::sync::atomic::Ordering;

use chrono::Duration;
use crossbeam::channel::Sender;
use log::*;
use serde_json::Value;

use crate::CONSOLE_ENABLED;

#[derive(Debug, Eq, PartialEq)]
pub enum FromServerLog {
    ServerStarted,
    ServerStopping,
    ServerError {
        exception: String,
        sender: String
    },
    LagSpike {
        length: Duration,
        ticks: usize
    },

    BackupStarted,
    BackupFinished {
        time: Duration
    },

    UserLogin {
        name: String
    },
    UserLogout {
        name: String
    },

    ChatMessage {
        name: String,
        message: String
    },
}


pub mod scanners {
    use std::error::Error;
    use text_io::try_scan;
    use super::FromServerLog;
    use chrono::{Duration};

    #[derive(Debug, Default, Eq, PartialEq)]
    pub struct ScannedLine {
        pub time_str: String,
        pub sender_thread: String,
        pub level: String,
        pub sender_handle: String,
        pub is_chat_msg: bool,
        pub message: String,
    }

    macro_rules! bytes_endl {
        ($message:expr) => ( $message.bytes().chain(std::iter::once(b'\n')) )
    }

    macro_rules! check_sender {
        ($sender_var:expr, $sender_lit:literal) => {
            if $sender_var != $sender_lit {
                return Err("Not the right username".into());
            }
        }
    }

    macro_rules! check_message {
        ($sender_var:expr, $sender_lit:literal) => {
            if $sender_var != $sender_lit {
                return Err("Not the right message".into());
            }
        }
    }

    macro_rules! simple_scan {
        {$($name:ident => $sender:literal: $message:literal -> $variant:expr);*} => {
            $(
                pub fn $name(sender: &str, message: &str) -> Result<FromServerLog, Box<dyn Error>> {
                    check_sender!(sender, $sender);
                    check_message!(message, $message);
                
                    Ok($variant)
                }
            )*
        }
    }

    pub fn scan_line(line: &str) -> Result<ScannedLine, Box<dyn Error>> {
        fn scan_msg(line: &str) -> Result<ScannedLine, Box<dyn Error>> {
            let mut scanned_line = ScannedLine::default();
            let _temp: String;

            try_scan!(bytes_endl!(line) => "[{}] [{}/{}] [{}]: <{}> {}\n", 
                scanned_line.time_str,
                scanned_line.sender_thread,
                scanned_line.level,
                _temp,
                scanned_line.sender_handle,
                scanned_line.message
            );

            scanned_line.is_chat_msg = true;

            Ok(scanned_line)
        }

        fn scan_console_msg(line: &str) -> Result<ScannedLine, Box<dyn Error>> {
            let mut scanned_line = ScannedLine::default();
            let _temp: String;

            try_scan!(bytes_endl!(line) => "[{}] [{}/{}] [{}]: [{}] {}\n", 
                scanned_line.time_str,
                scanned_line.sender_thread,
                scanned_line.level,
                _temp,
                scanned_line.sender_handle,
                scanned_line.message
            );

            scanned_line.is_chat_msg = 
                scanned_line.sender_handle == "Server" 
                || scanned_line.sender_handle == "Rcon";

            Ok(scanned_line)
        }

        fn scan_fucky_log(line: &str) -> Result<ScannedLine, Box<dyn Error>> {
            let mut scanned_line = ScannedLine::default();
            let _temp: String;

            try_scan!(bytes_endl!(line) => "[{}] [{}/{}] [{}]: [{}]: {}\n", 
                scanned_line.time_str,
                scanned_line.sender_thread,
                scanned_line.level,
                _temp,
                scanned_line.sender_handle,
                scanned_line.message
            );

            Ok(scanned_line)
        }

        fn scan_log(line: &str) -> Result<ScannedLine, Box<dyn Error>> {
            let mut scanned_line = ScannedLine::default();

            try_scan!(bytes_endl!(line) => "[{}] [{}/{}] [{}]: {}\n", 
                scanned_line.time_str,
                scanned_line.sender_thread,
                scanned_line.level,
                scanned_line.sender_handle,
                scanned_line.message
            );

            Ok(scanned_line)
        }

        let scanned_line = scan_msg(line)
            .or_else(|_| scan_console_msg(line))
            .or_else(|_| scan_fucky_log(line))
            .or_else(|_| scan_log(line))?;

        Ok(scanned_line)
    }

    simple_scan!(
        scan_server_start => "mcjtylib_ng": "RFTools: server is starting" -> FromServerLog::ServerStarted;
        scan_server_stop => "minecraft/DedicatedServer": "Stopping the server" -> FromServerLog::ServerStopping;
        scan_backup_start => "minecraft/DedicatedServer": "Server Backup started!" -> FromServerLog::BackupStarted
    );

    pub fn scan_backup_stop(sender: &str, message: &str) -> Result<(FromServerLog, Duration), Box<dyn Error>> {
        check_sender!(sender, "minecraft/DedicatedServer");

        let mins: i64;
        let secs: i64;
        let _size: String;

        try_scan!(bytes_endl!(message) => "Server backup done in {}:{}! ({})\n", mins, secs, _size);

        let time = Duration::minutes(mins) + Duration::seconds(secs);
        Ok((FromServerLog::BackupFinished { time }, time))
    }

    pub fn scan_lag_spike(sender: &str, message: &str) -> Result<(FromServerLog, Duration), Box<dyn Error>> {
        check_sender!(sender, "minecraft/MinecraftServer");

        let num: i64;
        let ticks: usize;

        try_scan!(bytes_endl!(message) => "Can't keep up! Did the system time change, or is the server overloaded? Running {}ms behind, skipping {} tick(s)\n", num, ticks);

        let length = Duration::milliseconds(num);
        Ok((FromServerLog::LagSpike { length, ticks }, length))
    }

    pub fn scan_user_login(sender: &str, message: &str) -> Result<(FromServerLog, String), Box<dyn Error>> {
        check_sender!(sender, "minecraft/DedicatedServer");

        let name: String;

        try_scan!(bytes_endl!(message) => "{} joined the game\n", name);

        Ok((FromServerLog::UserLogin { name: name.clone() }, name))
    }

    pub fn scan_user_logout(sender: &str, message: &str) -> Result<(FromServerLog, String), Box<dyn Error>> {
        check_sender!(sender, "minecraft/DedicatedServer");

        let name: String;

        try_scan!(bytes_endl!(message) => "{} left the game\n", name);

        Ok((FromServerLog::UserLogout { name: name.clone() }, name))
    }

    #[cfg(test)]
    mod tests {
        /// [21:07:11] [Server thread/INFO] [minecraft/DedicatedServer]: <Kistepsi> nem
        #[test]
        fn test_scan_line_chat() {
            use super::*;
    
            let scan_msg = r#"[21:07:11] [Server thread/INFO] [minecraft/DedicatedServer]: <Kistepsi> nem"#;
            let scan_option = ScannedLine {
                time_str: "21:07:11".to_string(),
                sender_thread: "Server thread".to_string(),
                level: "INFO".to_string(),
                sender_handle: "Kistepsi".to_string(),
                is_chat_msg: true,
                message: "nem".to_string(),
            };
            let result = scan_line(scan_msg);
    
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), scan_option);
        }
    
        /// [21:31:06] [Server thread/INFO] [minecraft/DedicatedServer]: [Server] sdgfhljkjhlkdsfglkjhgfd sdgfhljkjhlkdsfglkjhgfd
        #[test]
        fn test_scan_line_server() {
            use super::*;
    
            let scan_msg = r#"[21:31:06] [Server thread/INFO] [minecraft/DedicatedServer]: [Server] sdgfhljkjhlkdsfglkjhgfd sdgfhljkjhlkdsfglkjhgfd"#;
            let scan_option = ScannedLine {
                time_str: "21:31:06".to_string(),
                sender_thread: "Server thread".to_string(),
                level: "INFO".to_string(),
                sender_handle: "Server".to_string(),
                is_chat_msg: true,
                message: "sdgfhljkjhlkdsfglkjhgfd sdgfhljkjhlkdsfglkjhgfd".to_string(),
            };
            let result = scan_line(scan_msg);
    
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), scan_option);
        }
    
        /// [21:03:02] [Server thread/INFO] [tombmanygraves]: [TombManyGraves]: szmarci07iq died in dimension 0 at BlockPos{x=108, y=40, z=2184}. Their grave may be near!
        #[test]
        fn test_scan_line_log_fucky() {
            use super::*;
    
            let scan_msg = r#"[21:03:02] [Server thread/INFO] [tombmanygraves]: [TombManyGraves]: szmarci07iq died in dimension 0 at BlockPos{x=108, y=40, z=2184}. Their grave may be near!"#;
            let scan_option = ScannedLine {
                time_str: "21:03:02".to_string(),
                sender_thread: "Server thread".to_string(),
                level: "INFO".to_string(),
                sender_handle: "TombManyGraves".to_string(),
                is_chat_msg: false,
                message: "szmarci07iq died in dimension 0 at BlockPos{x=108, y=40, z=2184}. Their grave may be near!".to_string(),
            };
            let result = scan_line(scan_msg);
    
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), scan_option);
        }
    
        /// [19:39:17] [Server thread/INFO] [Astral Sorcery]: [Astral Sorcery] Synchronizing baseline information to Screeper__
        #[test]
        fn test_scan_line_log_msg_lookalike() {
            use super::*;
    
            let scan_msg = r#"[19:39:17] [Server thread/INFO] [Astral Sorcery]: [Astral Sorcery] Synchronizing baseline information to Screeper__"#;
            let scan_option = ScannedLine {
                time_str: "19:39:17".to_string(),
                sender_thread: "Server thread".to_string(),
                level: "INFO".to_string(),
                sender_handle: "Astral Sorcery".to_string(),
                is_chat_msg: false,
                message: "Synchronizing baseline information to Screeper__".to_string(),
            };
            let result = scan_line(scan_msg);
    
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), scan_option);
        }
    
        /// [21:03:02] [Server thread/INFO] [minecraft/DedicatedServer]: szmarci07iq fell from a high place
        #[test]
        fn test_scan_line_log_normal() {
            use super::*;
    
            let scan_msg = r#"[21:03:02] [Server thread/INFO] [minecraft/DedicatedServer]: szmarci07iq fell from a high place"#;
            let scan_option = ScannedLine {
                time_str: "21:03:02".to_string(),
                sender_thread: "Server thread".to_string(),
                level: "INFO".to_string(),
                sender_handle: "minecraft/DedicatedServer".to_string(),
                is_chat_msg: false,
                message: "szmarci07iq fell from a high place".to_string(),
            };
            let result = scan_line(scan_msg);
    
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), scan_option);
        }

        /// [minecraft/MinecraftServer]: Can't keep up! Did the system time change, or is the server overloaded? Running 5125ms behind, skipping 102 tick(s)
        #[test]
        fn test_scan_lag_spike() {
            use super::*;
            use super::super::FromServerLog;
    
            let scan_sender = "minecraft/MinecraftServer";
            let scan_msg = r#"Can't keep up! Did the system time change, or is the server overloaded? Running 5125ms behind, skipping 102 tick(s)"#;
            let expected_time = Duration::milliseconds(5125);
            let expected_msg = FromServerLog::LagSpike {
                length: expected_time,
                ticks: 102
            };

            let result = scan_lag_spike(scan_sender, scan_msg);

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), (expected_msg, expected_time));
        }

        /// [mcjtylib_ng]: RFTools: server is starting
        #[test]
        fn test_scan_server_start() {
            use super::*;
            use super::super::FromServerLog;
    
            let scan_sender = "mcjtylib_ng";
            let scan_msg = r#"RFTools: server is starting"#;
            let expected_msg = FromServerLog::ServerStarted;

            let result = scan_server_start(scan_sender, scan_msg);

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), expected_msg);
        }

        /// [net.minecraft.server.MinecraftServer]: Stopping server
        #[test]
        fn test_scan_server_stop() {
            use super::*;
            use super::super::FromServerLog;
    
            let scan_sender = "net.minecraft.server.MinecraftServer";
            let scan_msg = r#"Stopping server"#;
            let expected_msg = FromServerLog::ServerStopping;

            let result = scan_server_stop(scan_sender, scan_msg);

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), expected_msg);
        }

        /// [minecraft/DedicatedServer]: Server Backup started!
        #[test]
        fn test_scan_backup_start() {
            use super::*;
            use super::super::FromServerLog;
    
            let scan_sender = "minecraft/DedicatedServer";
            let scan_msg = "Server Backup started!";
            let expected_msg = FromServerLog::BackupStarted;

            let result = scan_backup_start(scan_sender, scan_msg);

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), expected_msg);
        }

        /// [minecraft/DedicatedServer]: Server backup done in 00:10! (202.8MB | 1.2GB)
        #[test]
        fn test_scan_backup_stop() {
            use super::*;
            use super::super::FromServerLog;
    
            let scan_sender = "minecraft/DedicatedServer";
            let scan_msg = r#"Server backup done in 00:10! (202.8MB | 1.2GB)"#;
            let expected_time = Duration::seconds(10);
            let expected_msg = FromServerLog::BackupFinished {
                time: expected_time
            };

            let result = scan_backup_stop(scan_sender, scan_msg);

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), (expected_msg, expected_time));
        }

        /// [minecraft/DedicatedServer]: Davidminer_MC joined the game
        #[test]
        fn test_scan_user_login() {
            use super::*;
            use super::super::FromServerLog;
    
            let scan_sender = "minecraft/DedicatedServer";
            let scan_msg = r#"Davidminer_MC joined the game"#;

            let expected_name = "Davidminer_MC".to_string();
            let expected_msg = FromServerLog::UserLogin {
                name: expected_name.clone()
            };

            let result = scan_user_login(scan_sender, scan_msg);

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), (expected_msg, expected_name));
        }

        /// [minecraft/DedicatedServer]: Kistepsi left the game
        #[test]
        fn test_scan_user_logout() {
            use super::*;
            use super::super::FromServerLog;
    
            let scan_sender = "minecraft/DedicatedServer";
            let scan_msg = r#"Kistepsi left the game"#;
            
            let expected_name = "Kistepsi".to_string();
            let expected_msg = FromServerLog::UserLogout {
                name: expected_name.clone()
            };

            let result = scan_user_logout(scan_sender, scan_msg);

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), (expected_msg, expected_name));
        }
    }
}

use scanners::*;

pub fn server_log_thread(_config: Value, output: ChildStdout, log_send: Sender<FromServerLog>) -> Result<(), Box<dyn Error>> {
    info!("Server thread is now running.");

    let buf_read = BufReader::new(output);
    
    for line in buf_read.lines() {
        let line = line?;
        if let Ok(scanned_line) = scan_line(line.as_str()) {
            if scanned_line.is_chat_msg {
                let ScannedLine { sender_handle: name, message, .. } = scanned_line;
                
                info!(target: "server_chat", "<{}>: {}", name, message);
                log_send.send(FromServerLog::ChatMessage { name, message })?;

                continue;
            }
            
            let ScannedLine { sender_handle, message, .. } = scanned_line;

            let level = match scanned_line.level.as_str() {
                "INFO" => Level::Info,
                "WARN" => Level::Warn,
                "ERROR" => Level::Error,
                "FATAL" => Level::Error,
                "DEBUG" => Level::Debug,
                "TRACE" => Level::Trace,
                _ => Level::Trace
            };

            macro_rules! simple_scan {
                {$($fn_name:ident => [$level:expr] $target:literal: $log_msg:literal$(, $arg:ident)*);*} => {
                    
                    #[allow(unused_parens)]
                    $(
                        if let Ok((msg $(, $arg)*)) = $fn_name(sender_handle.as_str(), message.as_str()) {
                            log_send.send(msg)?;
                            log!(target: $target, $level, $log_msg$(, $arg)*);
                            continue;
                        }
                    )else*
                }
            }

            simple_scan!(
                scan_server_start => [Level::Info] "server_status": "Server is now up";
                scan_server_stop => [Level::Info] "server_status": "Server is now stopping";
                scan_lag_spike => [Level::Warn] "server_status": "Server overloaded! Lagspike of {} ms", length;
                scan_backup_start => [Level::Info] "server_status": "Backup started";
                scan_backup_stop => [Level::Info] "server_status": "Backup finished in {}", duration;
                scan_user_login => [Level::Info] "server_chat": "{} joined the game", name;
                scan_user_logout => [Level::Info] "server_chat": "{} left the game", name
            );

            if level <= Level::Error {
                let error_msg = FromServerLog::ServerError {
                    exception: message.clone(),
                    sender: sender_handle.clone()
                };
                log_send.send(error_msg)?;
            }

            if level <= Level::Warn {
                log!(target: "server_log", level, "[{}/{}]: {}",
                    scanned_line.sender_thread,
                    sender_handle,
                    message
                )
            }

            if CONSOLE_ENABLED.load(Ordering::Relaxed) {
                log!(target: "server_log", level, "[{}/{}]: {}",
                    scanned_line.sender_thread,
                    sender_handle,
                    message
                )
            }
        } else {
            if CONSOLE_ENABLED.load(Ordering::Relaxed) {
                error!(target: "server_log", "[Stack Trace]: {}", line);
                continue;
            }

            debug!(target: "server_log", "server line skipped");
        }
    }

    Ok(())
}
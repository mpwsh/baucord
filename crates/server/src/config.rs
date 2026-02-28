use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerSettings,
    pub network: NetworkSettings,
    pub channels: ChannelSettings,
    pub logging: LoggingSettings,
    pub authentication: AuthSettings,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerSettings {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub greeting_message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkSettings {
    pub max_clients: usize,
    pub protocol_id: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelSettings {
    pub reliable_channel_id: u8,
    pub voice_channel_id: u8,
    pub voice_buffer_size_mb: usize,
    #[serde(default = "default_video_channel_id")]
    pub video_channel_id: u8,
    #[serde(default = "default_video_buffer_size_mb")]
    pub video_buffer_size_mb: usize,
    /// Max bytes renet can send per tick per client.
    #[serde(default = "default_bytes_per_tick")]
    pub bytes_per_tick: u64,
}

fn default_bytes_per_tick() -> u64 {
    300_000
}

fn default_video_channel_id() -> u8 {
    protocol::VIDEO_CHANNEL
}

fn default_video_buffer_size_mb() -> usize {
    protocol::VIDEO_BUFFER_BYTES / (1024 * 1024)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoggingSettings {
    pub level: String,
    pub stats_interval_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthSettings {
    pub enabled: bool,
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&contents)?)
    }

    /// Write this config to a YAML file.
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), Box<dyn std::error::Error>> {
        let contents = serde_yaml::to_string(self)?;
        fs::write(path, contents)?;
        Ok(())
    }

    pub fn socket_addr(&self) -> Result<SocketAddr, std::net::AddrParseError> {
        format!("{}:{}", self.server.host, self.server.port).parse()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerSettings {
                name: "Baucord Server".to_string(),
                host: "0.0.0.0".to_string(),
                port: 5000,
                greeting_message: "Welcome to the server!".to_string(),
            },
            network: NetworkSettings {
                max_clients: 64,
                protocol_id: protocol::PROTOCOL_ID,
            },
            channels: ChannelSettings {
                reliable_channel_id: protocol::RELIABLE_CHANNEL,
                voice_channel_id: protocol::VOICE_CHANNEL,
                voice_buffer_size_mb: protocol::VOICE_BUFFER_BYTES / (1024 * 1024),
                video_channel_id: protocol::VIDEO_CHANNEL,
                video_buffer_size_mb: protocol::VIDEO_BUFFER_BYTES / (1024 * 1024),
                bytes_per_tick: 300_000,
            },
            logging: LoggingSettings {
                level: "info".to_string(),
                stats_interval_secs: 30,
            },
            authentication: AuthSettings { enabled: false },
        }
    }
}

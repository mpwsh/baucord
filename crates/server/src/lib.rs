//! baucord server library — split from the binary so integration tests
//! (tests/) can drive a real [`server::ChatServer`] in-process.

pub mod auth;
pub mod config;
pub mod relay;
pub mod server;

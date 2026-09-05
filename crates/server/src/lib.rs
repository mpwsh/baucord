//! baucord server library — split from the binary so integration tests
//! (tests/) can drive a real [`server::ChatServer`] in-process.

pub mod clock;
pub mod config;
pub mod link_stats;
pub mod persistence;
pub mod policy;
pub mod relay;
pub mod server;

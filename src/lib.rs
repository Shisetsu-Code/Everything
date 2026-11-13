//! Persistent, asynchronous local file search.
pub mod cli;
pub mod config;
pub mod database;
pub mod engine;
#[cfg(feature = "gui")]
pub mod gui;
pub mod indexer;
mod lease;
pub mod model;
pub mod operations;
pub mod query;

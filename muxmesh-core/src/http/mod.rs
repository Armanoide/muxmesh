//! Minimal HTTP client for the hub's open endpoints (std::net only, no deps).

pub mod client;

pub use client::get_state;

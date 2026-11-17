//! The Durable Runner adapter (A2, D-012, D-027): Durable Runner calls this proxy instead
//! of its allowlisted handlers; the proxy forwards every call unchanged, reports each
//! attempt and reconcile probe through the SDK under the runner's own run, step and
//! attempt numbers, and declares a run's outcome only from the runner's answer to
//! `GET /jobs/{id}`. Neither the runner nor the handlers change.

pub mod adapter;
pub mod classify;
pub mod config;

//! `magebuild deploy` and `magebuild agent`: ship a built artifact to a host
//! over one ssh session, with a small agent on the host doing the release work
//! that a Deployer recipe does one remote command at a time.

pub mod agent;
pub mod config;
mod cron;
mod envphp;
mod fcgi;
mod protocol;
mod redis;
pub mod runner;

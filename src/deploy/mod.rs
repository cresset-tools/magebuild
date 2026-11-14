//! `magebuild deploy` and `magebuild agent`: ship a built artifact to a host
//! over one ssh session, with a small agent on the host doing the release work
//! that a Deployer recipe does one remote command at a time.

//!
//! The runner side builds everywhere; the agent and its helpers use Unix APIs
//! (sockets, permissions, flock, /proc) and are built for Unix only.

#[cfg(unix)]
pub mod agent;
pub mod config;
#[cfg(unix)]
mod cron;
#[cfg(unix)]
mod envphp;
#[cfg(unix)]
mod fcgi;
mod protocol;
#[cfg(unix)]
mod redis;
pub mod runner;

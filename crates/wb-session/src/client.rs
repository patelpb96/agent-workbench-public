//! Client side of the broker protocol (used by the CLI, the GUI's native layer and tests).
#![cfg(windows)]
use crate::{
    auth,
    proto::{self, ClientKind, Request, Response, PROTOCOL_VERSION},
    win::*,
};
use std::{
    io,
    path::Path,
    time::Duration,
};

pub struct Client {
    io: PipeIo,
}

impl Client {
    pub fn connect(pipe_name: &str, data_dir: &Path, kind: ClientKind) -> io::Result<Client> {
        let secret = load_secret(data_dir, false)?;
        Self::connect_with_secret(pipe_name, &secret, kind)
    }

    /// Connect as an external agent (restricted capabilities; see `CapSet::AGENT`).
    pub fn connect_agent(pipe_name: &str, data_dir: &Path, agent: &str) -> io::Result<Client> {
        let secret = load_secret(data_dir, false)?;
        Self::connect_as(pipe_name, &secret, ClientKind::Agent, Some(agent.to_owned()))
    }

    pub fn connect_with_secret(pipe_name: &str, secret: &[u8], kind: ClientKind) -> io::Result<Client> {
        Self::connect_as(pipe_name, secret, kind, None)
    }

    /// As `connect`, giving up after `timeout` instead of the default 5 seconds.
    pub fn connect_timeout(pipe_name: &str, data_dir: &Path, kind: ClientKind, timeout: Duration) -> io::Result<Client> {
        let secret = load_secret(data_dir, false)?;
        Self::connect_as_timeout(pipe_name, &secret, kind, None, timeout)
    }

    pub fn connect_as(pipe_name: &str, secret: &[u8], kind: ClientKind, agent: Option<String>) -> io::Result<Client> {
        Self::connect_as_timeout(pipe_name, secret, kind, agent, Duration::from_secs(5))
    }

    pub fn connect_as_timeout(pipe_name: &str, secret: &[u8], kind: ClientKind, agent: Option<String>, timeout: Duration) -> io::Result<Client> {
        let h = open_pipe(pipe_name, timeout)?;
        // The client owns the handle from here on: every early return below drops it, which closes the pipe
        // (before this, an error during the handshake leaked the handle).
        let mut client = Client { io: pipe_io(h) };
        let nonce = match proto::recv_response(&mut client.io)? {
            Some(Response::Challenge { version, nonce }) if version == PROTOCOL_VERSION => nonce,
            Some(Response::Error { message, .. }) => return Err(io::Error::new(io::ErrorKind::PermissionDenied, message)),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "no challenge")),
        };
        let nonce = auth::unhex(&nonce).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad challenge"))?;
        let proof = auth::hex(&auth::proof(secret, &nonce, PROTOCOL_VERSION, kind));
        proto::send(&mut client.io, &Request::Hello { version: PROTOCOL_VERSION, client: kind, proof, agent })?;
        match proto::recv_response(&mut client.io)? {
            Some(Response::Ok) => Ok(client),
            Some(Response::Error { message, .. }) => Err(io::Error::new(io::ErrorKind::PermissionDenied, message)),
            _ => Err(io::Error::new(io::ErrorKind::ConnectionAborted, "connection closed during handshake")),
        }
    }

    pub fn call(&mut self, req: &Request) -> io::Result<Response> {
        proto::send(&mut self.io, req)?;
        proto::recv_response(&mut self.io)?.ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionAborted, "broker closed the connection"))
    }

    /// Send an arbitrary frame (tests: malformed or forged requests).
    pub fn raw(&mut self, payload: &[u8]) -> io::Result<Option<Response>> {
        proto::write_frame(&mut self.io, payload)?;
        proto::recv_response(&mut self.io)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        close_handle(self.io.raw());
    }
}

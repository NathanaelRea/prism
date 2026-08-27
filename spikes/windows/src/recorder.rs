use std::{
    io,
    net::{SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

use tokio::time;

use crate::{
    security::random_token_hex,
    support::{SpikeResult, require},
};

const VERSION: u8 = 1;
const TOKEN_LEN: usize = 64;
const MAX_EVENT: usize = 4 * 1024;
const MAX_PACKET: usize = 1 + TOKEN_LEN + MAX_EVENT;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordOutcome {
    Sent,
    DroppedBackpressure,
    DroppedOversize,
}

struct RecorderClient {
    socket: UdpSocket,
    endpoint: SocketAddr,
    token: String,
}

impl RecorderClient {
    fn new(endpoint: SocketAddr, token: String) -> SpikeResult<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            endpoint,
            token,
        })
    }

    fn try_record(&self, event: &[u8]) -> SpikeResult<RecordOutcome> {
        if event.len() > MAX_EVENT {
            return Ok(RecordOutcome::DroppedOversize);
        }
        let mut packet = Vec::with_capacity(1 + TOKEN_LEN + event.len());
        packet.push(VERSION);
        packet.extend_from_slice(self.token.as_bytes());
        packet.extend_from_slice(event);
        match self.socket.send_to(&packet, self.endpoint) {
            Ok(written) if written == packet.len() => Ok(RecordOutcome::Sent),
            Ok(_) => Ok(RecordOutcome::DroppedBackpressure),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Ok(RecordOutcome::DroppedBackpressure)
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn decode_packet<'a>(packet: &'a [u8], token: &str) -> Option<&'a [u8]> {
    if packet.len() < 1 + TOKEN_LEN || packet.len() > MAX_PACKET || packet[0] != VERSION {
        return None;
    }
    if &packet[1..1 + TOKEN_LEN] != token.as_bytes() {
        return None;
    }
    Some(&packet[1 + TOKEN_LEN..])
}

pub async fn run_spike() -> SpikeResult {
    println!("[flight recorder] authenticated nonblocking loopback datagrams");
    let receiver = UdpSocket::bind("127.0.0.1:0")?;
    receiver.set_nonblocking(true)?;
    let endpoint = receiver.local_addr()?;
    let receiver = tokio::net::UdpSocket::from_std(receiver)?;
    let token = random_token_hex()?;
    require(
        token.len() == TOKEN_LEN,
        "recorder secret has an unexpected size",
    )?;

    let client = RecorderClient::new(endpoint, token.clone())?;
    let invalid = RecorderClient::new(endpoint, random_token_hex()?)?;
    require(
        invalid.try_record(b"must-be-rejected")? == RecordOutcome::Sent,
        "invalid-token datagram was not submitted",
    )?;
    require(
        client.try_record("accepted:\u{2603}".as_bytes())? == RecordOutcome::Sent,
        "valid recorder datagram was not submitted",
    )?;
    require(
        client.try_record(&vec![0; MAX_EVENT + 1])? == RecordOutcome::DroppedOversize,
        "oversized telemetry was not dropped before I/O",
    )?;

    let mut accepted = Vec::new();
    let mut rejected = 0;
    for _ in 0..2 {
        let mut packet = [0_u8; MAX_PACKET];
        let (len, source) = time::timeout(Duration::from_secs(2), receiver.recv_from(&mut packet))
            .await
            .map_err(|_| "timed out receiving recorder datagram")??;
        require(
            source.ip().is_loopback(),
            "recorder accepted a non-loopback source",
        )?;
        match decode_packet(&packet[..len], &token) {
            Some(event) => accepted.push(event.to_vec()),
            None => rejected += 1,
        }
    }
    require(
        rejected == 1,
        "recorder did not reject the invalid run secret",
    )?;
    require(
        accepted == ["accepted:\u{2603}".as_bytes()],
        format!("recorder accepted unexpected events: {accepted:?}"),
    )?;

    let started = Instant::now();
    let mut observed_drop = false;
    for index in 0..50_000_u32 {
        let event = index.to_le_bytes();
        observed_drop |= client.try_record(&event)? == RecordOutcome::DroppedBackpressure;
    }
    require(
        started.elapsed() < Duration::from_secs(2),
        format!(
            "nonblocking telemetry producer took {:?} under saturation",
            started.elapsed()
        ),
    )?;
    println!(
        "[flight recorder] saturation outcome: {}",
        if observed_drop {
            "kernel backpressure observed and dropped"
        } else {
            "all datagrams accepted into the bounded kernel transport"
        }
    );
    println!("[flight recorder] PASS");
    Ok(())
}

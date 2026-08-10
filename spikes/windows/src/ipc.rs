use std::{io, time::Duration};

use interprocess::{
    local_socket::{
        GenericNamespaced, ListenerOptions, ToNsName,
        tokio::{Listener, Stream, prelude::*},
    },
    os::windows::local_socket::ListenerOptionsExt,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
    time,
};

use crate::{
    security::{private_pipe_security_descriptor, random_token_hex},
    support::{SpikeResult, fail, require, unique_name},
};

const MAX_FRAME: usize = 64 * 1024;
const REQUEST_CLIENTS: usize = 8;
const SUBSCRIBERS: usize = 2;
const INVALID_CLIENTS: usize = 1;

fn listener(name: &str) -> SpikeResult<Listener> {
    let name = name.to_ns_name::<GenericNamespaced>()?;
    Ok(ListenerOptions::new()
        .name(name)
        .security_descriptor(private_pipe_security_descriptor()?)
        .create_tokio()?)
}

async fn write_frame(stream: &Stream, payload: &[u8]) -> SpikeResult {
    require(
        payload.len() <= MAX_FRAME,
        "attempted to write an oversized IPC frame",
    )?;
    let len = u32::try_from(payload.len())?.to_le_bytes();
    let mut sender = stream;
    sender.write_all(&len).await?;
    sender.write_all(payload).await?;
    sender.flush().await?;
    Ok(())
}

async fn read_frame(stream: &Stream) -> SpikeResult<Vec<u8>> {
    let mut len = [0_u8; 4];
    let mut receiver = stream;
    receiver.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    require(
        len <= MAX_FRAME,
        format!("received oversized IPC frame ({len} bytes)"),
    )?;
    let mut payload = vec![0_u8; len];
    receiver.read_exact(&mut payload).await?;
    Ok(payload)
}

fn authenticated(token: &str, message: &str) -> Vec<u8> {
    format!("{token}\0{message}").into_bytes()
}

fn authenticate<'a>(token: &str, payload: &'a [u8]) -> SpikeResult<&'a str> {
    let payload = std::str::from_utf8(payload)?;
    let Some((candidate, message)) = payload.split_once('\0') else {
        return fail("worker frame omitted authentication separator");
    };
    require(
        candidate == token,
        "worker frame carried an invalid run secret",
    )?;
    Ok(message)
}

async fn handle_connection(stream: Stream, token: String) -> SpikeResult {
    let payload = read_frame(&stream).await?;
    let message = match authenticate(&token, &payload) {
        Ok(message) => message,
        Err(_) => return Ok(()),
    };
    match message {
        message if message.starts_with("request:") => {
            write_frame(
                &stream,
                format!("response:{}", &message["request:".len()..]).as_bytes(),
            )
            .await?;
        }
        "subscribe" => {
            write_frame(&stream, b"event:one").await?;
            write_frame(&stream, "event:two:\u{2603}".as_bytes()).await?;
        }
        message => return fail(format!("unexpected worker message {message:?}")),
    }
    Ok(())
}

async fn connect(name: &str) -> SpikeResult<Stream> {
    let name = name.to_ns_name::<GenericNamespaced>()?;
    let deadline = time::Instant::now() + Duration::from_secs(5);
    loop {
        match Stream::connect(name.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::WouldBlock
                ) && time::Instant::now() < deadline =>
            {
                time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn request_client(name: String, token: String, index: usize) -> SpikeResult {
    let stream = connect(&name).await?;
    let request = format!("request:{index}:\u{03bb}");
    write_frame(&stream, &authenticated(&token, &request)).await?;
    let response = String::from_utf8(read_frame(&stream).await?)?;
    require(
        response == format!("response:{index}:\u{03bb}"),
        format!("worker response mismatch: {response:?}"),
    )
}

async fn subscriber(name: String, token: String) -> SpikeResult {
    let stream = connect(&name).await?;
    write_frame(&stream, &authenticated(&token, "subscribe")).await?;
    require(
        read_frame(&stream).await? == b"event:one",
        "first subscriber event mismatch",
    )?;
    require(
        read_frame(&stream).await? == "event:two:\u{2603}".as_bytes(),
        "second subscriber event mismatch",
    )
}

async fn invalid_client(name: String) -> SpikeResult {
    let stream = connect(&name).await?;
    write_frame(
        &stream,
        &authenticated("invalid-secret", "request:must-not-run"),
    )
    .await?;
    require(
        read_frame(&stream).await.is_err(),
        "worker replied to an unauthenticated client",
    )
}

async fn join(task: JoinHandle<SpikeResult>) -> SpikeResult {
    task.await
        .map_err(|error| format!("IPC task failed: {error}"))?
}

pub async fn run_spike() -> SpikeResult {
    println!("[worker IPC] interprocess Tokio local sockets");
    let name = unique_name("prism-worker-spike");
    let token = random_token_hex()?;

    let first = listener(&name)?;
    match listener(&name) {
        Err(_) => {}
        Ok(_) => return fail("a second worker owner bound the same local-socket name"),
    }
    drop(first);
    let server_listener = listener(&name)?;

    let server_token = token.clone();
    let server = tokio::spawn(async move {
        let mut handlers = Vec::new();
        for _ in 0..(REQUEST_CLIENTS + SUBSCRIBERS + INVALID_CLIENTS) {
            let stream = server_listener.accept().await?;
            let token = server_token.clone();
            handlers.push(tokio::spawn(async move {
                handle_connection(stream, token).await
            }));
        }
        for handler in handlers {
            join(handler).await?;
        }
        Ok(())
    });

    let mut clients = Vec::new();
    for index in 0..REQUEST_CLIENTS {
        clients.push(tokio::spawn(request_client(
            name.clone(),
            token.clone(),
            index,
        )));
    }
    for _ in 0..SUBSCRIBERS {
        clients.push(tokio::spawn(subscriber(name.clone(), token.clone())));
    }
    clients.push(tokio::spawn(invalid_client(name.clone())));
    for client in clients {
        join(client).await?;
    }
    time::timeout(Duration::from_secs(10), join(server))
        .await
        .map_err(|_| "worker IPC server did not shut down")??;

    let rebound = listener(&name)?;
    drop(rebound);
    println!("[worker IPC] PASS");
    Ok(())
}

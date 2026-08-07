//! Rust authoring SDK for Prism executable extensions.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use prism_extension_protocol::{
    AttemptEnvelope, AttemptOutcome, DEFAULT_MAX_FRAME_BYTES, ExecuteResult, ExtensionDescriptor,
    HOST_FEATURES, HelloAck, HostOperation, Message, PROTOCOL_MAJOR, ProtocolError,
    ProtocolVersion, StructuredRender,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

pub use prism_extension_protocol as protocol;

pub type ExecuteFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'static>>;
type HostResponseSender = oneshot::Sender<Result<Value, ProtocolError>>;
type PendingHostResponses = Arc<Mutex<HashMap<String, HostResponseSender>>>;

/// The only public Rust implementation seam. Values crossing it are protocol values.
pub trait Extension: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn revision(&self) -> &str;
    fn descriptor(&self) -> ExtensionDescriptor;
    fn package_id(&self) -> String {
        self.id()
            .split_once('/')
            .map_or_else(|| self.id().to_owned(), |(package, _)| package.to_owned())
    }
    fn execute(&self, context: ExecuteContext, attempt: AttemptEnvelope) -> ExecuteFuture;
    fn invoke_trigger(
        &self,
        _context: ExecuteContext,
        adapter_id: String,
        _input: Value,
    ) -> ExecuteFuture {
        Box::pin(async move { Err(format!("Trigger adapter '{adapter_id}' is not implemented")) })
    }
    fn send_notification(
        &self,
        _context: ExecuteContext,
        channel_id: String,
        _notification: Value,
    ) -> ExecuteFuture {
        Box::pin(async move {
            Err(format!(
                "notification channel '{channel_id}' is not implemented"
            ))
        })
    }
    fn suggest_input(&self, schema_id: String, _context: Value) -> InputFuture<Vec<Value>> {
        Box::pin(async move {
            Err(ProtocolError::new(
                "input_support_unavailable",
                format!("input suggestions for '{schema_id}' are not implemented"),
            ))
        })
    }
    fn validate_input(&self, schema_id: String, _value: Value) -> InputFuture<()> {
        Box::pin(async move {
            Err(ProtocolError::new(
                "input_support_unavailable",
                format!("input validation for '{schema_id}' is not implemented"),
            ))
        })
    }
    fn render_artifact(
        &self,
        schema_id: String,
        _value: Value,
        _width: u16,
    ) -> InputFuture<StructuredRender> {
        Box::pin(async move {
            Err(ProtocolError::new(
                "renderer_unavailable",
                format!("renderer for '{schema_id}' is not implemented"),
            ))
        })
    }
}

pub type InputFuture<T> = Pin<Box<dyn Future<Output = Result<T, ProtocolError>> + Send + 'static>>;

#[derive(Clone)]
pub struct ExecuteContext {
    host: HostClient,
    cancellation: watch::Receiver<bool>,
}

impl ExecuteContext {
    pub fn is_cancelled(&self) -> bool {
        *self.cancellation.borrow()
    }

    pub fn cancellation(&self) -> watch::Receiver<bool> {
        self.cancellation.clone()
    }

    pub async fn host_operation(&self, operation: HostOperation) -> Result<Value, ProtocolError> {
        self.host.call(operation).await
    }
}

#[derive(Clone)]
struct HostClient {
    outbound: mpsc::Sender<Message>,
    pending: PendingHostResponses,
    sequence: Arc<AtomicU64>,
    attempt: Option<(String, u64)>,
}

impl HostClient {
    async fn call(&self, operation: HostOperation) -> Result<Value, ProtocolError> {
        let (attempt_id, generation) = self.attempt.clone().ok_or_else(|| {
            ProtocolError::new("no_attempt", "host operation is outside an Attempt")
        })?;
        let id = format!("sdk-host-{}", self.sequence.fetch_add(1, Ordering::Relaxed));
        let (send, receive) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| ProtocolError::new("sdk_poisoned", "host response registry poisoned"))?
            .insert(id.clone(), send);
        if self
            .outbound
            .send(Message::HostRequest {
                id: id.clone(),
                attempt_id,
                generation,
                operation,
            })
            .await
            .is_err()
        {
            self.pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&id));
            return Err(ProtocolError::new(
                "host_closed",
                "Prism host connection closed",
            ));
        }
        receive.await.unwrap_or_else(|_| {
            Err(ProtocolError::new(
                "host_closed",
                "Prism host dropped the operation",
            ))
        })
    }

    fn for_attempt(&self, attempt_id: String, generation: u64) -> Self {
        let mut client = self.clone();
        client.attempt = Some((attempt_id, generation));
        client
    }
}

/// Serve an extension over stdin/stdout using protocol-only JSON Lines on stdout.
pub async fn serve(extension: impl Extension) -> Result<(), String> {
    serve_arc(Arc::new(extension)).await
}

pub async fn serve_arc(extension: Arc<dyn Extension>) -> Result<(), String> {
    let mut input = BufReader::new(tokio::io::stdin());
    let (outbound, mut messages) = mpsc::channel::<Message>(128);
    let output_task = tokio::spawn(async move {
        let mut output = tokio::io::stdout();
        while let Some(message) = messages.recv().await {
            let frame = prism_extension_protocol::encode_frame(&message, DEFAULT_MAX_FRAME_BYTES)
                .map_err(|error| error.message)?;
            output
                .write_all(&frame)
                .await
                .map_err(|error| error.to_string())?;
            output.flush().await.map_err(|error| error.to_string())?;
        }
        Ok::<(), String>(())
    });

    let first = read_message(&mut input)
        .await
        .map_err(|error| format!("read hello: {}", error.message))?
        .ok_or_else(|| "host closed before hello".to_string())?;
    let Message::Hello { hello } = first else {
        return Err("first message must be hello".into());
    };
    if hello.protocol.major != PROTOCOL_MAJOR {
        return Err(format!(
            "unsupported protocol major {}",
            hello.protocol.major
        ));
    }
    outbound
        .send(Message::HelloAck {
            hello: HelloAck {
                protocol: ProtocolVersion {
                    major: PROTOCOL_MAJOR,
                    minor: prism_extension_protocol::PROTOCOL_MINOR,
                },
                features: hello
                    .features
                    .into_iter()
                    .filter(|feature| HOST_FEATURES.contains(&feature.as_str()))
                    .collect(),
                extension_id: extension.id().into(),
                extension_revision: extension.revision().into(),
                sdk_version: env!("CARGO_PKG_VERSION").into(),
                package_id: extension.package_id(),
                platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
                executable_digest: current_executable_digest()?,
            },
        })
        .await
        .map_err(|_| "write hello acknowledgement".to_string())?;

    let pending = Arc::new(Mutex::new(HashMap::new()));
    let host = HostClient {
        outbound: outbound.clone(),
        pending: pending.clone(),
        sequence: Arc::new(AtomicU64::new(1)),
        attempt: None,
    };
    let mut attempts: HashMap<(String, u64), watch::Sender<bool>> = HashMap::new();
    let (completed_send, mut completed) = mpsc::unbounded_channel();
    loop {
        let frame = tokio::select! {
            frame = read_message(&mut input) => match frame {
                Ok(frame) => frame,
                Err(error) => {
                    let _ = outbound.send(Message::Error { id: None, error }).await;
                    continue;
                }
            },
            completed = completed.recv() => {
                if let Some(key) = completed { attempts.remove(&key); }
                continue;
            }
        };
        let Some(message) = frame else { break };
        match message {
            Message::Describe { id } => outbound
                .send(Message::Description {
                    id,
                    descriptor: extension.descriptor(),
                })
                .await
                .map_err(|_| "host closed".to_string())?,
            Message::Execute { id, attempt } => {
                let key = (attempt.attempt_id.clone(), attempt.generation);
                if attempts.contains_key(&key) {
                    outbound
                        .send(Message::Error {
                            id: Some(id),
                            error: ProtocolError::new(
                                "duplicate_attempt",
                                "attempt identity is already active",
                            ),
                        })
                        .await
                        .map_err(|_| "host closed".to_string())?;
                    continue;
                }
                let (cancel, cancellation) = watch::channel(false);
                attempts.insert(key.clone(), cancel);
                let extension = extension.clone();
                let outbound = outbound.clone();
                let host = host.for_attempt(attempt.attempt_id.clone(), attempt.generation);
                let completed = completed_send.clone();
                tokio::spawn(async move {
                    let attempt_id = attempt.attempt_id.clone();
                    let generation = attempt.generation;
                    let outcome = match extension
                        .execute(
                            ExecuteContext {
                                host,
                                cancellation: cancellation.clone(),
                            },
                            attempt,
                        )
                        .await
                    {
                        _ if *cancellation.borrow() => AttemptOutcome::Cancelled,
                        Ok(outputs) => AttemptOutcome::Succeeded { outputs },
                        Err(error) => AttemptOutcome::Failed { error },
                    };
                    let _ = outbound
                        .send(Message::ExecuteResult {
                            id,
                            result: ExecuteResult {
                                attempt_id,
                                generation,
                                outcome,
                            },
                        })
                        .await;
                    let _ = completed.send(key);
                });
            }
            Message::Cancel {
                id,
                attempt_id,
                generation,
            } => {
                if let Some(cancel) = attempts.remove(&(attempt_id.clone(), generation)) {
                    let _ = cancel.send(true);
                }
                outbound
                    .send(Message::Cancelled {
                        id,
                        attempt_id,
                        generation,
                    })
                    .await
                    .map_err(|_| "host closed".to_string())?;
            }
            Message::HostResponse { id, result } => {
                if let Some(sender) = pending
                    .lock()
                    .map_err(|_| "host response registry poisoned".to_string())?
                    .remove(&id)
                {
                    let _ = sender.send(result);
                }
            }
            Message::InvokeTrigger {
                id,
                adapter_id,
                input,
            } => {
                let extension = extension.clone();
                let outbound = outbound.clone();
                let host = host.for_attempt(format!("trigger:{id}"), 1);
                tokio::spawn(async move {
                    let result = extension
                        .invoke_trigger(
                            ExecuteContext {
                                host,
                                cancellation: watch::channel(false).1,
                            },
                            adapter_id,
                            input,
                        )
                        .await
                        .map_err(|error| ProtocolError::new("trigger_failed", error));
                    let _ = outbound.send(Message::TriggerResult { id, result }).await;
                });
            }
            Message::SendNotification {
                id,
                channel_id,
                notification,
            } => {
                let extension = extension.clone();
                let outbound = outbound.clone();
                let host = host.for_attempt(format!("notification:{id}"), 1);
                tokio::spawn(async move {
                    let result = extension
                        .send_notification(
                            ExecuteContext {
                                host,
                                cancellation: watch::channel(false).1,
                            },
                            channel_id,
                            notification,
                        )
                        .await
                        .map_err(|error| ProtocolError::new("notification_failed", error));
                    let _ = outbound
                        .send(Message::NotificationResult { id, result })
                        .await;
                });
            }
            Message::SuggestInput {
                id,
                schema_id,
                context,
            } => {
                let extension = extension.clone();
                let outbound = outbound.clone();
                tokio::spawn(async move {
                    let result = extension.suggest_input(schema_id, context).await;
                    let _ = outbound
                        .send(Message::InputSuggestions { id, result })
                        .await;
                });
            }
            Message::ValidateInput {
                id,
                schema_id,
                value,
            } => {
                let extension = extension.clone();
                let outbound = outbound.clone();
                tokio::spawn(async move {
                    let result = extension.validate_input(schema_id, value).await;
                    let _ = outbound.send(Message::InputValidation { id, result }).await;
                });
            }
            Message::RenderArtifact {
                id,
                schema_id,
                value,
                width,
            } => {
                let extension = extension.clone();
                let outbound = outbound.clone();
                tokio::spawn(async move {
                    let result = extension.render_artifact(schema_id, value, width).await;
                    let _ = outbound.send(Message::ArtifactRender { id, result }).await;
                });
            }
            Message::Ping { id } => outbound
                .send(Message::Pong { id })
                .await
                .map_err(|_| "host closed".to_string())?,
            Message::Shutdown { id } => {
                for (_, cancel) in attempts.drain() {
                    let _ = cancel.send(true);
                }
                outbound
                    .send(Message::ShutdownAck { id })
                    .await
                    .map_err(|_| "host closed".to_string())?;
                break;
            }
            other => {
                outbound
                    .send(Message::Error {
                        id: other.correlation_id().map(str::to_owned),
                        error: ProtocolError::new(
                            "unexpected_message",
                            "message is not valid from host to extension",
                        ),
                    })
                    .await
                    .map_err(|_| "host closed".to_string())?;
            }
        }
    }
    drop(host);
    drop(completed_send);
    drop(outbound);
    output_task.await.map_err(|error| error.to_string())??;
    Ok(())
}

pub fn current_executable_digest() -> Result<String, String> {
    if let Ok(digest) = std::env::var("PRISM_EXTENSION_EXECUTABLE_DIGEST")
        && digest.starts_with("sha256:")
    {
        return Ok(digest);
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve extension executable: {error}"))?;
    let bytes = std::fs::read(executable)
        .map_err(|error| format!("read extension executable for handshake: {error}"))?;
    let digest = Sha256::digest(bytes);
    Ok(format!("sha256:{digest:x}"))
}

async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Message>, ProtocolError> {
    let mut frame = Vec::new();
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .map_err(|error| ProtocolError::new("read_frame", error.to_string()))?;
        if buffer.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(ProtocolError::new(
                    "malformed_json",
                    "unterminated JSON frame",
                ))
            };
        }
        let count = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if frame.len().saturating_add(count) > DEFAULT_MAX_FRAME_BYTES {
            reader.consume(count);
            return Err(ProtocolError::new(
                "oversized_frame",
                format!("protocol frame exceeds {DEFAULT_MAX_FRAME_BYTES} bytes"),
            ));
        }
        frame.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if frame.last() == Some(&b'\n') {
            break;
        }
    }
    prism_extension_protocol::decode_frame(&frame, DEFAULT_MAX_FRAME_BYTES).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inbound_frames_are_bounded_before_json_decoding() {
        let mut bytes = vec![b' '; DEFAULT_MAX_FRAME_BYTES + 1];
        bytes.push(b'\n');
        let mut reader = BufReader::new(bytes.as_slice());
        let error = read_message(&mut reader).await.unwrap_err();
        assert_eq!(error.code, "oversized_frame");
    }
}

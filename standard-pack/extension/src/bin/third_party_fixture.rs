use prism_extension_sdk::protocol::{
    AttemptEnvelope, ExtensionDescriptor, ImplementationDescriptor, NotificationChannelDescriptor,
    StepClass, TriggerDescriptor,
};
use prism_extension_sdk::{ExecuteContext, ExecuteFuture, Extension};

struct ThirdPartyFixture;

impl Extension for ThirdPartyFixture {
    fn id(&self) -> &str {
        "acme.fixture/extension"
    }

    fn revision(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn descriptor(&self) -> ExtensionDescriptor {
        ExtensionDescriptor {
            implementations: vec![ImplementationDescriptor {
                id: "acme.fixture/echo".into(),
                class: StepClass::Action,
                inputs: Vec::new(),
                outputs: Vec::new(),
                capabilities: Vec::new(),
                targets: Vec::new(),
                effect_boundary: prism_extension_sdk::protocol::EffectBoundary::Unbrokered,
            }],
            triggers: vec![TriggerDescriptor {
                id: "acme.fixture/trigger".into(),
                capabilities: Vec::new(),
            }],
            notification_channels: vec![NotificationChannelDescriptor {
                id: "acme.fixture/channel".into(),
                capabilities: Vec::new(),
            }],
            ..ExtensionDescriptor::default()
        }
    }

    fn execute(&self, context: ExecuteContext, attempt: AttemptEnvelope) -> ExecuteFuture {
        Box::pin(async move {
            if attempt
                .input
                .get("crash")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                std::process::exit(23);
            }
            let delay = attempt
                .input
                .get("delay_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let mut cancellation = context.cancellation();
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {},
                _ = cancellation.changed() => return Err("cancelled".into()),
            }
            Ok(serde_json::json!({"value": attempt.input}))
        })
    }

    fn invoke_trigger(
        &self,
        _context: ExecuteContext,
        _adapter_id: String,
        input: serde_json::Value,
    ) -> ExecuteFuture {
        Box::pin(async move { Ok(input) })
    }

    fn send_notification(
        &self,
        _context: ExecuteContext,
        _channel_id: String,
        notification: serde_json::Value,
    ) -> ExecuteFuture {
        Box::pin(async move { Ok(notification) })
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = prism_extension_sdk::serve(ThirdPartyFixture).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

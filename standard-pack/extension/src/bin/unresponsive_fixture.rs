use std::io::{BufRead, Write};

use prism_extension_sdk::protocol::{ExtensionDescriptor, HelloAck, Message, ProtocolVersion};

fn send(message: Message) {
    let mut output = std::io::stdout();
    serde_json::to_writer(&mut output, &message).unwrap();
    output.write_all(b"\n").unwrap();
    output.flush().unwrap();
}

fn main() {
    let input = std::io::BufReader::new(std::io::stdin());
    for line in input.lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Message>(&line) else {
            continue;
        };
        match message {
            Message::Hello { .. } => send(Message::HelloAck {
                hello: HelloAck {
                    protocol: ProtocolVersion::CURRENT,
                    features: Vec::new(),
                    extension_id: "acme.fixture/unresponsive".into(),
                    extension_revision: "fixture-v1".into(),
                    sdk_version: env!("CARGO_PKG_VERSION").into(),
                    package_id: "acme.fixture".into(),
                    platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
                    executable_digest: prism_extension_sdk::current_executable_digest().unwrap(),
                },
            }),
            Message::Describe { id } => send(Message::Description {
                id,
                descriptor: ExtensionDescriptor::default(),
            }),
            _ => std::thread::sleep(std::time::Duration::from_secs(60)),
        }
    }
}

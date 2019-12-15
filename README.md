
# tmi-rs: Rust Twitch chat interface

This is an asynchronous websocket based interface to Twitch chat intended as
a base for chat bots and other programs that interact with Twitch chat.

All official chat events and tags are supported, but some more advanced features are still
missing, including:

* Ability to directly `await` the results of commands that have a response from Twitch, like `/mods`

### Example usage

```rust
#[macro_use]
extern crate log;

use std::env;
use std::error::Error;

use futures::future::{join, ready};
use futures::sink::SinkExt;
use futures::stream::StreamExt;

use tmi_rs::client_messages::ClientMessage;
use tmi_rs::event::*;
use tmi_rs::selectors::priv_msg;
use tmi_rs::{TwitchChatConnection, TwitchClient, TwitchClientConfigBuilder};

/// To run this example, the TWITCH_CHANNEL, TWITCH_USERNAME and TWITCH_AUTH environment variables
/// need to be set.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();
    let channel = env::var("TWITCH_CHANNEL")?;
    let client: TwitchClient = TwitchClientConfigBuilder::default()
        .username(env::var("TWITCH_USERNAME")?)
        .token(env::var("TWITCH_AUTH")?)
        .build()?
        .into();

    let TwitchChatConnection {
        sender,
        receiver,
        error_receiver,
    } = client.connect().await?;
    let mut sender = &sender;

    // join a channel
    sender.send(ClientMessage::join(channel.clone())).await?;

    // process messages and do stuff with the data
    let process_messages = async {
        let send_result = receiver
            .filter_map(priv_msg)
            .filter_map(|event_data| {
                info!("{:?}", event_data);
                ready(if event_data.message().starts_with("!hello") {
                    // return response message to the stream
                    Some(ClientMessage::message(
                        event_data.channel().to_owned(),
                        "Hello World!",
                    ))
                } else {
                    None
                })
            })
            .map(Ok)
            .forward(sender)
            .await;

        if let Err(e) = send_result {
            error!("{}", e);
        }
    };

    // log any connection errors
    let process_errors = error_receiver.for_each(|error| {
        async move {
            error!("Connection error: {}", error);
        }
    });
    join(process_messages, process_errors).await;
    Ok(())
}
```

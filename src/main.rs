use anyhow::Result;
use std::env;
use tokio::signal;

use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt};
use twilight_http::Client as HttpClient;
use twilight_model::{
    application::{
        command::CommandType,
        interaction::{InteractionData},
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{marker::ApplicationMarker, Id},
};
use twilight_util::builder::command::CommandBuilder;

#[tokio::main]
async fn main() -> Result<()> {
    // Fix rustls provider selection (because both ring + aws-lc may exist)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls crypto provider");

    // Env
    let token = env::var("DISCORD_TOKEN")?;
    let application_id = Id::<ApplicationMarker>::new(env::var("APPLICATION_ID")?.parse()?);

    // HTTP (REST)
    let http = HttpClient::new(token.clone());

    // Register /ping (GLOBAL; may take time to appear)
    let ping_cmd = CommandBuilder::new("ping", "Replies with Pong!", CommandType::ChatInput).build();

    http.interaction(application_id)
        .set_global_commands(&[ping_cmd])
        .await?;

    // Gateway (WebSocket)
    let intents = Intents::GUILDS;
    let shard_id = ShardId::new(0, 1);
    let mut shard = Shard::new(shard_id, token, intents);

    println!("Connecting to Discord…");

    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                println!("Shutting down.");
                break;
            }

            event = shard.next_event(EventTypeFlags::all()) => {
                let event = match event {
                    Some(Ok(ev)) => ev,
                    Some(Err(e)) => {
                        eprintln!("Gateway error: {e:?}");
                        continue;
                    }
                    None => {
                        eprintln!("Gateway stream ended.");
                        break;
                    }
                };

                match event {
                    Event::Ready(ready) => {
                        println!("READY as {}", ready.user.name);
                    }

                    Event::InteractionCreate(ic) => {
                        let interaction = ic.0;

                        // Slash commands live in interaction.data as InteractionData::ApplicationCommand
                        if let Some(InteractionData::ApplicationCommand(cmd)) = interaction.data {
                            if cmd.name == "ping" {
                                let resp = InteractionResponse {
                                    kind: InteractionResponseType::ChannelMessageWithSource,
                                    data: Some(InteractionResponseData {
                                        content: Some("Pong!".into()),
                                        ..Default::default()
                                    }),
                                };

                                http.interaction(application_id)
                                    .create_response(interaction.id, &interaction.token, &resp)
                                    .await?;
                            }
                        }
                    }

                    _ => {}
                }
            }
        }
    }

    Ok(())
}


use std::collections::HashMap;
use bindings::region::{*, UserModerationPolicy::*};
use bindings::ext::ctx::*;
use bindings::sdk::{DbContext, Timestamp};

mod glue;
use glue::{Config, Configurable};

use serde;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender, UnboundedReceiver};

#[derive(serde::Serialize)]
#[serde(untagged)]
enum Message {
    Disconnect,
    Chat {
        username: String,
        content: String,
    }
}

impl Message {
    pub fn chat(username: String, content: String) -> Self { Self::Chat{ username, content } }

    pub fn claim(username: String, claim: &str, content: String) -> Self {
        Self::chat(format!("{} [{}]", username, claim), content)
    }

    pub fn empire(username: String, empire: &str, content: String) -> Self {
        Self::chat(format!("{} [{}]", username, empire), content)
    }

    pub fn moderation(username: String, policy: &str, expiry: &str) -> Self {
        Self::chat(
            "<<MODERATION>>".to_string(),
            format!("User {} has been banned from {} {}!", username, policy, expiry),
        )
    }
}

#[tokio::main]
async fn main() {
    let config = Config::from("config.json").expect("failed to load config.json");

    if config.is_empty() {
        eprintln!("please fill out the configuration file (config.json)!");
        return;
    }

    let (tx_ctx, rx_ctx) = unbounded_channel::<DbUpdate>();
    let (tx_msg, rx_msg) = unbounded_channel::<Message>();

    let tx_shutdown = tx_msg.clone();
    let ctx = DbConnection::builder()
        .configure(&config)
        .on_connect(|_, _, _| println!("connected!"))
        .on_disconnect(move |_, _| {
            println!("disconnected!");
            tx_shutdown.send(Message::Disconnect).unwrap();
        })
        .with_channel(tx_ctx)
        .build()
        .expect("failed to connect");

    let start = Timestamp::now();
    ctx.subscription_builder()
        .on_error(|_, err| eprintln!("subscription error: {}", err))
        .subscribe([
        "SELECT * FROM claim_state",
        "SELECT * FROM empire_state",
        "SELECT * FROM player_username_state",
        &format!(r"SELECT t.*
                   FROM chat_message_state t
                   WHERE t.channel_id > 2
                     AND t.timestamp > {}", start.to_micros_since_unix_epoch() / 1_000_000),
        &format!(r"SELECT t.*
                   FROM user_moderation_state t
                   WHERE t.created_time > '{}'", start),
    ]);

    let (con, _, _) = tokio::join!(
        tokio::spawn(ctx.run_until(tokio::signal::ctrl_c())),
        tokio::spawn(sieve(rx_ctx, tx_msg)),
        tokio::spawn(consume(rx_msg, config.webhook_url())),
    );

    if let Ok(Err(e)) = con { eprintln!("db error: {:?}", e); }
}

async fn sieve(mut rx: UnboundedReceiver<DbUpdate>, tx: UnboundedSender<Message>) {
    const EMPIRE_INTERNAL: i32 = ChatChannel::EmpireInternal as i32;
    const EMPIRE_PUBLIC: i32 = ChatChannel::EmpirePublic as i32;
    const CLAIM: i32 = ChatChannel::Claim as i32;
    const REGION: i32 = ChatChannel::Region as i32;


    let mut claims = HashMap::new();
    let mut empires = HashMap::new();
    let mut players = HashMap::new();


    while let Some(update) = rx.recv().await {
        for claim in update.claim_state.inserts {
            claims.insert(claim.row.entity_id, claim.row.name);
        }
        for empire in update.empire_state.inserts {
            empires.insert(empire.row.entity_id, empire.row.name);
        }
        for player in update.player_username_state.inserts {
            players.insert(player.row.entity_id, player.row.username);
        }

        for msg in update.chat_message_state.inserts {
            let msg = match msg.row.channel_id {
                EMPIRE_INTERNAL | EMPIRE_PUBLIC =>
                    empires
                        .get(&msg.row.target_id)
                        .map(|e| Message::empire(msg.row.username, e, msg.row.text)),
                CLAIM =>
                    claims
                        .get(&msg.row.target_id)
                        .map(|e| Message::claim(msg.row.username, e, msg.row.text)),
                REGION =>
                    Some(Message::chat(msg.row.username, msg.row.text)),
                _ => None,
            };

            if let Some(msg) = msg { tx.send(msg).unwrap() }
        }

        for msg in update.user_moderation_state.inserts {
            let user = players
                .get(&msg.row.target_entity_id)
                .map_or(format!("{{{}}}", msg.row.target_entity_id), &String::to_string);

            let msg = match msg.row.user_moderation_policy {
                PermanentBlockLogin =>
                    Message::moderation(user, "logging in", "permanently"),
                TemporaryBlockLogin =>
                    Message::moderation(user, "logging in", &as_expiry(msg.row.expiration_time)),
                BlockChat =>
                    Message::moderation(user, "chatting", &as_expiry(msg.row.expiration_time)),
                BlockConstruct =>
                    Message::moderation(user, "building", &as_expiry(msg.row.expiration_time)),
            };

            tx.send(msg).unwrap();
        }
    }
}

fn as_expiry(expiry: Timestamp) -> String {
    format!("until <t:{}:f>!", expiry.to_micros_since_unix_epoch() / 1_000_000)
}

async fn consume(mut rx: UnboundedReceiver<Message>, webhook_url: String) {
    let client = reqwest::Client::new();

    while let Some(msg) = rx.recv().await {
        match &msg {
            Message::Disconnect => { break }
            Message::Chat { username, content } => {
                println!("{}: {}", username, content);
                if webhook_url.is_empty() {
                    continue;
                }

                let payload = serde_json::to_string(&msg).unwrap();
                let response = client
                    .post(&webhook_url)
                    .header("Content-Type", "application/json")
                    .body(payload)
                    .send()
                    .await;

                if !response.is_ok_and(|r| r.status().is_success()) {
                    eprintln!("failed to send message");
                }
            }
        }
    }
}
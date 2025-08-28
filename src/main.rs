use bindings::region::{*, UserModerationPolicy::*};
use bindings::ext::{con::*, send::*};
use bindings::sdk::{DbContext, Timestamp};

mod glue;
use glue::{Config, Configurable};

use serde;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

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

    pub fn claim(username: String, claim: String, content: String) -> Self {
        Self::chat(format!("{} [{}]", username, claim), content)
    }

    pub fn empire(username: String, empire: String, content: String) -> Self {
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

    let (tx, rx) = unbounded_channel::<Message>();

    let tx_shutdown = tx.clone();
    let ctx = DbConnection::builder()
        .configure(&config)
        .on_connect(|_, _, _| println!("connected!"))
        .on_disconnect(move |_, _| {
            println!("disconnected!");
            tx_shutdown.send(Message::Disconnect).unwrap();
        })
        .build()
        .expect("failed to connect");

    ctx.db.chat_message_state().on_insert_send(&tx, on_message);
    ctx.db.user_moderation_state().on_insert_send(&tx, on_moderation);

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

    let (con, _) = tokio::join!(
        tokio::spawn(ctx.run_until(tokio::signal::ctrl_c())),
        tokio::spawn(consume(rx, config.webhook_url())),
    );

    if let Ok(Err(e)) = con { eprintln!("db error: {:?}", e); }
}

fn on_message(ctx: &EventContext, row: &ChatMessageState) -> Option<Message> {
    const EMPIRE_INTERNAL: i32 = ChatChannel::EmpireInternal as i32;
    const EMPIRE_PUBLIC: i32 = ChatChannel::EmpirePublic as i32;
    const CLAIM: i32 = ChatChannel::Claim as i32;
    const REGION: i32 = ChatChannel::Region as i32;

    match row.channel_id {
        EMPIRE_INTERNAL | EMPIRE_PUBLIC =>
            ctx.db.empire_state()
                .entity_id()
                .find(&row.target_id)
                .map(|e|
                    Message::empire(row.username.clone(), e.name, row.text.clone())),
        CLAIM =>
            ctx.db.claim_state()
                .entity_id()
                .find(&row.target_id)
                .map(|c|
                    Message::claim(row.username.clone(), c.name, row.text.clone())),
        REGION =>
            Some(Message::chat(row.username.clone(), row.text.clone())),
        _ => None,
    }
}

fn on_moderation(ctx: &EventContext, row: &UserModerationState) -> Option<Message> {
    let user = ctx.db.player_username_state()
        .entity_id()
        .find(&row.target_entity_id)
        .map_or_else(|| format!("{{{}}}", row.target_entity_id), |p| p.username);

    Some(match row.user_moderation_policy {
        PermanentBlockLogin =>
            Message::moderation(user, "logging in", "permanently"),
        TemporaryBlockLogin =>
            Message::moderation(user, "logging in", &as_expiry(row.expiration_time)),
        BlockChat =>
            Message::moderation(user, "chatting", &as_expiry(row.expiration_time)),
        BlockConstruct =>
            Message::moderation(user, "building", &as_expiry(row.expiration_time)),
    })
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
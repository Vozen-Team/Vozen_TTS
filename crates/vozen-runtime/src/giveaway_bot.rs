use std::{
    collections::HashSet,
    num::NonZeroU64,
    sync::{Arc, Mutex},
    time::Duration,
};

use serenity::{
    Client,
    all::{
        ButtonStyle, Channel, ChannelId, ChannelType, Context, CreateActionRow,
        CreateAllowedMentions, CreateButton, CreateEmbed, CreateEmbedFooter,
        CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
        EditInteractionResponse, EditMessage, EventHandler, GatewayIntents, GuildId, Http,
        Interaction, MessageId, Nonce, UserId,
    },
};
use vozen_store::{Giveaway, GiveawayAward, GiveawayClick, SqliteStore};

use crate::system_now_ms;

pub fn config() -> Option<(String, GuildId)> {
    let token = std::env::var("VOZEN_GIVEAWAY_BOT_TOKEN")
        .ok()?
        .trim()
        .to_owned();
    let id: u64 = std::env::var("VOZEN_GIVEAWAY_GUILD_ID")
        .ok()?
        .parse()
        .ok()?;
    if token.trim().is_empty()
        || id == 0
        || std::env::var("DISCORD_TOKEN")
            .ok()
            .is_some_and(|primary| primary.trim() == token)
    {
        return None;
    }
    Some((token, GuildId::new(id)))
}

pub fn spawn(store: Arc<Mutex<SqliteStore>>) {
    let Some((token, guild)) = config() else {
        return;
    };
    tokio::spawn(async move {
        let three_bots = three_bots_enabled(
            std::env::var("VOZEN_GIVEAWAY_THREE_BOTS_ENABLED")
                .ok()
                .as_deref(),
            [
                std::env::var("TOPGG_HELPER_WEBHOOK_SECRET").ok().as_deref(),
                std::env::var("TOPGG_MUSIC_WEBHOOK_SECRET").ok().as_deref(),
            ],
        );
        let bot = GiveawayBot {
            store,
            guild,
            three_bots,
        };
        // A transient store error must not disable giveaways for the rest of the process.
        while bot
            .db(|db| db.giveaway_refresh_open(&guild.to_string()))
            .is_err()
        {
            eprintln!("[giveaways] failed to refresh announcements; retrying in 30s");
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        let client = Client::builder(&token, GatewayIntents::GUILDS)
            .event_handler(bot.clone())
            .await;
        let Ok(mut client) = client else {
            eprintln!("[giveaways] Discord client could not be created");
            return;
        };
        let http = client.http.clone();
        // Publication, draws and winner DMs only need REST, so they keep running even if the
        // gateway connection ends.
        let jobs = bot.clone();
        tokio::spawn(async move { jobs.run(&http).await });
        if let Err(error) = client.start().await {
            eprintln!("[giveaways] Discord gateway stopped: {error}");
        } else {
            eprintln!("[giveaways] Discord gateway stopped");
        }
    });
}

type BotError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
struct GiveawayBot {
    store: Arc<Mutex<SqliteStore>>,
    guild: GuildId,
    three_bots: bool,
}

impl GiveawayBot {
    fn db<T>(
        &self,
        operation: impl FnOnce(&SqliteStore) -> Result<T, vozen_store::StoreError>,
    ) -> Result<T, BotError> {
        let store = self
            .store
            .lock()
            .map_err(|_| "giveaway store unavailable")?;
        Ok(operation(&store)?)
    }

    async fn run(&self, http: &Http) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let jobs = self.db(|db| db.giveaway_jobs(&self.guild.to_string(), system_now_ms()));
            let Ok(jobs) = jobs else {
                eprintln!("[giveaways] failed to load jobs");
                continue;
            };
            for giveaway in jobs {
                if let Err(error) = self.process(http, &giveaway).await {
                    eprintln!("[giveaways] campaign {}: {error}", giveaway.id);
                }
            }
        }
    }

    async fn process(&self, http: &Http, g: &Giveaway) -> Result<(), BotError> {
        let now = system_now_ms();
        let channel = ChannelId::new(g.channel_id.parse()?);
        if g.status == "scheduled" {
            if g.ends_at <= now {
                return self.db(|db| {
                    db.giveaway_error(g.id, "The giveaway ended before it could be published.")
                });
            }
            let verified = match channel.to_channel(http).await {
                Ok(channel) => channel,
                Err(error) if discord_permission_error(&error) => {
                    return self
                        .db(|db| db.giveaway_error(g.id, "The bot cannot access this channel."));
                }
                Err(error) => return Err(error.into()),
            };
            if !matches!(verified, Channel::Guild(ref c) if c.guild_id == self.guild
                && matches!(c.kind, ChannelType::Text | ChannelType::News))
            {
                return self
                    .db(|db| db.giveaway_error(g.id, "Choose a text channel in Vozen Support."));
            }
            if !self.db(|db| db.giveaway_publish_attempt(g.id, now))? {
                return self.db(|db| {
                    db.giveaway_error(
                        g.id,
                        "Publication could not be confirmed. Check the channel before cancelling.",
                    )
                });
            }
            let (embed, components) = announcement(g, true, self.three_bots);
            let message = channel
                .send_message(
                    http,
                    CreateMessage::new()
                        .embed(embed)
                        .components(components)
                        .allowed_mentions(CreateAllowedMentions::new())
                        .nonce(Nonce::String(g.id.to_string()))
                        .enforce_nonce(true),
                )
                .await;
            let message = match message {
                Ok(message) => message,
                Err(error) if discord_permission_error(&error) => {
                    return self.db(|db| {
                        db.giveaway_error(
                            g.id,
                            "The bot could not post in this channel. Check its permissions.",
                        )
                    });
                }
                Err(error) => return Err(error.into()),
            };
            return self.db(|db| db.giveaway_published(g.id, &message.id.to_string()));
        }
        if g.status == "open" && g.ends_at <= now {
            let winners = self.eligible(http, g.id, g.winner_count as usize).await?;
            return self.db(|db| db.giveaway_draw(g.id, None, &winners, system_now_ms()));
        }
        if let Some(award) = g.awards.iter().find(|award| award.replace_requested) {
            let winners = self.eligible(http, g.id, 1).await?;
            return self.db(|db| db.giveaway_draw(g.id, Some(award.id), &winners, system_now_ms()));
        }
        for award in g.awards.iter().filter(|a| {
            !a.replaced
                && a.accepted_at.is_none()
                && a.deadline > now
                && a.dm_message.is_none()
                && !a.dm_blocked
                && a.dm_attempt.is_none_or(|attempt| attempt <= now - 60_000)
        }) {
            self.notify(http, g, award).await?;
        }
        for award in g
            .awards
            .iter()
            .filter(|a| a.accepted_at.is_some() && a.expires_at.is_none())
        {
            let _ = self.db(|db| db.giveaway_deliver(award.id, now));
        }
        if let Some(msg) = &g.message_id
            && g.synced < g.revision
        {
            let (embed, components) = announcement(g, false, self.three_bots);
            let edited = channel
                .edit_message(
                    http,
                    MessageId::new(msg.parse()?),
                    EditMessage::new()
                        .content("")
                        .embeds(vec![embed])
                        .components(components)
                        .allowed_mentions(CreateAllowedMentions::new()),
                )
                .await;
            match edited {
                Ok(_) => {}
                // The announcement was deleted or the bot lost access. Retrying every tick would
                // only add rate-limit pressure; the giveaway itself still draws and notifies.
                Err(error) if discord_permission_error(&error) => {
                    eprintln!(
                        "[giveaways] campaign {}: announcement can no longer be edited",
                        g.id
                    );
                }
                Err(error) => return Err(error.into()),
            }
            self.db(|db| db.giveaway_sync(g.id, g.revision))?;
        }
        Ok(())
    }

    async fn notify(
        &self,
        http: &Http,
        g: &Giveaway,
        award: &GiveawayAward,
    ) -> Result<(), BotError> {
        self.db(|db| db.giveaway_dm_attempt(award.id, system_now_ms()))?;
        let user = UserId::from(award.user_id.parse::<NonZeroU64>()?);
        let result = async {
            let channel=user.create_dm_channel(http).await?;
            channel.send_message(http,CreateMessage::new()
                .embed(CreateEmbed::new().colour(0xA4A4FF).title("🎉 You won a Vozen giveaway!")
                    .description(format!("Your prize: **{}**.\n\nClaim it before <t:{}:F> (<t:{}:R>).\nUse the button below to activate your prize.",
                        prize(g),award.deadline/1000,award.deadline/1000)))
                .components(vec![CreateActionRow::Buttons(vec![button(g.id,"claim","Activate prize",ButtonStyle::Success)])])
                .allowed_mentions(CreateAllowedMentions::new())
                .nonce(Nonce::String(format!("award:{}",award.id))).enforce_nonce(true)).await
        }.await;
        match result {
            Ok(message) => self.db(|db| {
                db.giveaway_dm_sent(
                    award.id,
                    &message.channel_id.to_string(),
                    &message.id.to_string(),
                )
            }),
            Err(error) if discord_permission_error(&error) => {
                self.db(|db| db.giveaway_dm_blocked(g.id, award.id))
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn eligible(&self, http: &Http, id: i64, count: usize) -> Result<Vec<String>, BotError> {
        let mut winners = Vec::new();
        let mut seen = HashSet::new();
        for candidate in self.db(|db| db.giveaway_candidates(id, self.three_bots))? {
            if !seen.insert(candidate.clone()) {
                continue;
            }
            // `UserId::new` panics on zero; an unusable ticket must not stop the whole draw.
            let Ok(user_id) = candidate.parse::<NonZeroU64>() else {
                eprintln!("[giveaways] campaign {id}: skipped a ticket with an invalid user id");
                continue;
            };
            match self.guild.member(http, UserId::from(user_id)).await {
                Ok(member) if !member.user.bot => winners.push(candidate),
                Ok(_) => (),
                Err(error) if matches!(&error, serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) if response.error.code == 10007) =>
                    {}
                Err(error) => return Err(error.into()),
            }
            if winners.len() == count {
                break;
            }
        }
        Ok(winners)
    }
}

fn discord_permission_error(error: &serenity::Error) -> bool {
    matches!(error, serenity::Error::Http(http)
        if http.status_code().is_some_and(|status| matches!(status.as_u16(), 403 | 404)))
}

fn button(g: i64, action: &str, label: &str, style: ButtonStyle) -> CreateButton {
    CreateButton::new(format!("giveaway:{g}:{action}"))
        .label(label)
        .style(style)
}

const VOTE_RULE: &str = "Up to 3 entries: one vote per bot";

fn three_bots_enabled(flag: Option<&str>, secrets: [Option<&str>; 2]) -> bool {
    flag == Some("true")
        && secrets
            .iter()
            .all(|secret| secret.is_some_and(|value| !value.trim().is_empty()))
}

fn prize(g: &Giveaway) -> String {
    let plan = if g.plan == "plus" {
        "Plus".to_owned()
    } else {
        format!("Premium · {} servers", g.seats)
    };
    let duration = if g.days == 30 {
        "1 Month".to_owned()
    } else if g.days == 365 {
        "1 Year".to_owned()
    } else {
        format!("{} Days", g.days)
    };
    format!("Vozen {plan} ({duration})")
}

fn announcement(
    g: &Giveaway,
    opening: bool,
    three_bots: bool,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let mut embed = CreateEmbed::new().colour(0xA4A4FF);
    if !g.description.is_empty() {
        embed = embed.field("Details", &g.description, false);
    }
    if opening || g.status == "open" {
        let plural = if g.winner_count == 1 {
            "Winner"
        } else {
            "Winners"
        };
        embed = embed
            .title(format!("🎉 Giveaway for {}", prize(g)))
            .description(format!(
                "{} {plural}\n\nEnds: <t:{}:R>",
                g.winner_count,
                g.ends_at / 1000
            ))
            .footer(CreateEmbedFooter::new(if three_bots {
                VOTE_RULE
            } else {
                "One entry per person: vote for TTS"
            }));
        return (
            embed,
            vec![CreateActionRow::Buttons(
                vozen_core::GIVEAWAY_VOTE_BOTS[..if three_bots { 3 } else { 1 }]
                    .iter()
                    .map(|(name, id)| {
                        CreateButton::new_link(format!("https://top.gg/bot/{id}/vote"))
                            .label(format!("Vote for {name}"))
                    })
                    .collect(),
            )],
        );
    }
    if g.status == "cancelled" {
        return (
            embed
                .title(format!("Giveaway cancelled · {}", prize(g)))
                .description("This giveaway has been cancelled."),
            Vec::new(),
        );
    }
    let winners: Vec<_> = g.awards.iter().filter(|a| !a.replaced).collect();
    let mut body = format!("Ended: <t:{}:R>\n", g.ends_at / 1000);
    if winners.is_empty() {
        body.push_str("\nNo eligible entries were received.");
    } else {
        for (index, award) in winners.iter().enumerate() {
            body.push_str(&format!(
                "\nWinner #{}: <@{}> ({} entries)",
                index + 1,
                award.user_id,
                award.entries
            ));
        }
        body.push_str("\n\nCheck your DM from Vozen Giveaways to activate your prize.");
    }
    let blocked = winners.iter().any(|a| a.dm_blocked);
    if blocked {
        body.push_str("\nIf your DMs are closed, use the claim button below.");
    }
    let components = if blocked {
        vec![CreateActionRow::Buttons(vec![button(
            g.id,
            "claim",
            "Claim prize",
            ButtonStyle::Success,
        )])]
    } else {
        Vec::new()
    };
    (
        embed
            .title(format!("🎉 Giveaway ended · {}", prize(g)))
            .description(body),
        components,
    )
}

#[serenity::async_trait]
impl EventHandler for GiveawayBot {
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Component(click) = interaction else {
            return;
        };
        if click.user.bot || click.guild_id.is_some_and(|guild| guild != self.guild) {
            return;
        }
        let Some(custom) = click.data.custom_id.strip_prefix("giveaway:") else {
            return;
        };
        let Some((id, action)) = custom.split_once(':') else {
            return;
        };
        let Ok(id) = id.parse() else {
            return;
        };
        if click
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Defer(
                    CreateInteractionResponseMessage::new().ephemeral(true),
                ),
            )
            .await
            .is_err()
        {
            return;
        }
        let original = if click.guild_id.is_none() {
            self.db(|db| {
                db.giveaway_dm_original(
                    id,
                    &click.user.id.to_string(),
                    &click.message.id.to_string(),
                )
            })
        } else {
            Ok(Some(click.message.id.to_string()))
        };
        let text = match original {
            Ok(Some(message)) => {
                if click.guild_id.is_none()
                    && self.guild.member(&ctx.http, click.user.id).await.is_err()
                {
                    "Please join Vozen Support and try again. Your prize is reserved until the claim deadline.".to_owned()
                } else {
                    let result = self.db(|db| {
                        db.giveaway_click(
                            &self.guild.to_string(),
                            &message,
                            id,
                            &click.user.id.to_string(),
                            action,
                            system_now_ms(),
                        )
                    });
                    match result {
                        Ok(GiveawayClick::VoteRequired) if self.three_bots => "Vote using the TTS, Helper or Music buttons on the giveaway message. Each bot gives one entry, up to 3 entries per person for this giveaway.".into(),
                        Ok(GiveawayClick::VoteRequired) => "Vote using the TTS button on the giveaway message. One entry per person for this giveaway.".into(),
                        Ok(GiveawayClick::Left) => "Your entries have been removed.".to_owned(),
                        Ok(GiveawayClick::Closed) => "This giveaway is closed.".to_owned(),
                        Ok(GiveawayClick::NotWinner) => "This prize belongs to another participant.".to_owned(),
                        Ok(GiveawayClick::Expired) => "The deadline to claim this prize has passed.".to_owned(),
                        Ok(GiveawayClick::Waiting) => "Prize reserved. It will activate after your paid subscription of the same type ends.".to_owned(),
                        Ok(GiveawayClick::Delivered(expiry)) => format!("Your prize is active until <t:{}:F>. Manage your Vozen plans and Premium servers at https://vozen.org/account/.",expiry/1000),
                        Err(_) => "Vozen could not complete this request. Please try again.".to_owned(),
                    }
                }
            }
            Ok(None) => {
                "This prize message is invalid or belongs to another participant.".to_owned()
            }
            Err(_) => "Vozen could not complete this request. Please try again.".to_owned(),
        };
        let _ = click
            .edit_response(&ctx.http, EditInteractionResponse::new().content(text))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_vote_buttons_require_explicit_activation_and_both_secrets() {
        assert!(!three_bots_enabled(None, [Some("helper"), Some("music")]));
        assert!(!three_bots_enabled(
            Some("false"),
            [Some("helper"), Some("music")]
        ));
        assert!(!three_bots_enabled(Some("true"), [Some("helper"), None]));
        assert!(!three_bots_enabled(
            Some("true"),
            [Some("helper"), Some(" ")]
        ));
        assert!(three_bots_enabled(
            Some("true"),
            [Some("helper"), Some("music")]
        ));
    }

    #[test]
    fn cards_use_english_embeds_vote_links_and_closed_dm_fallback() {
        let store = SqliteStore::open_in_memory().unwrap();
        let action=serde_json::from_value(serde_json::json!({
            "action":"save","request_id":"ea01298f-a728-489a-9639-856befd711cc",
            "channel_id":"200","title":"Internal label","description":"","plan":"plus","days":30,"seats":0,
            "winner_count":1,"starts_at":0,"ends_at":3600000,"claim_hours":48
        })).unwrap();
        store.giveaway_admin("100", action, 1).unwrap().unwrap();
        let mut g = store.giveaways("100").unwrap().remove(0);
        let (embed, buttons) = announcement(&g, true, true);
        let card = serde_json::to_value(embed).unwrap();
        assert_eq!(card["title"], "🎉 Giveaway for Vozen Plus (1 Month)");
        assert_eq!(card["footer"]["text"], VOTE_RULE);
        let buttons = serde_json::to_value(buttons).unwrap();
        assert_eq!(buttons[0]["components"].as_array().unwrap().len(), 3);
        for (index, (name, id)) in vozen_core::GIVEAWAY_VOTE_BOTS.iter().enumerate() {
            assert_eq!(
                buttons[0]["components"][index]["url"],
                format!("https://top.gg/bot/{id}/vote")
            );
            assert_eq!(
                buttons[0]["components"][index]["label"],
                format!("Vote for {name}")
            );
        }
        let (pending_card, pending_buttons) = announcement(&g, true, false);
        assert_eq!(
            serde_json::to_value(pending_card).unwrap()["footer"]["text"],
            "One entry per person: vote for TTS"
        );
        let pending_buttons = serde_json::to_value(pending_buttons).unwrap();
        assert_eq!(
            pending_buttons[0]["components"].as_array().unwrap().len(),
            1
        );
        assert_eq!(pending_buttons[0]["components"][0]["label"], "Vote for TTS");
        g.status = "closed".into();
        g.awards.push(GiveawayAward {
            id: 1,
            user_id: "501".into(),
            deadline: 9000000,
            accepted_at: None,
            expires_at: None,
            replaced: false,
            replace_requested: false,
            delivery_error: false,
            entries: 3,
            dm_channel: None,
            dm_message: None,
            dm_attempt: None,
            dm_blocked: true,
        });
        let (embed, buttons) = announcement(&g, false, true);
        assert!(
            serde_json::to_value(embed).unwrap()["description"]
                .as_str()
                .unwrap()
                .contains("<@501> (3 entries)")
        );
        assert_eq!(
            serde_json::to_value(buttons).unwrap()[0]["components"][0]["label"],
            "Claim prize"
        );
    }
    #[tokio::test]
    async fn winner_dm_persists_the_message_and_enables_public_claim_when_discord_blocks_delivery()
    {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use serenity::http::HttpBuilder;
        let now = 1_800_000_000_000;
        let user = "12345678901234567";
        for blocked in [false, true] {
            let store = SqliteStore::open_in_memory().unwrap();
            let action = serde_json::from_value(serde_json::json!({
                "action":"save","request_id":"ea01298f-a728-489a-9639-856befd711cc",
                "channel_id":"200","title":"","description":"","plan":"plus","days":30,"seats":0,
                "winner_count":1,"starts_at":now,"ends_at":now+1000,"claim_hours":48
            }))
            .unwrap();
            let id = store.giveaway_admin("100", action, now).unwrap().unwrap();
            store
                .giveaway_admin("100", vozen_store::GiveawayAction::Schedule { id }, now)
                .unwrap();
            store.giveaway_published(id, "300").unwrap();
            store
                .claim_topgg_vote_reward_at(
                    Some("vote"),
                    user,
                    now + 1,
                    "0123456789abcdef0123456789abcdef",
                    Some(now),
                )
                .unwrap();
            store
                .giveaway_draw(id, None, &[user.into()], now + 1000)
                .unwrap();
            let g = store.giveaways("100").unwrap().remove(0);
            let mut dm = serenity::all::PrivateChannel::default();
            dm.id = ChannelId::new(400);
            dm.kind = ChannelType::Private;
            dm.recipient.id = UserId::new(user.parse().unwrap());
            let dm_json = serde_json::to_value(dm).unwrap();
            let recorded = Arc::new(Mutex::new(None));
            let receiver = recorded.clone();
            let server=Router::new()
                .route("/api/v10/users/@me/channels",post(move || {
                    let data=dm_json.clone();
                    async move { Json(data) }
                }))
                .route("/api/v10/channels/400/messages",post(move |Json(payload):Json<serde_json::Value>| {
                    *receiver.lock().unwrap()=Some(payload);
                    async move {
                        if blocked {
                            (StatusCode::FORBIDDEN,Json(serde_json::json!({"code":50007,"message":"Cannot send messages to this user"})))
                        } else {
                            let mut message=serenity::all::Message::default();
                            message.id=MessageId::new(500);message.channel_id=ChannelId::new(400);
                            (StatusCode::OK,Json(serde_json::to_value(message).unwrap()))
                        }
                    }
                }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                axum::serve(listener, server).await.unwrap();
            });
            let http = HttpBuilder::new("synthetic-token")
                .proxy(format!("http://{address}"))
                .ratelimiter_disabled(true)
                .build();
            let bot = GiveawayBot {
                store: Arc::new(Mutex::new(store)),
                guild: GuildId::new(100),
                three_bots: false,
            };
            bot.notify(&http, &g, &g.awards[0]).await.unwrap();
            let payload = recorded.lock().unwrap().clone().unwrap();
            assert_eq!(
                payload["embeds"][0]["title"],
                "🎉 You won a Vozen giveaway!"
            );
            assert_eq!(
                payload["components"][0]["components"][0]["custom_id"],
                format!("giveaway:{id}:claim")
            );
            let state = bot.db(|db| db.giveaways("100")).unwrap();
            assert_eq!(state[0].awards[0].dm_blocked, blocked);
            if blocked {
                assert!(state[0].awards[0].dm_message.is_none());
            } else {
                assert_eq!(
                    bot.db(|db| db.giveaway_dm_original(id, user, "500"))
                        .unwrap(),
                    Some("300".into())
                );
            }
            task.abort();
        }
    }
}

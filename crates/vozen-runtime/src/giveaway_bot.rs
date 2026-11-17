use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use serenity::{
    Client,
    all::{
        ButtonStyle, Channel, ChannelId, ChannelType, Context, CreateActionRow,
        CreateAllowedMentions, CreateButton, CreateInteractionResponse,
        CreateInteractionResponseMessage, CreateMessage, EditMessage, EventHandler, GatewayIntents,
        GuildId, Http, Interaction, MessageId, Nonce, UserId,
    },
};
use vozen_store::{Giveaway, GiveawayClick, SqliteStore};

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
        let bot = GiveawayBot { store, guild };
        let client = Client::builder(&token, GatewayIntents::GUILDS)
            .event_handler(bot.clone())
            .await;
        let Ok(mut client) = client else {
            eprintln!("[giveaways] Discord client could not be created");
            return;
        };
        let http = client.http.clone();
        tokio::select! {
            _ = client.start() => eprintln!("[giveaways] Discord gateway stopped"),
            _ = bot.run(&http) => (),
        }
    });
}

type BotError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
struct GiveawayBot {
    store: Arc<Mutex<SqliteStore>>,
    guild: GuildId,
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
                return self
                    .db(|db| db.giveaway_error(g.id, "O prazo terminou antes da publicação."));
            }
            let verified = match channel.to_channel(http).await {
                Ok(channel) => channel,
                Err(error) if discord_permission_error(&error) => {
                    return self.db(|db| {
                        db.giveaway_error(g.id, "O bot não tem acesso ao canal indicado.")
                    });
                }
                Err(error) => return Err(error.into()),
            };
            if !matches!(verified, Channel::Guild(ref c) if c.guild_id == self.guild
                && matches!(c.kind, ChannelType::Text | ChannelType::News))
            {
                return self.db(|db| {
                    db.giveaway_error(g.id, "Escolhe um canal de texto do servidor de suporte.")
                });
            }
            if !self.db(|db| db.giveaway_publish_attempt(g.id, now))? {
                return self.db(|db| {
                    db.giveaway_error(
                        g.id,
                        "Publicação por confirmar. Verifica o canal antes de cancelar.",
                    )
                });
            }
            let (content, components) = announcement(g, true);
            let message = channel
                .send_message(
                    http,
                    CreateMessage::new()
                        .content(content)
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
                            "O bot não conseguiu publicar neste canal. Verifica as permissões.",
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
            let (content, components) = announcement(g, false);
            channel
                .edit_message(
                    http,
                    MessageId::new(msg.parse()?),
                    EditMessage::new()
                        .content(content)
                        .components(components)
                        .allowed_mentions(CreateAllowedMentions::new()),
                )
                .await?;
            self.db(|db| db.giveaway_sync(g.id, g.revision))?;
        }
        Ok(())
    }

    async fn eligible(&self, http: &Http, id: i64, count: usize) -> Result<Vec<String>, BotError> {
        let mut winners = Vec::new();
        for candidate in self.db(|db| db.giveaway_candidates(id))? {
            match self
                .guild
                .member(http, UserId::new(candidate.parse()?))
                .await
            {
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

fn announcement(g: &Giveaway, opening: bool) -> (String, Vec<CreateActionRow>) {
    let plan = if g.plan == "plus" {
        "Plus pessoal".to_string()
    } else {
        format!("Premium para {} servidores", g.seats)
    };
    let title = if g.title.is_empty() {
        "Giveaway Vozen"
    } else {
        &g.title
    };
    let mut content = format!(
        "🎉 **{title}**\nPrémio por vencedor: **{} dias de Vozen {plan}**.\nVencedores: **{}** · Prazo de ativação: **{} h**.\nInscrições até <t:{}:F>.",
        g.days,
        g.winner_count,
        g.claim_hours,
        g.ends_at / 1000
    );
    if !g.description.is_empty() {
        content.push('\n');
        content.push_str(&g.description);
    }
    let buttons = if opening {
        vec![
            button(g.id, "join", "Participar", ButtonStyle::Primary),
            button(
                g.id,
                "leave",
                "Retirar participação",
                ButtonStyle::Secondary,
            ),
        ]
    } else if g.status == "cancelled" {
        content.push_str(&format!(
            "\n\n**Cancelado.** {}",
            g.cancel_reason.as_deref().unwrap_or("")
        ));
        Vec::new()
    } else {
        let active: Vec<_> = g.awards.iter().filter(|award| !award.replaced).collect();
        if active.is_empty() {
            content.push_str("\n\n**Encerrado sem participantes elegíveis.**");
            Vec::new()
        } else {
            content.push_str("\n\n**Vencedores**");
            for award in active {
                content.push_str(&format!(
                    "\n<@{}> · ativar até <t:{}:f>",
                    award.user_id,
                    award.deadline / 1000
                ));
            }
            vec![button(g.id, "claim", "Ativar prémio", ButtonStyle::Success)]
        }
    };
    let components = if buttons.is_empty() {
        Vec::new()
    } else {
        vec![CreateActionRow::Buttons(buttons)]
    };
    (content, components)
}

#[serenity::async_trait]
impl EventHandler for GiveawayBot {
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Component(click) = interaction else {
            return;
        };
        if click.guild_id != Some(self.guild) || click.user.bot {
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
        let result = self.db(|db| {
            db.giveaway_click(
                &self.guild.to_string(),
                &click.message.id.to_string(),
                id,
                &click.user.id.to_string(),
                action,
                system_now_ms(),
            )
        });
        let text = match result {
            Ok(GiveawayClick::Joined) => "Participação registada.".into(),
            Ok(GiveawayClick::AlreadyJoined) => "Já estás inscrito.".into(),
            Ok(GiveawayClick::Left) => "Participação retirada.".into(),
            Ok(GiveawayClick::Closed) => "Este giveaway está fechado.".into(),
            Ok(GiveawayClick::NotWinner) => "Este prémio pertence a outro participante.".into(),
            Ok(GiveawayClick::Expired) => "O prazo para ativar este prémio terminou.".into(),
            Ok(GiveawayClick::Waiting) => {
                "Prémio reservado. Será aplicado quando terminar a subscrição paga do mesmo tipo."
                    .into()
            }
            Ok(GiveawayClick::Delivered(expiry)) => format!(
                "Prémio ativado até <t:{}:F>. Se ganhaste Premium, usa o painel do Vozen para escolher os servidores.",
                expiry / 1000
            ),
            Err(_) => "O Vozen não conseguiu concluir a operação. Tenta novamente.".into(),
        };
        let _ = click
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .content(text),
                ),
            )
            .await;
    }
}

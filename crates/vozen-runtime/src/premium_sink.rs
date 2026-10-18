//! Opt-in gateway adapter for read-only `/premium info`.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serenity::{
    builder::{
        CreateActionRow, CreateButton, CreateInteractionResponse, CreateInteractionResponseMessage,
    },
    client::Context,
    model::{
        Permissions,
        application::{ComponentInteraction, Interaction},
    },
};
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, PremiumCommand, VoiceResponseLocalizer,
    parse_premium_command,
};
use vozen_store::{
    ActivateStatus, EntitlementGrant, PremiumKind, SqliteStore, VOTE_REDEMPTION_SECRET_MIN_LENGTH,
};

const ACTIVATION_TTL_SECONDS: i64 = 30;

fn activation_within_limit(activations: &[String], seats: i64, guild_id: &str) -> bool {
    activations
        .iter()
        .take(usize::try_from(seats).unwrap_or(0))
        .any(|id| id == guild_id)
}

#[derive(Clone)]
pub struct PremiumGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    kofi_url: String,
    payments_enabled: bool,
    client_id: Option<String>,
    redemption_secret: Option<String>,
    guild_sku_id: Option<u64>,
    user_sku_id: Option<u64>,
    localizer: VoiceResponseLocalizer,
    entitlement_sync_state: Arc<Mutex<EntitlementSyncState>>,
}

impl PremiumGatewaySink {
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        kofi_url: String,
        payments_enabled: bool,
        client_id: Option<String>,
        redemption_secret: Option<String>,
        guild_sku_id: Option<u64>,
        user_sku_id: Option<u64>,
    ) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            store,
            kofi_url,
            payments_enabled,
            client_id,
            redemption_secret,
            guild_sku_id,
            user_sku_id,
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
            entitlement_sync_state: Arc::new(Mutex::new(EntitlementSyncState::default())),
        })
    }

    fn message(
        &self,
        key: &str,
        command: &serenity::model::application::CommandInteraction,
        parameters: &BTreeMap<&str, String>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(
                key,
                Some(&command.locale),
                command.guild_locale.as_deref(),
                parameters,
            )
            .ok_or(GatewayEventDispatchError)
    }

    fn response(
        &self,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<(String, bool), GatewayEventDispatchError> {
        let now = now_ms();
        let user_id = command.user.id.get().to_string();
        let store = self.store.lock().map_err(|_| GatewayEventDispatchError)?;
        let guild_active = command
            .guild_id
            .map(|id| id.get().to_string())
            .map(|guild_id| {
                let active = store
                    .is_guild_premium(&guild_id, now)
                    .map_err(|_| GatewayEventDispatchError)?;
                let expiry = store
                    .effective_guild_premium_expiry(&guild_id, now)
                    .map_err(|_| GatewayEventDispatchError)?;
                Ok::<_, GatewayEventDispatchError>((active, expiry))
            })
            .transpose()?
            .unwrap_or((false, None));
        let status = store
            .premium_status(&user_id, now)
            .map_err(|_| GatewayEventDispatchError)?;
        let mut lines = Vec::new();
        if command.guild_id.is_some() {
            if guild_active.0 {
                let mut parameters = BTreeMap::new();
                parameters.insert("date", discord_date(guild_active.1.unwrap_or(now)));
                lines.push(self.message("premium.lineServerActive", command, &parameters)?);
            } else {
                lines.push(self.message("premium.lineServerFree", command, &BTreeMap::new())?);
            }
        }
        let pass_active = status.pass.as_ref().is_some_and(|pass| pass.active);
        if let Some(pass) = status.pass.as_ref().filter(|pass| pass.active) {
            let mut parameters = BTreeMap::new();
            parameters.insert("used", pass.used.to_string());
            parameters.insert("total", pass.seats.to_string());
            parameters.insert("date", discord_date(pass.expires_at));
            lines.push(self.message("premium.linePass", command, &parameters)?);
            if !pass.guilds.is_empty() {
                let mut parameters = BTreeMap::new();
                parameters.insert("servers", pass.guilds.join(", "));
                lines.push(self.message("premium.passServers", command, &parameters)?);
            }
        }
        if status.plus_active {
            let mut parameters = BTreeMap::new();
            parameters.insert("date", discord_date(status.plus_expires_at.unwrap_or(now)));
            lines.push(self.message("premium.lineUserActive", command, &parameters)?);
        } else {
            lines.push(self.message("premium.lineUserFree", command, &BTreeMap::new())?);
        }
        let any_active = guild_active.0 || pass_active || status.plus_active;
        if any_active {
            lines.push(String::new());
            lines.push(self.message("premium.enginePerks", command, &BTreeMap::new())?);
        } else if !self.payments_enabled {
            lines.push("Payments are currently disabled. Stripe checkout is coming soon.".into());
        } else {
            lines.push(self.message("premium.pitch", command, &BTreeMap::new())?);
            lines.push(String::new());
            lines.push(self.message("premium.enginePerks", command, &BTreeMap::new())?);
            lines.push(String::new());
            let mut parameters = BTreeMap::new();
            parameters.insert("link", self.kofi_url.clone());
            lines.push(self.message("premium.buyHint", command, &parameters)?);
            if let Some(secret) = self.redemption_secret.as_deref()
                && secret.len() >= VOTE_REDEMPTION_SECRET_MIN_LENGTH
            {
                let eligible = store
                    .vote_reward_status(&user_id, secret, now)
                    .map_err(|_| GatewayEventDispatchError)?
                    .eligible;
                if eligible && let Some(client_id) = self.client_id.as_deref() {
                    lines.push(String::new());
                    let mut parameters = BTreeMap::new();
                    parameters.insert("url", format!("https://top.gg/bot/{client_id}/vote"));
                    lines.push(self.message("vote.upsell", command, &parameters)?);
                }
            }
        }
        Ok((lines.join("\n"), any_active))
    }

    fn component_message(
        &self,
        key: &str,
        component: &ComponentInteraction,
        parameters: &BTreeMap<&str, String>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(
                key,
                Some(&component.locale),
                component.guild_locale.as_deref(),
                parameters,
            )
            .ok_or(GatewayEventDispatchError)
    }

    fn command_permission(
        &self,
        command: &serenity::model::application::CommandInteraction,
    ) -> bool {
        command
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .is_some_and(|permissions| permissions.contains(Permissions::MANAGE_GUILD))
    }

    fn activation_id(action: &str, user_id: &str, guild_id: &str, issued_at: i64) -> String {
        format!("premium:activate:{action}:{user_id}:{guild_id}:{issued_at}")
    }

    fn parse_activation_id(id: &str) -> Option<(&str, &str, &str, i64)> {
        let mut parts = id.split(':');
        if parts.next()? != "premium" || parts.next()? != "activate" {
            return None;
        }
        let action = parts.next()?;
        let user = parts.next()?;
        let guild = parts.next()?;
        let issued = parts.next()?.parse().ok()?;
        (parts.next().is_none() && matches!(action, "yes" | "no"))
            .then_some((action, user, guild, issued))
    }

    fn spawn_entitlement_sync(&self, context: Context) {
        if !self.payments_enabled || (self.guild_sku_id.is_none() && self.user_sku_id.is_none()) {
            return;
        }
        let should_start = {
            let Ok(mut state) = self.entitlement_sync_state.lock() else {
                eprintln!("[premium] Discord entitlement synchronization state is poisoned.");
                return;
            };
            if state.running {
                state.queued = true;
                false
            } else {
                state.running = true;
                true
            }
        };
        if !should_start {
            return;
        }
        let sink = self.clone();
        tokio::spawn(async move {
            loop {
                if sink.sync_discord_entitlements(&context).await.is_err() {
                    eprintln!(
                        "[premium] Discord entitlement synchronization failed; keeping the previous projection."
                    );
                }
                let continue_sync = {
                    let Ok(mut state) = sink.entitlement_sync_state.lock() else {
                        eprintln!(
                            "[premium] Discord entitlement synchronization state is poisoned."
                        );
                        break;
                    };
                    if state.queued {
                        state.queued = false;
                        true
                    } else {
                        state.running = false;
                        false
                    }
                };
                if !continue_sync {
                    break;
                }
            }
        });
    }

    /// Fetches the complete Discord Premium Apps entitlement set before replacing the Rust
    /// projection. A partial page must never be written: `sync_discord_entitlements` removes
    /// stale Discord grants by design, so an API failure or pagination bug must preserve the
    /// previous state instead of revoking paying users.
    async fn sync_discord_entitlements(
        &self,
        context: &Context,
    ) -> Result<(), GatewayEventDispatchError> {
        if self.guild_sku_id.is_none() && self.user_sku_id.is_none() {
            return Ok(());
        }
        const PAGE_SIZE: u8 = 100;
        const MAX_PAGES: usize = 1_000;

        let sku_ids = [self.guild_sku_id, self.user_sku_id]
            .into_iter()
            .flatten()
            .map(serenity::model::id::SkuId::new)
            .collect::<Vec<_>>();
        let mut after = None;
        let mut entitlements = Vec::new();
        for _ in 0..MAX_PAGES {
            let page = context
                .http
                .get_entitlements(
                    None,
                    Some(sku_ids.clone()),
                    None,
                    after,
                    Some(PAGE_SIZE),
                    None,
                    None,
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            let page_len = page.len();
            after = page.last().map(|entitlement| entitlement.id);
            entitlements.extend(page);
            if page_len < PAGE_SIZE as usize {
                break;
            }
        }
        if entitlements.len() >= PAGE_SIZE as usize * MAX_PAGES {
            // Hitting the guard means the endpoint never produced a terminating page. Do not
            // replace the projection with an incomplete list.
            return Err(GatewayEventDispatchError);
        }

        let now = now_ms();
        let grants = entitlements
            .into_iter()
            .filter_map(|entitlement| {
                map_entitlement_grant(
                    entitlement.sku_id.get(),
                    entitlement
                        .guild_id
                        .map(|id| id.get().to_string())
                        .as_deref(),
                    entitlement
                        .user_id
                        .map(|id| id.get().to_string())
                        .as_deref(),
                    entitlement.deleted,
                    entitlement
                        .ends_at
                        .map(|timestamp| timestamp.unix_timestamp()),
                    now,
                    EntitlementSkuIds {
                        guild: self.guild_sku_id,
                        user: self.user_sku_id,
                    },
                )
            })
            .collect::<Vec<_>>();
        let result = self
            .store
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .sync_discord_entitlements(&grants)
            .map_err(|_| GatewayEventDispatchError)?;
        eprintln!(
            "[premium] Discord entitlements synchronized: {} guild(s), {} user(s), {} revoked.",
            result.guilds_active, result.users_active, result.revoked
        );
        Ok(())
    }

    fn activate_response(
        &self,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<CreateInteractionResponseMessage, GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id else {
            return Ok(CreateInteractionResponseMessage::new()
                .content(self.message("premium.needManageGuild", command, &BTreeMap::new())?)
                .ephemeral(true));
        };
        if !self.command_permission(command) {
            return Ok(CreateInteractionResponseMessage::new()
                .content(self.message("premium.needManageGuild", command, &BTreeMap::new())?)
                .ephemeral(true));
        }
        let user_id = command.user.id.get().to_string();
        let guild_id = guild_id.get().to_string();
        let now = now_ms();
        let (content, row) = {
            let store = self.store.lock().map_err(|_| GatewayEventDispatchError)?;
            let Some(pass) = store
                .premium_pass_at(&user_id, now)
                .map_err(|_| GatewayEventDispatchError)?
            else {
                if !self.payments_enabled {
                    return Ok(CreateInteractionResponseMessage::new()
                        .content("Payments are currently disabled. Stripe checkout is coming soon.")
                        .ephemeral(true));
                }
                let mut p = BTreeMap::new();
                p.insert("link", self.kofi_url.clone());
                return Ok(CreateInteractionResponseMessage::new()
                    .content(self.message("premium.noPass", command, &p)?)
                    .ephemeral(true));
            };
            if pass.expires_at <= now {
                if !self.payments_enabled {
                    return Ok(CreateInteractionResponseMessage::new()
                        .content("Payments are currently disabled. Stripe checkout is coming soon.")
                        .ephemeral(true));
                }
                let mut p = BTreeMap::new();
                p.insert("link", self.kofi_url.clone());
                return Ok(CreateInteractionResponseMessage::new()
                    .content(self.message("premium.noPass", command, &p)?)
                    .ephemeral(true));
            }
            let activations = store
                .pass_activations(&user_id)
                .map_err(|_| GatewayEventDispatchError)?;
            if activation_within_limit(&activations, pass.seats, &guild_id) {
                return Ok(CreateInteractionResponseMessage::new()
                    .content(self.message("premium.alreadyActive", command, &BTreeMap::new())?)
                    .ephemeral(true));
            }
            if activations.len() as i64 >= pass.seats {
                let mut p = BTreeMap::new();
                p.insert("total", pass.seats.to_string());
                p.insert("servers", activations.join(", "));
                return Ok(CreateInteractionResponseMessage::new()
                    .content(self.message("premium.noSeats", command, &p)?)
                    .ephemeral(true));
            }
            let issued_at = now_seconds();
            let mut p = BTreeMap::new();
            p.insert("total", pass.seats.to_string());
            p.insert("used", activations.len().to_string());
            let content = self.message("premium.confirmActivate", command, &p)?;
            let yes = self.message("premium.confirmYes", command, &BTreeMap::new())?;
            let no = self.message("premium.confirmNo", command, &BTreeMap::new())?;
            let row = CreateActionRow::Buttons(vec![
                CreateButton::new(Self::activation_id("yes", &user_id, &guild_id, issued_at))
                    .label(yes)
                    .style(serenity::model::application::ButtonStyle::Success),
                CreateButton::new(Self::activation_id("no", &user_id, &guild_id, issued_at))
                    .label(no)
                    .style(serenity::model::application::ButtonStyle::Secondary),
            ]);
            (content, row)
        };
        Ok(CreateInteractionResponseMessage::new()
            .content(content)
            .components(vec![row])
            .ephemeral(true))
    }

    async fn activate(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        let response = self.activate_response(command)?;
        command
            .create_response(context, CreateInteractionResponse::Message(response))
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    async fn deactivate(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id else {
            let content = self.message("premium.needManageGuild", command, &BTreeMap::new())?;
            command
                .create_response(
                    context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(content)
                            .ephemeral(true),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        };
        if !self.command_permission(command) {
            let content = self.message("premium.needManageGuild", command, &BTreeMap::new())?;
            command
                .create_response(
                    context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(content)
                            .ephemeral(true),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let removed = self
            .store
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .deactivate_seat(
                &command.user.id.get().to_string(),
                &guild_id.get().to_string(),
            )
            .map_err(|_| GatewayEventDispatchError)?;
        let content = self.message(
            if removed {
                "premium.deactivateOk"
            } else {
                "premium.deactivateNone"
            },
            command,
            &BTreeMap::new(),
        )?;
        command
            .create_response(
                context,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(content)
                        .ephemeral(true),
                ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }
}

#[derive(Default)]
struct EntitlementSyncState {
    running: bool,
    queued: bool,
}

#[derive(Clone, Copy)]
struct EntitlementSkuIds {
    guild: Option<u64>,
    user: Option<u64>,
}

fn map_entitlement_grant(
    sku_id: u64,
    guild_id: Option<&str>,
    user_id: Option<&str>,
    deleted: bool,
    ends_at_seconds: Option<i64>,
    now_ms: i64,
    skus: EntitlementSkuIds,
) -> Option<EntitlementGrant> {
    if deleted {
        return None;
    }
    let expires_at = ends_at_seconds
        .map(|seconds| seconds.saturating_mul(1_000))
        .unwrap_or_else(|| now_ms.saturating_add(100 * 365 * 24 * 60 * 60 * 1_000));
    if expires_at <= now_ms {
        return None;
    }
    if skus.guild == Some(sku_id) {
        return guild_id.map(|id| EntitlementGrant {
            kind: PremiumKind::Guild,
            id: id.to_owned(),
            expires_at,
        });
    }
    if skus.user == Some(sku_id) {
        return user_id.map(|id| EntitlementGrant {
            kind: PremiumKind::User,
            id: id.to_owned(),
            expires_at,
        });
    }
    None
}

#[async_trait::async_trait]
impl GatewayEventSink for PremiumGatewaySink {
    async fn on_ready(&self, context: Context) -> Result<(), GatewayEventDispatchError> {
        self.spawn_entitlement_sync(context);
        Ok(())
    }

    async fn on_entitlement_change(
        &self,
        context: Context,
    ) -> Result<(), GatewayEventDispatchError> {
        self.spawn_entitlement_sync(context);
        Ok(())
    }

    async fn on_message(
        &self,
        _context: Context,
        _message: serenity::model::channel::Message,
    ) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    async fn on_interaction(
        &self,
        context: Context,
        interaction: Interaction,
    ) -> Result<(), GatewayEventDispatchError> {
        match interaction {
            Interaction::Command(command) => {
                let Some(parsed) =
                    parse_premium_command(&command.data).map_err(|_| GatewayEventDispatchError)?
                else {
                    return Ok(());
                };
                match parsed {
                    PremiumCommand::Info => {
                        let (content, _active) = self.response(&command)?;
                        let mut buttons = Vec::new();
                        if self.payments_enabled
                            && let Some(sku) = self.guild_sku_id
                            && command.guild_id.is_some()
                        {
                            buttons.push(CreateButton::new_premium(sku));
                        }
                        if self.payments_enabled
                            && let Some(sku) = self.user_sku_id
                        {
                            buttons.push(CreateButton::new_premium(sku));
                        }
                        let mut response = CreateInteractionResponseMessage::new()
                            .content(content)
                            .ephemeral(true);
                        if !buttons.is_empty() {
                            response = response.components(vec![CreateActionRow::Buttons(buttons)]);
                        }
                        command
                            .create_response(&context, CreateInteractionResponse::Message(response))
                            .await
                            .map_err(|_| GatewayEventDispatchError)?;
                    }
                    PremiumCommand::Activate => self.activate(&context, &command).await?,
                    PremiumCommand::Deactivate => self.deactivate(&context, &command).await?,
                }
            }
            Interaction::Component(component) => {
                let Some((action, user, guild, issued)) =
                    Self::parse_activation_id(&component.data.custom_id)
                else {
                    return Ok(());
                };
                let now = now_seconds();
                if user != component.user.id.get().to_string()
                    || component
                        .guild_id
                        .is_none_or(|id| id.get().to_string() != guild)
                    || issued > now
                    || now.saturating_sub(issued) > ACTIVATION_TTL_SECONDS
                {
                    let content = self.component_message(
                        "premium.activateCancelled",
                        &component,
                        &BTreeMap::new(),
                    )?;
                    component
                        .create_response(
                            &context,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .content(content)
                                    .ephemeral(true),
                            ),
                        )
                        .await
                        .map_err(|_| GatewayEventDispatchError)?;
                    return Ok(());
                }
                let content = if action == "no" {
                    self.component_message(
                        "premium.activateCancelled",
                        &component,
                        &BTreeMap::new(),
                    )?
                } else {
                    let result = self
                        .store
                        .lock()
                        .map_err(|_| GatewayEventDispatchError)?
                        .activate_seat(user, guild, now_ms())
                        .map_err(|_| GatewayEventDispatchError)?;
                    match result.status {
                        ActivateStatus::Ok => {
                            let mut p = BTreeMap::new();
                            p.insert("date", discord_date(result.expires_at.unwrap_or(0)));
                            p.insert("used", result.used.unwrap_or_default().to_string());
                            p.insert("total", result.seats.unwrap_or_default().to_string());
                            self.component_message("premium.activateOk", &component, &p)?
                        }
                        ActivateStatus::Already => self.component_message(
                            "premium.alreadyActive",
                            &component,
                            &BTreeMap::new(),
                        )?,
                        ActivateStatus::NoSeats => {
                            let mut p = BTreeMap::new();
                            p.insert("total", result.seats.unwrap_or_default().to_string());
                            p.insert(
                                "servers",
                                self.store
                                    .lock()
                                    .map_err(|_| GatewayEventDispatchError)?
                                    .pass_activations(user)
                                    .map_err(|_| GatewayEventDispatchError)?
                                    .join(", "),
                            );
                            self.component_message("premium.noSeats", &component, &p)?
                        }
                        ActivateStatus::NoPass | ActivateStatus::Expired => {
                            if !self.payments_enabled {
                                "Payments are currently disabled. Stripe checkout is coming soon."
                                    .into()
                            } else {
                                let mut p = BTreeMap::new();
                                p.insert("link", self.kofi_url.clone());
                                self.component_message("premium.noPass", &component, &p)?
                            }
                        }
                    }
                };
                component
                    .create_response(
                        &context,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .content(content)
                                .components(Vec::new())
                                .ephemeral(true),
                        ),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_guild_delete(&self, _guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }
}

fn discord_date(ms: i64) -> String {
    format!("<t:{}:D>", ms.div_euclid(1_000))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(i64::MAX as u128) as i64
        })
}

fn now_seconds() -> i64 {
    now_ms().div_euclid(1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entitlement_mapping_preserves_paid_targets_and_expiry_rules() {
        assert_eq!(
            map_entitlement_grant(
                10,
                Some("guild"),
                None,
                false,
                Some(2_000),
                1_000_000,
                EntitlementSkuIds {
                    guild: Some(10),
                    user: Some(20),
                },
            ),
            Some(EntitlementGrant {
                kind: PremiumKind::Guild,
                id: "guild".into(),
                expires_at: 2_000_000,
            })
        );
        assert_eq!(
            map_entitlement_grant(
                20,
                None,
                Some("user"),
                false,
                None,
                1_000_000,
                EntitlementSkuIds {
                    guild: Some(10),
                    user: Some(20),
                },
            )
            .map(|grant| (grant.kind, grant.id)),
            Some((PremiumKind::User, "user".into()))
        );
    }

    #[test]
    fn saved_associations_outside_the_downgraded_limit_are_not_reported_active() {
        let activations = vec!["oldest".into(), "second".into(), "third".into()];
        assert!(activation_within_limit(&activations, 2, "oldest"));
        assert!(activation_within_limit(&activations, 2, "second"));
        assert!(!activation_within_limit(&activations, 2, "third"));
        assert!(!activation_within_limit(&activations, 0, "oldest"));
        assert!(!activation_within_limit(&activations, -1, "oldest"));
    }

    #[test]
    fn entitlement_mapping_ignores_unknown_expired_deleted_and_missing_targets() {
        for input in [
            (99, Some("guild"), None, false, Some(2_000_000)),
            (10, Some("guild"), None, false, Some(999)),
            (10, Some("guild"), None, true, Some(2_000_000)),
            (10, None, None, false, Some(2_000_000)),
        ] {
            assert!(
                map_entitlement_grant(
                    input.0,
                    input.1,
                    input.2,
                    input.3,
                    input.4,
                    1_000_000,
                    EntitlementSkuIds {
                        guild: Some(10),
                        user: Some(20),
                    },
                )
                .is_none()
            );
        }
    }
}

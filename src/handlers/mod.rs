use std::{
    backtrace::Backtrace,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use deadpool_diesel::sqlite::Pool;
use twilight_gateway::Event;
use twilight_http::Client;
use twilight_model::{
    channel::{
        message::{MessageReference, MessageType},
        Channel, ChannelType, Message, Webhook,
    },
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, RoleMarker},
        Id,
    },
};

use crate::{
    actions::{self, mapping, DatabaseError},
    config::{Blacklist, GuildSettings},
    model::{IdMapping, MappingData},
    util::{
        self, build_channel_map, create_channels, register_channels, reposition_channels,
        MappingsError,
    },
};

#[derive(thiserror::Error, Debug)]
pub enum HandlerError {
    #[error("Client request failed. {0}")]
    ClientSend(#[from] twilight_http::error::Error, Backtrace),
    #[error("Client deserializing server response failed. {0}")]
    ClientSerde(
        #[from] twilight_http::response::DeserializeBodyError,
        Backtrace,
    ),
    #[error("Client request validation failed. {0}")]
    ClientValidation(
        #[from] twilight_validate::request::ValidationError,
        Backtrace,
    ),
    #[error("Could not validate channel when creating. {0}")]
    ClientChannelValidation(
        #[from] twilight_validate::channel::ChannelValidationError,
        Backtrace,
    ),
    #[error("Could not validate message when creating. {0}")]
    ClientMessageValidation(
        #[from] twilight_validate::message::MessageValidationError,
        Backtrace,
    ),
    #[error("Reqwest error. {0}")]
    ReqwestError(#[from] reqwest::Error, Backtrace),
    #[error("Couldn't fetch database pool. {0}")]
    Pool(#[from] deadpool_diesel::PoolError, Backtrace),
    #[error("Pool interaction failed. {0}")]
    Interact(#[from] deadpool_diesel::InteractError, Backtrace),
    #[error("Database action failed. {0}")]
    Database(#[from] actions::DatabaseError, Backtrace),
    #[error("An error occurred while mapping channels. {0}")]
    Mapping(#[from] MappingsError, Backtrace),
}

impl HandlerError {
    pub fn backtrace(&self) -> &Backtrace {
        match self {
            HandlerError::ClientSend(_, b) => b,
            HandlerError::ClientSerde(_, b) => b,
            HandlerError::ClientValidation(_, b) => b,
            HandlerError::ClientChannelValidation(_, b) => b,
            HandlerError::ClientMessageValidation(_, b) => b,
            HandlerError::ReqwestError(_, b) => b,
            HandlerError::Pool(_, b) => b,
            HandlerError::Interact(_, b) => b,
            HandlerError::Database(_, b) => b,
            HandlerError::Mapping(_, b) => b,
        }
    }
}

type Result<T> = std::result::Result<T, HandlerError>;

async fn get_or_create_webhook(
    client: &Client,
    channel_id: Id<ChannelMarker>,
    default_webhook_username: &str,
) -> Result<Webhook> {
    use twilight_http::error::ErrorType;

    let webhook = client.channel_webhooks(channel_id).await;

    let webhook = match webhook {
        Err(e) => match e.kind() {
            ErrorType::Response {
                body: _,
                error: _,
                status,
            } if *status == 404 => {
                let channel = client.channel(channel_id).await?.model().await?;
                let parent_channel_id = channel
                    .parent_id
                    .expect("expected thread to have parent_id");

                Ok(client
                    .channel_webhooks(parent_channel_id)
                    .await?
                    .model()
                    .await?
                    .first()
                    .cloned())
            }
            _ => Err(e),
        },
        Ok(v) => Ok(v.model().await?.first().cloned()),
    }?;

    if let Some(webhook) = webhook {
        Ok(webhook)
    } else {
        Ok(client
            .create_webhook(channel_id, default_webhook_username)?
            .await?
            .model()
            .await?)
    }
}

/// Tries to get mapping and returns the destination thread/channel.
/// If it fails and the source channel type is AnnouncementThread or
/// PublicThread it creates a thread and returns that.
async fn get_or_create_thread(
    src_guild_id: Id<GuildMarker>,
    dest_guild_id: Id<GuildMarker>,
    src_channel_id: Id<ChannelMarker>,
    starter_message: Message,
    settings: &GuildSettings,
    client: &Client,
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
) -> Result<(Option<Channel>, bool)> {
    if let Some(mapping) = object
        .interact(move |conn| mapping::get_mapping(conn, Some(src_channel_id.get()), None))
        .await??
    {
        match mapping.mapping_data().1 {
            Some(MappingData::Channel(v)) => Ok((Some(v), false)),
            _ => Ok((None, false)),
        }
    } else {
        // otherwise, we assume it's a thread and try to create one
        let src_thread = client.channel(src_channel_id).await?.model().await?;

        if !src_thread.kind.is_thread() {
            return Ok((None, false));
        }

        let Some(src_parent_id) = src_thread.parent_id else {
            return Ok((None, false));
        };

        let Some((_, Some(MappingData::Channel(dest_parent)))) = object
            .interact(move |conn| mapping::get_mapping(conn, Some(src_parent_id.get()), None))
            .await??
            .map(|x| x.mapping_data())
        else {
            return Ok((None, false));
        };

        let starter_message_data = {
            let (starter_message_content, attachments) =
                format_message(object, dest_guild_id, starter_message.clone(), settings).await?;

            Some((starter_message, starter_message_content, attachments))
        };

        if let Some(ref name) = src_thread.name {
            let Some(thread) = create_thread(
                &src_thread,
                &dest_parent,
                client,
                starter_message_data,
                name,
            )
            .await?
            else {
                return Ok((None, false));
            };

            let thread_clone = thread.clone();

            object
                .interact(move |conn| -> std::result::Result<(), DatabaseError> {
                    mapping::register_mapping(
                        conn,
                        IdMapping::new(
                            &src_thread.id.get(),
                            &thread.id.get(),
                            &src_guild_id,
                            &dest_guild_id,
                        )
                        .set_src_mapping_data(MappingData::Channel(src_thread))?
                        .set_dest_mapping_data(MappingData::Channel(thread_clone))?,
                    )?;
                    Ok(())
                })
                .await??;

            Ok((Some(thread), dest_parent.kind == ChannelType::GuildForum))
        } else {
            Ok((None, false))
        }
    }
}

async fn handle_single_event(
    event: &Event,
    client: &Client,
    pool: &Pool,
    settings: &GuildSettings,
    sleep_duration: Duration,
) -> Result<()> {
    let object = &pool.get().await?;
    let (src_guild_id, dest_guild_id) = (settings.src_guild_id, settings.dest_guild_id);

    match event.clone() {
        Event::ChannelCreate(v) => {
            let dest_guild = client.guild(dest_guild_id).await?.model().await?;

            let src_channels = client.guild_channels(src_guild_id).await?.model().await?;

            let dest_channels =
                create_channels(vec![(**v).clone()], client, &dest_guild, sleep_duration).await?;

            register_channels(dest_channels.clone(), (src_guild_id, dest_guild_id), object).await?;

            let dest_channels = dest_channels.into_values().collect::<Vec<_>>();

            let dest_channels = client
                .guild_channels(dest_guild_id)
                .await?
                .model()
                .await?
                .into_iter()
                .filter(|x| dest_channels.contains(x) || x.kind == ChannelType::GuildCategory)
                .collect::<Vec<Channel>>();

            let channel_map = &build_channel_map(src_channels, dest_channels, object).await?;

            reposition_channels(dest_guild_id, channel_map, client, sleep_duration).await?;

            Ok(())
        }
        Event::ChannelUpdate(v) => {
            if !settings.relay_updates {
                return Ok(());
            }

            let src_channel = (**v).clone();
            let src_channel_id = v.id;
            let src_channel_parent_id = v.parent_id;
            let dest_channel_id = object
                .interact(move |conn| mapping::get_mapping(conn, Some(src_channel_id.get()), None))
                .await??
                .map(|x| x.ref_id::<ChannelMarker>());

            if let Some(dest_channel_id) = dest_channel_id {
                let parent_id = object
                    .interact(move |conn| match src_channel_parent_id {
                        Some(v) => mapping::get_mapping(conn, Some(v.get()), None),
                        None => Ok(None),
                    })
                    .await??;

                let mut update_channel = client.update_channel(dest_channel_id);

                if let Some(ref v) = src_channel.name {
                    update_channel = update_channel.name(v)?;
                }

                if let Some(v) = src_channel.nsfw {
                    update_channel = update_channel.nsfw(v);
                }

                if let Some(v) = &src_channel.topic {
                    update_channel = update_channel.topic(v)?;
                }

                if let Some(v) = &src_channel.permission_overwrites {
                    update_channel = update_channel.permission_overwrites(v);
                }

                if let Some(v) = src_channel.position {
                    update_channel = update_channel.position(v as u64);
                }

                update_channel = update_channel.kind(src_channel.kind);
                update_channel =
                    update_channel.parent_id(parent_id.map(|x| x.ref_id::<ChannelMarker>()));

                let dest_channel = update_channel.await?.model().await?;

                object
                    .interact(move |conn| -> std::result::Result<(), DatabaseError> {
                        let Some(mapping) =
                            mapping::get_mapping(conn, Some(src_channel_id.get()), None)?
                        else {
                            return Ok(());
                        };

                        mapping::update_mapping(
                            conn,
                            mapping.clone(),
                            mapping
                                .set_src_mapping_data(MappingData::Channel(src_channel))?
                                .set_dest_mapping_data(MappingData::Channel(dest_channel))?,
                        )?;
                        Ok(())
                    })
                    .await??;

                tracing::info!(
                    "Updated channel <#{}> -> <#{}>",
                    src_channel_id,
                    dest_channel_id
                );
            }

            Ok(())
        }
        Event::ChannelDelete(v) => {
            // unregister
            let mapping = object
                .interact(
                    move |conn| -> std::result::Result<Option<IdMapping>, DatabaseError> {
                        let Some(mapping) = mapping::get_mapping(conn, Some(v.id.get()), None)?
                        else {
                            return Ok(None);
                        };

                        mapping::unregister_mapping(conn, mapping.clone())?;
                        tracing::info!(
                            "Unregistered channel <#{}> -> <#{}>",
                            mapping.orig_id::<ChannelMarker>(),
                            mapping.orig_id::<ChannelMarker>()
                        );

                        Ok(Some(mapping))
                    },
                )
                .await??;

            if !settings.relay_deletes {
                return Ok(());
            }

            // delete
            if let Some(mapping) = mapping {
                client
                    .delete_channel(mapping.ref_id::<ChannelMarker>())
                    .await?;

                tracing::info!(
                    "Deleted channel <#{}> -> <#{}>",
                    mapping.orig_id::<ChannelMarker>(),
                    mapping.ref_id::<ChannelMarker>()
                );
            }

            Ok(())
        }
        Event::MessageCreate(v) => {
            let message = (**v).clone();
            let (min_attachments_amount, min_content_length, min_embeds_amount) = (
                settings.message_requirements.min_attachments_amount as usize,
                settings.message_requirements.min_content_length as usize,
                settings.message_requirements.min_embeds_amount as usize,
            );

            if settings
                .blacklists
                .iter()
                .filter(|x| match x {
                    Blacklist::User(x) => x == &message.author.id,
                    Blacklist::Role(x) => {
                        if let Some(member) = &message.member {
                            member.roles.contains(x)
                        } else {
                            false
                        }
                    }
                    _ => false,
                })
                .peekable()
                .peek()
                .is_some()
            {
                return Ok(());
            }

            if message.attachments.len() < min_attachments_amount
                || message.content.len() < min_content_length
                || message.embeds.len() < min_embeds_amount
            {
                return Ok(());
            }

            if message.reference.is_some() && !settings.message_settings.mirror_reply_messages {
                return Ok(());
            }

            if message.author.bot && !settings.message_settings.mirror_messages_from_bots {
                return Ok(());
            }

            if message.kind != MessageType::Regular
                && message.kind != MessageType::ThreadStarterMessage
                && message.kind != MessageType::ThreadCreated
            {
                return Ok(());
            }

            if message.kind == MessageType::ThreadCreated {
                client.join_thread(v.id.cast::<ChannelMarker>()).await?;
                return Ok(());
            }

            let (Some(dest_channel), is_parent_forum) = get_or_create_thread(
                src_guild_id,
                dest_guild_id,
                message.channel_id,
                message.clone(),
                settings,
                client,
                object,
            )
            .await?
            else {
                return Ok(());
            };

            let dest_channel_id = if dest_channel.kind.is_thread() {
                dest_channel.parent_id
            } else {
                Some(dest_channel.id)
            };

            let Some(dest_channel_id) = dest_channel_id else {
                return Ok(());
            };

            let (new_content, new_attachments) =
                format_message(object, dest_guild_id, message.clone(), settings).await?;

            let webhook =
                get_or_create_webhook(client, dest_channel_id, &settings.webhook_username).await?;

            let avatar_url = message.author.avatar.map(|avatar| {
                format!(
                    "https://cdn.discordapp.com/avatars/{}/{}.png",
                    message.author.id, avatar
                )
            });

            if let Some(token) = webhook.token {
                let mut exec_hook = client.execute_webhook(webhook.id, &token);

                if dest_channel.kind.is_thread() {
                    exec_hook = exec_hook.thread_id(dest_channel.id);
                }

                if !settings.message_settings.use_webhook_profile {
                    exec_hook = exec_hook.username(&message.author.name)?;
                    if let Some(ref avatar_url) = avatar_url {
                        exec_hook = exec_hook.avatar_url(avatar_url);
                    }
                }

                if !new_attachments.is_empty() {
                    exec_hook = exec_hook.attachments(&new_attachments)?;
                }

                let dest_message = exec_hook
                    .embeds(&message.embeds)?
                    .content(&new_content)?
                    .wait()
                    .await?
                    .model()
                    .await?;

                // TODO: think of another way of checking whether the
                //       message comes from a GuildAnnouncement or not
                if dest_channel.kind == ChannelType::GuildAnnouncement
                    || dest_channel.kind == ChannelType::AnnouncementThread
                {
                    client
                        .crosspost_message(dest_channel_id, dest_message.id)
                        .await?;
                }

                tracing::info!(
                    "Created and registered message {} -> {}",
                    message.id,
                    dest_message.id
                );

                if is_parent_forum || v.id.cast::<ChannelMarker>() == v.channel_id {
                    return Ok(());
                }

                object
                    .interact(move |conn| {
                        mapping::register_mapping(
                            conn,
                            IdMapping::new(
                                &message.id.get(),
                                &dest_message.id.get(),
                                &src_guild_id,
                                &dest_guild_id,
                            ),
                        )
                    })
                    .await??;
            }

            Ok(())
        }
        Event::MessageUpdate(v) => {
            let message = (*v).clone();

            let Some(dest_channel_id) = object
                .interact(
                    move |conn| -> std::result::Result<Option<Id<ChannelMarker>>, DatabaseError> {
                        Ok(
                            mapping::get_mapping(conn, Some(message.channel_id.get()), None)?
                                .map(|x| x.ref_id::<ChannelMarker>()),
                        )
                    },
                )
                .await??
            else {
                return Ok(());
            };

            let dest_channel = client.channel(dest_channel_id).await?.model().await?;
            tokio::time::sleep(sleep_duration).await;

            let webhook =
                get_or_create_webhook(client, dest_channel_id, &settings.webhook_username).await?;

            let Some(new_content) = object
                .interact(
                    move |conn| -> std::result::Result<Option<String>, DatabaseError> {
                        let channel_regex =
                            regex::Regex::new("<#([0-9]+)>").expect("Invalid channel regex");

                        let Some(mut content) = message.content else {
                            return Ok(None);
                        };

                        let Some(mention_roles) = message.mention_roles else {
                            return Ok(None);
                        };

                        for (_, [src_channel_id]) in channel_regex
                            .captures_iter(&content.clone())
                            .map(|c| c.extract())
                        {
                            let Ok(src_channel_id) = src_channel_id.parse::<u64>() else {
                                continue;
                            };

                            let Some(dest_channel_id) =
                                mapping::get_mapping(conn, Some(src_channel_id), None)?
                            else {
                                continue;
                            };

                            let dest_channel_id = dest_channel_id.ref_id::<ChannelMarker>();
                            content = content.replace(
                                &format!("<#{}>", src_channel_id),
                                &format!("<#{}>", dest_channel_id),
                            );
                        }

                        for src_role_id in mention_roles {
                            let Some(dest_role_id) =
                                mapping::get_mapping(conn, Some(src_role_id.get()), None)?
                            else {
                                continue;
                            };
                            let dest_role_id = dest_role_id.ref_id::<RoleMarker>();
                            content = content.replace(
                                &format!("<@&{}>", src_role_id),
                                &format!("<@&{}>", dest_role_id),
                            );
                        }

                        Ok(Some(content))
                    },
                )
                .await??
            else {
                return Ok(());
            };

            let avatar_url = if let Some(ref author) = message.author {
                author.avatar.map(|avatar| {
                    format!(
                        "https://cdn.discordapp.com/avatars/{}/{}.png",
                        author.id, avatar
                    )
                })
            } else {
                None
            };

            if let Some(token) = webhook.token {
                let mut exec_hook = client.execute_webhook(webhook.id, &token);

                if !settings.message_settings.use_webhook_profile {
                    if let Some(ref author) = message.author {
                        exec_hook = exec_hook.username(&author.name)?;
                    }

                    if let Some(ref avatar_url) = avatar_url {
                        exec_hook = exec_hook.avatar_url(avatar_url);
                    }
                }

                let new_content = if let (guild_id, Some(channel_id), Some(id)) = (
                    dest_guild_id,
                    object
                        .interact(move |conn| {
                            mapping::get_mapping(conn, Some(message.channel_id.get()), None)
                        })
                        .await??,
                    object
                        .interact(move |conn| {
                            mapping::get_mapping(conn, Some(message.id.get()), None)
                        })
                        .await??,
                ) {
                    let channel_id = channel_id.ref_id::<ChannelMarker>();
                    let id = id.ref_id::<MessageMarker>();

                    format!(
                        "[EDITED]\n{}\n\n{}",
                        format!(
                            "https://discord.com/channels/{}/{}/{}",
                            guild_id, channel_id, id
                        ),
                        new_content
                    )
                } else {
                    format!("[EDITED]\n\n{}", new_content)
                };

                if let Some(ref v) = message.embeds {
                    exec_hook = exec_hook.embeds(v)?;
                }

                let dest_message = exec_hook
                    .content(&new_content)?
                    .wait()
                    .await?
                    .model()
                    .await?;

                // TODO: think of another way of checking whether the
                //       message comes from a GuildAnnouncement or not
                if dest_channel.kind == ChannelType::GuildAnnouncement {
                    client
                        .crosspost_message(dest_channel_id, dest_message.id)
                        .await?;
                }

                tracing::info!("Relayed message update {}", message.id,);
            }

            Ok(())
        }
        Event::RoleCreate(v) => {
            if !settings.clone_roles {
                return Ok(());
            }

            let new_role =
                util::create_roles(vec![v.role], client, dest_guild_id, sleep_duration).await?;
            util::register_roles(new_role.clone(), (src_guild_id, dest_guild_id), object).await?;

            Ok(())
        }
        Event::RoleUpdate(v) => {
            if !settings.relay_updates {
                return Ok(());
            }

            let src_role = v.role;
            let src_role_id = src_role.id;
            let dest_role_id = object
                .interact(move |conn| mapping::get_mapping(conn, Some(src_role_id.get()), None))
                .await??
                .map(|x| x.ref_id::<RoleMarker>());

            if let Some(dest_role_id) = dest_role_id {
                let mut update_role = client.update_role(dest_guild_id, dest_role_id);

                update_role = update_role.color(Some(src_role.color));
                update_role = update_role.hoist(src_role.hoist);
                update_role = update_role.mentionable(src_role.mentionable);
                update_role = update_role.name(Some(&src_role.name));
                update_role = update_role.permissions(src_role.permissions);

                let dest_role = update_role.await?.model().await?;

                tracing::info!("Updated role <@&{}> -> <@&{}>", src_role.id, dest_role.id);

                object
                    .interact(move |conn| -> std::result::Result<(), DatabaseError> {
                        let Some(mapping) =
                            mapping::get_mapping(conn, Some(src_role_id.get()), None)?
                        else {
                            return Ok(());
                        };

                        mapping::update_mapping(
                            conn,
                            mapping.clone(),
                            mapping
                                .set_src_mapping_data(MappingData::Role(src_role))?
                                .set_dest_mapping_data(MappingData::Role(dest_role))?,
                        )?;
                        Ok(())
                    })
                    .await??;
            }

            Ok(())
        }
        Event::RoleDelete(v) => {
            // unregister
            let mapping = object
                .interact(
                    move |conn| -> std::result::Result<Option<IdMapping>, DatabaseError> {
                        let Some(mapping) =
                            mapping::get_mapping(conn, Some(v.role_id.get()), None)?
                        else {
                            return Ok(None);
                        };

                        mapping::unregister_mapping(conn, mapping.clone())?;
                        tracing::info!(
                            "Unregistered role <@&{}> -> <@&{}>",
                            mapping.orig_id::<RoleMarker>(),
                            mapping.ref_id::<RoleMarker>()
                        );

                        Ok(Some(mapping))
                    },
                )
                .await??;

            if !settings.relay_deletes {
                return Ok(());
            }

            // delete
            if let Some(mapping) = mapping {
                client
                    .delete_role(dest_guild_id, mapping.ref_id::<RoleMarker>())
                    .await?;

                tracing::info!(
                    "Deleted role <@&{}> -> <@&{}>",
                    mapping.orig_id::<RoleMarker>(),
                    mapping.ref_id::<RoleMarker>()
                );
            }

            Ok(())
        }
        Event::ThreadCreate(v) => {
            let src_thread = (**v).clone();

            let Some(parent_id) = v.parent_id else {
                return Ok(());
            };

            if settings.blacklists.contains(&Blacklist::Channel(parent_id)) {
                return Ok(());
            }

            if object
                .interact(
                    move |conn| -> std::result::Result<Option<IdMapping>, DatabaseError> {
                        mapping::get_mapping(conn, Some(src_thread.id.get()), None)
                    },
                )
                .await??
                .is_some()
            {
                return Ok(());
            };

            client.join_thread(src_thread.id).await?;

            let Some((_, dest_parent_id)) = object
                .interact(move |conn| mapping::get_mapping(conn, Some(parent_id.get()), None))
                .await??
                .map(|x| (x.orig_id::<ChannelMarker>(), x.ref_id::<ChannelMarker>()))
            else {
                return Ok(());
            };

            let dest_parent = client.channel(dest_parent_id).await?.model().await?;

            let Some(ref thread_name) = v.name else {
                return Ok(());
            };

            let Some(dest_thread) =
                create_thread(&src_thread, &dest_parent, client, None, thread_name).await?
            else {
                return Ok(());
            };

            tracing::info!(
                "Created and registered thread <#{}> -> <#{}>",
                src_thread.id,
                dest_thread.id
            );

            object
                .interact(move |conn| -> std::result::Result<usize, DatabaseError> {
                    mapping::register_mapping(
                        conn,
                        IdMapping::new(
                            &src_thread.id.get(),
                            &dest_thread.id.get(),
                            &src_guild_id,
                            &dest_guild_id,
                        )
                        .set_src_mapping_data(MappingData::Channel(src_thread))?
                        .set_dest_mapping_data(MappingData::Channel(dest_thread))?,
                    )
                })
                .await??;

            Ok(())
        }
        Event::ThreadUpdate(v) => {
            if let Some(ref metadata) = v.thread_metadata {
                if !metadata.archived {
                    client.join_thread(v.id).await?;
                }
            }

            if !settings.relay_updates {
                return Ok(());
            }

            let Some(parent_id) = v.parent_id else {
                return Ok(());
            };

            if settings.blacklists.contains(&Blacklist::Channel(parent_id)) {
                return Ok(());
            }

            let src_thread = &**v;
            let src_thread_id = src_thread.id;
            let dest_thread_id = object
                .interact(move |conn| mapping::get_mapping(conn, Some(src_thread_id.get()), None))
                .await??
                .map(|x| x.ref_id::<ChannelMarker>());
            let Some(dest_thread_id) = dest_thread_id else {
                return Ok(());
            };

            let mut update_thread = client.update_thread(dest_thread_id);

            if let Some(ref v) = v.name {
                update_thread = update_thread.name(v)?;
            };

            if let Some(v) = v.invitable {
                update_thread = update_thread.invitable(v);
            }

            if let Some(v) = v.default_auto_archive_duration {
                update_thread = update_thread.auto_archive_duration(v);
            }

            update_thread.await?;

            tracing::info!(
                "Updated thread <#{}> -> <#{}>",
                src_thread_id,
                dest_thread_id
            );

            Ok(())
        }
        Event::ThreadDelete(v) => {
            let mapping = object
                .interact(
                    move |conn| -> std::result::Result<Option<IdMapping>, DatabaseError> {
                        let Some(mapping) = mapping::get_mapping(conn, Some(v.id.get()), None)?
                        else {
                            return Ok(None);
                        };

                        mapping::unregister_mapping(conn, mapping.clone())?;
                        tracing::info!(
                            "Unregistered thread <#{}> -> <#{}>",
                            mapping.orig_id::<ChannelMarker>(),
                            mapping.ref_id::<ChannelMarker>()
                        );

                        Ok(Some(mapping))
                    },
                )
                .await??;

            if !settings.relay_deletes {
                return Ok(());
            }

            // delete
            if let Some(mapping) = mapping {
                client
                    .delete_channel(mapping.ref_id::<ChannelMarker>())
                    .await?;

                tracing::info!(
                    "Deleted thread <@&{}> -> <@&{}>",
                    mapping.orig_id::<ChannelMarker>(),
                    mapping.ref_id::<ChannelMarker>()
                );
            };

            Ok(())
        }
        _ => Ok(()),
    }
}

async fn create_thread(
    src_thread: &Channel,
    dest_parent: &Channel,
    client: &Client,
    starter_message_data: Option<(
        Message,
        String,
        Vec<twilight_model::http::attachment::Attachment>,
    )>,
    thread_name: &str,
) -> Result<Option<Channel>> {
    let dest_thread = if dest_parent.kind == ChannelType::GuildForum {
        let mut dest_thread = client.create_forum_thread(dest_parent.id, thread_name);

        if let Some(v) = src_thread.default_auto_archive_duration {
            dest_thread = dest_thread.auto_archive_duration(v);
        }

        let dest_thread = if let Some((starter_message, starter_message_content, attachments)) =
            &starter_message_data
        {
            dest_thread
                .message()
                .content(starter_message_content)?
                .embeds(&starter_message.embeds)?
                .attachments(attachments)?
        } else {
            dest_thread.message().content("_ _")?
        };

        
        dest_thread.await?.model().await?.channel
    } else {
        let mut dest_thread = client.create_thread(dest_parent.id, thread_name, src_thread.kind)?;
        
        if let Some(v) = src_thread.invitable {
            dest_thread = dest_thread.invitable(v);
        }

        if let Some(v) = src_thread.default_auto_archive_duration {
            dest_thread = dest_thread.auto_archive_duration(v);
        }
        
        
        dest_thread.await?.model().await?
    };

    if dest_parent.kind == ChannelType::GuildForum {
        tracing::info!("Created forum post <#{}> -> <#{}>", src_thread.id, dest_thread.id);
    } else {
        tracing::info!("Created text thread <#{}> -> <#{}>", src_thread.id, dest_thread.id);
    }

    Ok(Some(dest_thread))
}

async fn format_message(
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
    dest_guild_id: Id<GuildMarker>,
    message: twilight_model::channel::Message,
    settings: &GuildSettings,
) -> std::result::Result<(String, Vec<twilight_model::http::attachment::Attachment>), HandlerError>
{
    let mut new_content = object
        .interact(move |conn| -> std::result::Result<String, DatabaseError> {
            let channel_regex = regex::Regex::new("<#([0-9]+)>").expect("Invalid channel regex");

            let mut content = message.content.clone();

            for (_, [src_channel_id]) in channel_regex
                .captures_iter(&message.content)
                .map(|c| c.extract())
            {
                let Ok(src_channel_id) = src_channel_id.parse::<u64>() else {
                    continue;
                };

                let Some(dest_channel_id) = mapping::get_mapping(conn, Some(src_channel_id), None)?
                else {
                    continue;
                };

                let dest_channel_id = dest_channel_id.ref_id::<ChannelMarker>();
                content = content.replace(
                    &format!("<#{}>", src_channel_id),
                    &format!("<#{}>", dest_channel_id),
                );
            }

            for src_role_id in message.mention_roles {
                let Some(dest_role_id) = mapping::get_mapping(conn, Some(src_role_id.get()), None)?
                else {
                    continue;
                };

                let dest_role_id = dest_role_id.ref_id::<RoleMarker>();
                content = content.replace(
                    &format!("<@&{}>", src_role_id),
                    &format!("<@&{}>", dest_role_id),
                );
            }

            Ok(content)
        })
        .await??;

    let reference_url = object
        .interact(move |conn| reference_to_url(&message.reference, dest_guild_id, conn))
        .await??;
    if let Some(reference_url) = reference_url {
        new_content = format!("[REPLY]\n{}\n\n{}", reference_url, new_content);
    }
    let mut new_attachments = vec![];
    if settings.message_settings.remove_attachments {
        for attachment in message.attachments {
            let start = SystemTime::now();
            let since_the_epoch = start
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards");

            let in_ms = since_the_epoch.as_secs() * 1000
                + since_the_epoch.subsec_nanos() as u64 / 1_000_000;

            let bytes = reqwest::get(attachment.url)
                .await?
                .bytes()
                .await?
                .into_iter()
                .collect::<Vec<u8>>();

            new_attachments.push(twilight_model::http::attachment::Attachment::from_bytes(
                attachment.filename,
                bytes,
                in_ms,
            ));
        }
    }
    Ok((new_content, new_attachments))
}

fn reference_to_url(
    reference: &Option<MessageReference>,
    dest_guild: Id<GuildMarker>,
    conn: &mut diesel::SqliteConnection,
) -> std::result::Result<Option<String>, DatabaseError> {
    if let Some(reference) = reference {
        let Some(channel_id) = reference.channel_id else {
            return Ok(None);
        };
        let Some(message_id) = reference.message_id else {
            return Ok(None);
        };

        let Some(channel_id) = mapping::get_mapping(conn, Some(channel_id.get()), None)?
            .map(|x| x.ref_id::<ChannelMarker>())
        else {
            return Ok(None);
        };
        let Some(message_id) = mapping::get_mapping(conn, Some(message_id.get()), None)?
            .map(|x| x.ref_id::<MessageMarker>())
        else {
            return Ok(None);
        };

        Ok(Some(format!(
            "https://discord.com/channels/{}/{}/{}",
            dest_guild, channel_id, message_id
        )))
    } else {
        Ok(None)
    }
}

pub async fn handle_event(
    event: &Event,
    client: &Client,
    pool: &Pool,
    config: &[GuildSettings],
    sleep_duration: Duration,
) -> Result<()> {
    let guild_id = match event {
        Event::ChannelPinsUpdate(e) => e.guild_id,
        Event::MessageDelete(e) => e.guild_id,
        Event::MessageDeleteBulk(e) => e.guild_id,
        Event::MessageUpdate(e) => e.guild_id,
        _ => event.guild_id(),
    };

    let Some(guild_id) = guild_id else {
        return Ok(());
    };

    // send the event only to already configured guild (if any)
    for settings in config.iter().filter(|s| guild_id == s.src_guild_id) {
        handle_single_event(event, client, pool, settings, sleep_duration).await?;
    }

    Ok(())
}

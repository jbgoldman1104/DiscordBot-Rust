use std::{collections::HashMap, time::Duration};

use crate::{
    actions::{self, mapping, DatabaseError},
    config::{Blacklist, Config, GuildSettings, SystemSettings},
    model::{IdMapping, MappingData},
};
use async_fs::OpenOptions;
use deadpool_diesel::sqlite::{Manager, Pool, Runtime};
use diesel::SqliteConnection;
use either::Either;
use futures_lite::io::AsyncReadExt;
use twilight_http::{request::guild::update_guild_channel_positions::Position, Client};
use twilight_model::{
    channel::{Channel, ChannelType},
    guild::GuildFeature,
    id::{
        marker::{ChannelMarker, GuildMarker},
        Id,
    },
};

pub fn build_http_client(
    config: &SystemSettings,
) -> Result<twilight_http::Client, http::header::InvalidHeaderValue> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "User-Agent",
        http::HeaderValue::from_str(&config.user_agent)?,
    );

    headers.insert(
        "Authorization",
        http::HeaderValue::from_str(&config.discord_token)?,
    );

    Ok(twilight_http::Client::builder()
        .default_headers(headers)
        .build())
}

pub fn build_gateway_shard(config: &SystemSettings) -> twilight_gateway::Shard {
    use twilight_gateway::{EventTypeFlags, Intents, Shard, ShardId};
    use twilight_model::gateway::payload::outgoing::identify::IdentifyProperties;

    let intents = Intents::all();
    let event_types = EventTypeFlags::all();
    let identify_properties = IdentifyProperties::new(&config.user_agent, "", "Windows");

    let gateway_config = twilight_gateway::Config::builder(config.discord_token.clone(), intents)
        .identify_properties(identify_properties)
        .event_types(event_types)
        .build();

    Shard::with_config(ShardId::ONE, gateway_config)
}

pub async fn get_connection_pool(database_url: &str) -> anyhow::Result<Pool> {
    let manager = Manager::new(database_url, Runtime::Tokio1);

    let pool = Pool::builder(manager).build()?;

    match pool
        .clone()
        .get()
        .await?
        .interact(|conn| crate::migrations::run_migrations(conn))
        .await
    {
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, format!("{:?}", e)).into()),
        _ => Ok(pool),
    }
}

pub async fn get_config() -> anyhow::Result<Config> {
    let filename = match std::env::args().last() {
        Some(v) if v.ends_with(".toml") => Some(v),
        _ => None,
    }
    .unwrap_or("config.toml".to_string());

    tracing::info!("Reading config file '{}'", filename);

    async fn read_file(filename: &str) -> Result<String, std::io::Error> {
        let mut contents = String::new();

        OpenOptions::new()
            .read(true)
            .open(filename)
            .await?
            .read_to_string(&mut contents)
            .await?;

        Ok(contents)
    }

    let contents = read_file(&filename).await?;
    Ok(toml::from_str(&contents)?)
}

#[derive(thiserror::Error, Debug)]
pub enum MappingsError {
    #[error("Client request failed. {0}")]
    ClientSend(#[from] twilight_http::error::Error),
    #[error("Deserializing server response failed. {0}")]
    ClientSerde(#[from] twilight_http::response::DeserializeBodyError),
    #[error("Couldn't fetch database pool. {0}")]
    Pool(#[from] deadpool_diesel::PoolError),
    #[error("Pool interaction failed. {0}")]
    Interact(#[from] deadpool_diesel::InteractError),
    #[error("Database action failed. {0}")]
    Database(#[from] actions::DatabaseError),
    #[error("Could not validate channel when creating. {0}")]
    ChannelCreateValidation(#[from] twilight_validate::channel::ChannelValidationError),
}

pub async fn cleanup_unused_mappings(
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
    guilds: &[GuildSettings],
) -> Result<(), MappingsError> {
    let registered_guilds = object.interact(mapping::get_registered_guilds).await??;

    let mut counter = 0u32;
    for (src, dest) in registered_guilds {
        if guilds.iter().filter(|x| x.src_guild_id == src && x.dest_guild_id == dest).peekable().peek().is_none() {
            unregister_guild_pair(object, src, dest).await?;
            counter += 1;
        }
    }

    tracing::info!("Cleaned up {} unused mappings.", counter);

    Ok(())
}

pub async fn register_guild_settings(
    guilds: &[GuildSettings],
    sleep_duration: Duration,
    client: &Client,
    pool: &Pool,
) -> Result<(), MappingsError> {
    cleanup_unused_mappings(&pool.get().await?, guilds).await?;

    for settings in guilds {
        let object = pool.get().await?;
        let mut blacklists_sorted = settings.blacklists.clone();
        blacklists_sorted.sort();

        let src_guild = client.guild(settings.src_guild_id).await?.model().await?;
        let dest_guild = client.guild(settings.dest_guild_id).await?.model().await?;
        tokio::time::sleep(sleep_duration * 2).await;

        let src_channels = client.guild_channels(src_guild.id).await?.model().await?;
        let src_roles = client.roles(src_guild.id).await?.model().await?;
        let src_threads = vec![];
        tokio::time::sleep(sleep_duration * 2).await;

        if is_guild_pair_discrepant(&src_guild, &dest_guild) {
            tracing::warn!("Guilds ({} -> {}) are incompatible (one of them is a community while the other one isn't)", src_guild.id, dest_guild.id);
            tracing::warn!("Channels will still be created but incompatible channels will be replaced by their closest relative");
        };

        if src_guild.id == dest_guild.id {
            tracing::warn!(
                "In guild pair ({} -> {}) source and dest guilds are the same, skipping...",
                src_guild.id,
                dest_guild.id
            );
            continue;
        }

        unregister_blacklisted_channels(&object, blacklists_sorted.clone()).await?;

        let filtered_channels =
            filter_bar_channels(&object, src_channels.clone(), blacklists_sorted.clone()).await?;

        let filtered_roles =
            filter_bar_roles(&object, src_roles, blacklists_sorted.clone()).await?;

        let filtered_threads = filter_blacklisted_threads(blacklists_sorted, src_threads);

        tracing::info!(
            "Registering guild ({}) -> ({})",
            src_guild.id,
            dest_guild.id
        );

        if !filtered_channels.is_empty() {
            join_threads(client, filtered_threads, sleep_duration).await?;

            let dest_channels =
                create_channels(filtered_channels, client, &dest_guild, sleep_duration).await?;
            tracing::info!(" - Created channels");
            register_channels(
                dest_channels.clone(),
                (src_guild.id, dest_guild.id),
                &object,
            )
            .await?;
            tracing::info!(" - Registered channels");

            // stupid map building so categories are added in but already mapped (OR blacklisted) channels excluded :/
            let dest_channels = dest_channels.into_values().collect::<Vec<_>>();

            let dest_channels = client
                .guild_channels(dest_guild.id)
                .await?
                .model()
                .await?
                .into_iter()
                .filter(|x| dest_channels.contains(x) || x.kind == ChannelType::GuildCategory)
                .collect::<Vec<Channel>>();

            let channel_map = build_channel_map(src_channels, dest_channels, &object).await?;

            reposition_channels(dest_guild.id, &channel_map, client, sleep_duration).await?;
            tracing::info!(" - Repositioned channels");
        } else {
            tracing::info!(" - Nothing to do on channels");
        }

        if !filtered_roles.is_empty() {
            if !settings.clone_roles {
                tracing::warn!(" - Cloning roles has been disabled, skipping...");
                continue;
            }

            let roles = create_roles(filtered_roles, client, dest_guild.id, sleep_duration).await?;
            tracing::info!(" - Created roles");
            register_roles(roles.clone(), (src_guild.id, dest_guild.id), &object).await?;
            tracing::info!(" - Registered roles");
        } else {
            tracing::info!(" - Nothing to do on roles");
        }
    }

    Ok(())
}

#[allow(clippy::manual_try_fold)]
pub async fn build_channel_map(
    src_channels: Vec<Channel>,
    dest_channels: Vec<Channel>,
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
) -> Result<HashMap<Channel, Channel>, MappingsError> {
    let dest_channels = dest_channels
        .into_iter()
        .map(|x| (x.id, x))
        .collect::<HashMap<_, _>>();

    Ok(object
        .interact(
            move |conn| -> Result<HashMap<Channel, Channel>, DatabaseError> {
                src_channels
                    .iter()
                    .map(|x| -> Result<(&Channel, Option<&Channel>), DatabaseError> {
                        let mapping = mapping::get_mapping(conn, Some(x.id.get()), None)?;
                        match mapping {
                            Some(v) => Ok((x, dest_channels.get(&v.ref_id::<ChannelMarker>()))),
                            None => Ok((x, None)),
                        }
                    })
                    .fold(
                        Result::<HashMap<Channel, Channel>, DatabaseError>::Ok(HashMap::new()),
                        |mut acc, x| {
                            let Ok(ref mut map) = acc else {
                                return acc;
                            };

                            match x {
                                Ok((s, Some(d))) => {
                                    map.insert(s.to_owned(), d.to_owned());
                                }
                                Err(e) => {
                                    acc = Err(e);
                                }
                                _ => {}
                            };

                            acc
                        },
                    )
            },
        )
        .await??)
}

/// this function unregisters all mappings belonging to
/// a destination guild
pub async fn unregister_guild_pair(
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
    src_guild_id: Id<GuildMarker>,
    dest_guild_id: Id<GuildMarker>,
) -> Result<(), MappingsError> {
    let guild_mappings = object
        .interact(move |conn| mapping::get_mappings(conn, src_guild_id, dest_guild_id))
        .await??;

    if guild_mappings.is_empty() {
        return Ok(());
    }

    object
        .interact(move |conn| -> Result<(), DatabaseError> {
            guild_mappings
                .iter()
                .map(|x| mapping::unregister_mapping(conn, x.clone()))
                .collect::<Result<Vec<_>, DatabaseError>>()?;

            Ok(())
        })
        .await??;

    Ok(())
}

/// this function will attempt to delete everything (roles, channels) in a guild
pub async fn delete_everything_guild(
    client: &Client,
    guild_id: Id<GuildMarker>,
    sleep_duration: Duration,
) -> Result<(), MappingsError> {
    let channels = client.guild_channels(guild_id).await?.model().await?;
    let roles = client.roles(guild_id).await?.model().await?;

    for channel in channels {
        client.delete_channel(channel.id).await?;
        tracing::info!(" - Deleted channel <#{}>", channel.id);
        tokio::time::sleep(sleep_duration).await;
    }

    for role in roles {
        client.delete_role(guild_id, role.id).await?;
        tracing::info!(" - Deleted role <@&{}>", role.id);
        tokio::time::sleep(sleep_duration).await;
    }

    Ok(())
}

pub fn is_guild_pair_discrepant(
    src_guild: &twilight_model::guild::Guild,
    dest_guild: &twilight_model::guild::Guild,
) -> bool {
    src_guild.features.contains(&GuildFeature::Community)
        && !dest_guild.features.contains(&GuildFeature::Community)
}

pub fn filter_blacklisted_threads(
    blacklists_sorted: Vec<Blacklist>,
    threads: Vec<Channel>,
) -> Vec<Channel> {
    threads.into_iter().fold(vec![], |mut acc, thread| {
        let Some(parent_id) = thread.parent_id else {
            return acc;
        };

        if blacklists_sorted
            .binary_search(&Blacklist::Channel(parent_id))
            .is_ok()
        {
            return acc;
        }

        acc.push(thread);

        acc
    })
}

pub async fn join_threads(
    client: &Client,
    threads: Vec<Channel>,
    sleep_duration: Duration,
) -> Result<Vec<Channel>, MappingsError> {
    let mut joined_threads = vec![];

    for thread in threads {
        client.join_thread(thread.id).await?;

        tracing::info!(" - Joined thread <#{}>", thread.id);

        joined_threads.push(thread);

        tokio::time::sleep(sleep_duration).await;
    }

    Ok(joined_threads)
}

pub async fn create_roles(
    src_roles: Vec<twilight_model::guild::Role>,
    client: &Client,
    dest_guild_id: Id<GuildMarker>,
    sleep_duration: Duration,
) -> Result<HashMap<twilight_model::guild::Role, twilight_model::guild::Role>, MappingsError> {
    let mut res = HashMap::new();

    for src_role in src_roles {
        if src_role.name == "@everyone" {
            tracing::info!(" - Skipping @everyone");
            continue;
        }

        let mut create_role = client.create_role(dest_guild_id);

        create_role = create_role.color(src_role.color);
        create_role = create_role.hoist(src_role.hoist);
        create_role = create_role.mentionable(src_role.mentionable);
        create_role = create_role.name(&src_role.name);
        create_role = create_role.permissions(src_role.permissions);

        let dest_role = create_role.await?.model().await?;

        tracing::info!(
            " - Created role <@&{}> -> <@&{}>",
            src_role.id,
            dest_role.id
        );

        res.insert(src_role, dest_role);

        tokio::time::sleep(sleep_duration).await;
    }

    Ok(res)
}

pub async fn reposition_roles(
    dest_guild_id: Id<GuildMarker>,
    roles: HashMap<twilight_model::guild::Role, twilight_model::guild::Role>,
    client: &Client,
) -> Result<(), MappingsError> {
    if roles.is_empty() {
        return Ok(());
    }

    let positions = &roles
        .into_iter()
        .map(|(src_role, dest_role)| (dest_role.id, src_role.position as u64))
        .collect::<Vec<_>>();

    client
        .update_role_positions(dest_guild_id, positions)
        .await?;

    Ok(())
}

pub async fn register_roles(
    roles: HashMap<twilight_model::guild::Role, twilight_model::guild::Role>,
    guild_ids: (Id<GuildMarker>, Id<GuildMarker>),
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
) -> Result<(), MappingsError> {
    let (src_guild_id, dest_guild_id) = guild_ids;

    object
        .interact(move |conn| {
            roles
                .into_iter()
                .map(
                    |(src_role, dest_role)| -> std::result::Result<(), DatabaseError> {
                        mapping::register_mapping(
                            conn,
                            IdMapping::new(
                                &src_role.id.get(),
                                &dest_role.id.get(),
                                &src_guild_id,
                                &dest_guild_id,
                            )
                            .set_src_mapping_data(MappingData::Role(src_role.clone()))?
                            .set_dest_mapping_data(MappingData::Role(dest_role.clone()))?,
                        )?;

                        tracing::info!(
                            " - Registered role <#{}> -> <#{}>",
                            src_role.id,
                            dest_role.id
                        );

                        Ok(())
                    },
                )
                .collect::<Result<Vec<_>, DatabaseError>>()
        })
        .await??;

    Ok(())
}

pub async fn create_channels(
    src_channels: Vec<twilight_model::channel::Channel>,
    client: &Client,
    dest_guild: &twilight_model::guild::Guild,
    sleep_duration: Duration,
) -> Result<
    HashMap<twilight_model::channel::Channel, twilight_model::channel::Channel>,
    MappingsError,
> {
    async fn create_channel(
        client: &Client,
        src_channel: &twilight_model::channel::Channel,
        dest_guild: &twilight_model::guild::Guild,
    ) -> Result<Option<twilight_model::channel::Channel>, MappingsError> {
        let src_channel_name = if let Some(v) = src_channel.name.clone() {
            v
        } else {
            "no-name".to_string()
        };

        let mut create_channel = client
            .create_guild_channel(dest_guild.id, &src_channel_name)?
            .kind(src_channel.kind);

        match src_channel.kind {
            ChannelType::GuildText | ChannelType::GuildVoice | ChannelType::GuildCategory => {
                create_channel = create_channel.kind(src_channel.kind);
            }
            ChannelType::GuildAnnouncement
            | ChannelType::GuildDirectory
            | ChannelType::GuildForum
            | ChannelType::GuildStageVoice
                if dest_guild.features.contains(&GuildFeature::Community) =>
            {
                create_channel = create_channel.kind(src_channel.kind);
            }
            ChannelType::GuildStageVoice
                if !dest_guild.features.contains(&GuildFeature::Community) =>
            {
                create_channel = create_channel.kind(ChannelType::GuildVoice);
            }
            ChannelType::GuildAnnouncement | ChannelType::GuildForum
                if !dest_guild.features.contains(&GuildFeature::Community) =>
            {
                create_channel = create_channel.kind(ChannelType::GuildText);
            }
            _ => {
                return Ok(None);
            }
        }

        if let Some(v) = src_channel.nsfw {
            create_channel = create_channel.nsfw(v);
        }

        if let Some(v) = &src_channel.topic {
            create_channel = create_channel.topic(v)?;
        }

        if let Some(v) = &src_channel.permission_overwrites {
            create_channel = create_channel.permission_overwrites(v);
        }

        let dest_channel = create_channel.await?.model().await?;

        tracing::info!(
            " - Created channel <#{}> -> <#{}>",
            src_channel.id,
            dest_channel.id
        );

        Ok(Some(dest_channel))
    }

    let mut res = HashMap::new();

    for src_channel in src_channels.iter() {
        let Some(dest_channel) = create_channel(client, src_channel, dest_guild).await? else {
            continue;
        };

        res.insert(src_channel.clone(), dest_channel);

        tokio::time::sleep(sleep_duration).await;
    }

    Ok(res)
}

pub async fn reposition_channels(
    dest_guild_id: Id<GuildMarker>,
    channel_map: &HashMap<twilight_model::channel::Channel, twilight_model::channel::Channel>,
    client: &Client,
    sleep_duration: Duration,
) -> Result<(), MappingsError> {
    if channel_map.is_empty() {
        return Ok(());
    }

    let category_channel_map = channel_map
        .iter()
        .filter(|(s, d)| {
            s.kind == ChannelType::GuildCategory && d.kind == ChannelType::GuildCategory
        })
        .map(|(s, d)| (s.id, d))
        .collect::<HashMap<_, _>>();

    let non_category_channel_map = channel_map
        .iter()
        .filter(|(s, d)| {
            s.kind != ChannelType::GuildCategory && d.kind != ChannelType::GuildCategory
        })
        .collect::<HashMap<_, _>>();

    // positioning channels inside categories
    for (src_channel, dest_channel) in non_category_channel_map {
        let Some(Some(parent_id)) = src_channel
            .parent_id
            .map(|x| category_channel_map.get(&x).map(|x| x.id))
        else {
            continue;
        };

        client
            .update_channel(dest_channel.id)
            .parent_id(Some(parent_id))
            .await?;

        tracing::info!(
            " - Positioned channel (<#{}> -> <#{}>) inside of dest category (<#{}>)",
            src_channel.id,
            dest_channel.id,
            parent_id
        );

        tokio::time::sleep(sleep_duration).await;
    }

    // positioning channels with index
    let positions = &channel_map
        .iter()
        .fold(vec![], |mut acc, (src_channel, dest_channel)| {
            if let Some(pos) = src_channel.position {
                acc.push(Position::from((dest_channel.id, pos as u64)));
            }

            acc
        });

    client
        .update_guild_channel_positions(dest_guild_id, positions)
        .await?;

    Ok(())
}

pub async fn register_channels(
    channels: HashMap<twilight_model::channel::Channel, twilight_model::channel::Channel>,
    guild_ids: (Id<GuildMarker>, Id<GuildMarker>),
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
) -> Result<(), MappingsError> {
    let (src_guild_id, dest_guild_id) = guild_ids;

    object
        .interact(move |conn| {
            channels
                .iter()
                .map(
                    |(src_channel, dest_channel)| -> std::result::Result<(), DatabaseError> {
                        mapping::register_mapping(
                            conn,
                            IdMapping::new(
                                &src_channel.id.get(),
                                &dest_channel.id.get(),
                                &src_guild_id,
                                &dest_guild_id,
                            )
                            .set_src_mapping_data(MappingData::Channel(src_channel.clone()))?
                            .set_dest_mapping_data(
                                MappingData::Channel(dest_channel.clone()),
                            )?,
                        )?;

                        tracing::info!(
                            " - Registered channel <#{}> -> <#{}>",
                            src_channel.id,
                            dest_channel.id
                        );

                        Ok(())
                    },
                )
                .collect::<Result<Vec<_>, DatabaseError>>()
        })
        .await??;

    Ok(())
}

/// BaR - blacklisted and registered
async fn filter_bar_channels(
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
    src_guild_channels: Vec<twilight_model::channel::Channel>,
    blacklists_sorted: Vec<Blacklist>,
) -> Result<Vec<twilight_model::channel::Channel>, MappingsError> {
    let interact_fn = move |conn: &mut SqliteConnection| {
        src_guild_channels
            .iter()
            .filter(|x| {
                let mut res = true;
                res = res
                    && blacklists_sorted
                        .binary_search(&Blacklist::Channel(x.id))
                        .is_err();

                if let Ok(mapping) = mapping::get_mapping(conn, Some(x.id.get()), None) {
                    res = res && mapping.is_none();
                }

                res
            })
            .cloned()
            .collect::<Vec<_>>()
    };

    let filtered_channels = object.interact(interact_fn).await?;

    Ok(filtered_channels)
}

/// BaR - blacklisted and registered
async fn filter_bar_roles(
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
    src_roles: Vec<twilight_model::guild::Role>,
    blacklists_sorted: Vec<Blacklist>,
) -> Result<Vec<twilight_model::guild::Role>, MappingsError> {
    let interact_fn = move |conn: &mut SqliteConnection| {
        src_roles
            .iter()
            .filter(|x| {
                let mut res = true;
                res = res
                    && blacklists_sorted
                        .binary_search(&Blacklist::Role(x.id))
                        .is_err();

                if let Ok(mapping) = mapping::get_mapping(conn, Some(x.id.get()), None) {
                    res = res && mapping.is_none();
                }

                res
            })
            .cloned()
            .collect::<Vec<_>>()
    };

    let filtered_roles = object.interact(interact_fn).await?;

    Ok(filtered_roles)
}

async fn unregister_blacklisted_channels(
    object: &deadpool::managed::Object<deadpool_diesel::Manager<diesel::SqliteConnection>>,
    blacklists_sorted: Vec<Blacklist>,
) -> Result<(), MappingsError> {
    let mut counter = 0;
    for blacklist in blacklists_sorted {
        if let Blacklist::Channel(id) = blacklist {
            if let Some(v) = object
                .interact(move |conn| mapping::get_mapping(conn, Some(id.get()), None))
                .await??
            {
                object
                    .interact(|conn| mapping::unregister_mapping(conn, v))
                    .await??;

                counter += 1;
            }
        }
    }

    tracing::info!("Unregistered {} blacklisted channels", counter);

    Ok(())
}

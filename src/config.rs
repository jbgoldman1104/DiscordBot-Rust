use serde::Deserialize;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
    Id,
};

const fn default_bool_true() -> bool {
    true
}

fn default_webhook_username() -> String {
    "Captain Hook".to_string()
}

#[derive(Deserialize, Default, Debug)]
pub struct SystemSettings {
    pub duration_between_requests_secs: f64,
    pub database_url: String,
    pub discord_token: String,
    pub user_agent: String,
}

#[derive(Deserialize, Debug)]
pub struct GuildSettings {
    pub src_guild_id: Id<GuildMarker>,
    pub dest_guild_id: Id<GuildMarker>,
    #[serde(default = "default_webhook_username")]
    #[serde(alias = "default_webhook_username")]
    pub webhook_username: String,
    #[serde(default)]
    pub relay_deletes: bool,
    #[serde(default)]
    pub relay_updates: bool,
    #[serde(default = "default_bool_true")]
    pub clone_roles: bool,
    #[serde(alias = "requirements")]
    #[serde(default)]
    pub message_requirements: MessageRequirements,
    #[serde(alias = "msg_settings")]
    #[serde(default)]
    pub message_settings: MessageSettings,
    #[serde(alias = "blacklist")]
    #[serde(default)]
    pub blacklists: Vec<Blacklist>,
}

#[derive(Deserialize, Debug)]
pub struct MessageSettings {
    #[serde(default)]
    pub use_webhook_profile: bool,
    #[serde(default)]
    pub remove_attachments: bool,
    #[serde(default = "default_bool_true")]
    pub mirror_messages_from_bots: bool,
    #[serde(default = "default_bool_true")]
    pub mirror_reply_messages: bool,
}

#[derive(Deserialize, Debug, Default)]
pub struct MessageRequirements {
    pub min_embeds_amount: u32,
    pub min_content_length: u32,
    pub min_attachments_amount: u32,
}

#[derive(Deserialize, Debug, Clone, Ord, Eq, PartialOrd, PartialEq)]
#[serde(tag = "type", content = "id")]
pub enum Blacklist {
    Channel(Id<ChannelMarker>),
    User(Id<UserMarker>),
    Role(Id<RoleMarker>),
}

#[derive(Deserialize, Default, Debug)]
pub struct Config {
    pub system: SystemSettings,
    #[serde(alias = "guild")]
    pub guilds: Vec<GuildSettings>,
}

impl Default for MessageSettings {
    fn default() -> Self {
        MessageSettings {
            use_webhook_profile: false,
            remove_attachments: false,
            mirror_messages_from_bots: true,
            mirror_reply_messages: true,
        }
    }
}

use crate::schema::*;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use twilight_model::id::{marker::GuildMarker, Id};
use twilight_model::*;

#[derive(Identifiable, Selectable, Queryable, Insertable, AsChangeset, PartialEq, Clone)]
#[diesel(primary_key(orig_id, ref_id))]
pub struct IdMapping {
    orig_id: Vec<u8>,
    ref_id: Vec<u8>,
    orig_guild_id: Vec<u8>,
    ref_guild_id: Vec<u8>,
    src_mapping_data: Option<Vec<u8>>,
    dest_mapping_data: Option<Vec<u8>>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Deserialize, Serialize)]
#[serde(tag = "t", content = "c")] 
pub enum MappingData {
    Message(channel::Message),
    Channel(channel::Channel),
    Role(guild::Role),
}

impl IdMapping {
    pub fn new(
        orig_id: &u64,
        ref_id: &u64,
        orig_guild_id: &Id<GuildMarker>,
        ref_guild_id: &Id<GuildMarker>,
    ) -> Self {
        Self {
            orig_id: orig_id.to_be_bytes().to_vec(),
            ref_id: ref_id.to_be_bytes().to_vec(),
            orig_guild_id: orig_guild_id.get().to_be_bytes().to_vec(),
            ref_guild_id: ref_guild_id.get().to_be_bytes().to_vec(),
            src_mapping_data: None,
            dest_mapping_data: None,
        }
    }

    pub fn mapping_data(&self) -> (Option<MappingData>, Option<MappingData>) {
        let src = match self
            .src_mapping_data
            .clone()
            .map(|x| serde_json::from_slice(&x))
        {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => {
                tracing::warn!("Error when deserializing mapping data. {}", e);
                None
            },
            _ => None
        };

        let dest = match self
            .dest_mapping_data
            .clone()
            .map(|x| serde_json::from_slice(&x))
        {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => {
                tracing::warn!("Error when deserializing mapping data. {}", e);
                None
            },
            _ => None
        };

        (src, dest)
    }

    pub fn set_src_mapping_data(self, data: MappingData) -> Result<Self, serde_json::Error> {
        Ok(Self {
            src_mapping_data: Some(serde_json::to_string(&data)?.into_bytes()),
            ..self
        })
    }

    pub fn set_dest_mapping_data(
        self,
        data: MappingData,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            dest_mapping_data: Some(serde_json::to_string(&data)?.into_bytes()),
            ..self
        })
    }

    pub fn orig_id<T>(&self) -> Id<T> {
        Id::<T>::new(u64::from_be_bytes(
            self.orig_id[0..8].try_into().expect("Invalid slice size"),
        ))
    }

    pub fn ref_id<T>(&self) -> Id<T> {
        Id::<T>::new(u64::from_be_bytes(
            self.ref_id[0..8].try_into().expect("Invalid slice size"),
        ))
    }

    pub fn set_ref_id(self, ref_id: u64) -> Self {
        Self {
            ref_id: ref_id.to_be_bytes().to_vec(),
            ..self
        }
    }

    pub fn orig_guild_id(&self) -> Id<GuildMarker> {
        Id::<GuildMarker>::new(u64::from_be_bytes(
            self.orig_guild_id[0..8]
                .try_into()
                .expect("Invalid slice size"),
        ))
    }

    pub fn ref_guild_id(&self) -> Id<GuildMarker> {
        Id::<GuildMarker>::new(u64::from_be_bytes(
            self.ref_guild_id[0..8]
                .try_into()
                .expect("Invalid slice size"),
        ))
    }
}

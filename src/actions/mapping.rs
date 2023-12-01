use twilight_model::id::{marker::GuildMarker, Id};

use super::*;

pub fn register_mapping(conn: &mut SqliteConnection, mapping: IdMapping) -> Result<usize> {
    Ok(diesel::insert_into(id_mappings::table)
        .values(&mapping)
        .execute(conn)?)
}

pub fn unregister_mapping(conn: &mut SqliteConnection, mapping: IdMapping) -> Result<usize> {
    Ok(diesel::delete(id_mappings::table.find(mapping.id())).execute(conn)?)
}

pub fn update_mapping(
    conn: &mut SqliteConnection,
    mapping: IdMapping,
    new_mapping: IdMapping,
) -> Result<usize> {
    Ok(diesel::update(id_mappings::table.find(mapping.id()))
        .set(new_mapping)
        .execute(conn)?)
}

pub fn get_mapping(
    conn: &mut SqliteConnection,
    orig_id: Option<u64>,
    ref_id: Option<u64>,
) -> Result<Option<IdMapping>> {
    let mut query = id_mappings::table.limit(1).into_boxed();

    if let Some(id) = orig_id {
        query = query.filter(id_mappings::orig_id.eq(id.to_be_bytes().to_vec()));
    }

    if let Some(id) = ref_id {
        query = query.filter(id_mappings::ref_id.eq(id.to_be_bytes().to_vec()));
    }

    Ok(query.load::<IdMapping>(conn)?.first().cloned())
}

pub fn get_mappings(
    conn: &mut SqliteConnection,
    orig_guild_id: Id<GuildMarker>,
    ref_guild_id: Id<GuildMarker>,
) -> Result<Vec<IdMapping>> {
    Ok(id_mappings::table
        .filter(id_mappings::orig_guild_id.eq(orig_guild_id.get().to_be_bytes().to_vec()))
        .filter(id_mappings::ref_guild_id.eq(ref_guild_id.get().to_be_bytes().to_vec()))
        .load::<IdMapping>(conn)?)
}

pub fn get_registered_guilds(
    conn: &mut SqliteConnection,
) -> Result<Vec<(Id<GuildMarker>, Id<GuildMarker>)>> {
    Ok(id_mappings::table
        .select((id_mappings::orig_guild_id, id_mappings::ref_guild_id))
        .load::<(Vec<u8>, Vec<u8>)>(conn)?
        .iter()
        .map(|x| {
            let orig_id = Id::<GuildMarker>::new(u64::from_be_bytes(
                x.0[0..8].try_into().expect("Invalid slice size"),
            ));
            let ref_id = Id::<GuildMarker>::new(u64::from_be_bytes(
                x.1[0..8].try_into().expect("Invalid slice size"),
            ));

            (orig_id, ref_id)
        })
        .collect::<Vec<_>>())
}

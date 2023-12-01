// @generated automatically by Diesel CLI.

diesel::table! {
    id_mappings (orig_id, ref_id) {
        orig_id -> Binary,
        ref_id -> Binary,
        orig_guild_id -> Binary,
        ref_guild_id -> Binary,
        src_mapping_data -> Nullable<Binary>,
        dest_mapping_data -> Nullable<Binary>,
    }
}

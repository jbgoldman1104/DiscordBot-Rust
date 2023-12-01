-- Your SQL goes here

CREATE TABLE IF NOT EXISTS id_mappings (
	orig_id BLOB NOT NULL UNIQUE,
   	ref_id BLOB NOT NULL UNIQUE,
	orig_guild_id BLOB NOT NULL,
	ref_guild_id BLOB NOT NULL,
	src_mapping_data BLOB,
	dest_mapping_data BLOB,
    PRIMARY KEY (orig_id, ref_id)
) WITHOUT ROWID;
use std::error::Error;

use diesel::sqlite::Sqlite;
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

pub fn run_migrations(
    connection: &mut impl MigrationHarness<Sqlite>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    connection.run_pending_migrations(MIGRATIONS)?;
    
    tracing::info!("Pending migrations have been run");

    Ok(())
}

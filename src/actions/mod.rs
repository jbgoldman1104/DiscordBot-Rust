pub mod mapping;

use diesel::prelude::*;
use crate::model::*;
use crate::schema::*;

#[derive(thiserror::Error, Debug)]
pub enum DatabaseError {
    #[error("Query did not execute properly. {0}")]
    Query(#[from] diesel::result::Error),
    #[error("Failed serializing data. {0}")]
    Serialize(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, DatabaseError>;

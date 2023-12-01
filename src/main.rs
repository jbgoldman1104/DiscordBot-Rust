#![feature(error_generic_member_access)]
#![feature(provide_any)]

mod actions;
mod config;
mod handlers;
mod migrations;
mod model;
mod schema;
mod util;
mod event_handler;
mod timed_event;

use std::time::Duration;

use twilight_gateway::CloseFrame;
use util::{build_gateway_shard, build_http_client, register_guild_settings};

use event_handler::EventHandler;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = util::get_config().await?;

    let pool = util::get_connection_pool(&config.system.database_url).await?;

    let mut shard = build_gateway_shard(&config.system);
    let client = build_http_client(&config.system)?;

    let request_sleep_duration = Duration::from_secs_f64(config.system.duration_between_requests_secs);

    if let Err(error) =
        register_guild_settings(&config.guilds, request_sleep_duration, &client, &pool).await
    {
        tracing::error!("{error}");
        tracing::error!(?error, "Fatal error when setting up mappings");
        return Ok(());
    }

    let event_handler = EventHandler::new(client, pool, config.guilds, request_sleep_duration);

    loop {
        let should_break = tokio::select! {
            should_break = async { match tokio::signal::ctrl_c().await {
                Ok(_) => true,
                Err(e) => {
                    tracing::warn!("Couldn't listen for Ctrl+C. {}", e);
                    false
                },
            } } => {
                if should_break {
                    tracing::info!("Received Ctrl+C, gracefully shutting down...");
                }

                should_break
            },
            should_break = async {
                match shard.next_event().await {
                    Ok(event) => {
                        let _ = event_handler.send(event);

                        false
                    }
                    Err(source) => {
                        if source.is_fatal() {
                            tracing::error!("{source}");
                            tracing::error!(?source, "Fatal error! Terminating...");

                            true
                        } else {
                            false
                        }
                    }
                }
            } => should_break
        };

        if should_break {
            break;
        }
    }

    if shard.status().is_identified() {
        shard.close(CloseFrame::NORMAL).await?;
    }

    event_handler.dispose().await?;

    Ok(())
}

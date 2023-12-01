use std::time::Duration;

use deadpool_diesel::{Manager, Pool};
use diesel::SqliteConnection;
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use twilight_gateway::Event;
use twilight_http::{Client, error::ErrorType};

use crate::{config::GuildSettings, handlers::{self, HandlerError}, timed_event::TimedEvent};

pub struct EventHandler {
    sender: Option<UnboundedSender<Event>>,
    event_task: JoinHandle<()>,
}

impl EventHandler {
    async fn handler_loop(
        mut receiver: UnboundedReceiver<Event>,
        client: Client,
        pool: Pool<Manager<SqliteConnection>>,
        guild_settings: Vec<GuildSettings>,
        request_sleep_duration: Duration,
    ) {
        let (handler_sender, mut handler_receiver) =
            tokio::sync::mpsc::unbounded_channel::<Event>();
        let (failed_sender, mut failed_receiver) =
            tokio::sync::mpsc::unbounded_channel::<TimedEvent>();
        let (wait_sender, mut wait_receiver) =
            tokio::sync::mpsc::unbounded_channel::<TimedEvent>();

        let handler_sender_clone = handler_sender.clone();

        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler_receiver.recv().await {
                // tokio::time::sleep(event.wait_duration()).await;

                if let Err(e) = handlers::handle_event(
                    &event,
                    &client,
                    &pool,
                    &guild_settings,
                    request_sleep_duration,
                )
                .await
                {
                    if let HandlerError::ClientSend(ref e, _) = e {
                        if let ErrorType::Response { body: _, error: _, status } = e.kind() {
                            if *status == 429 {
                                let _ = failed_sender.send(
                                    TimedEvent::new(event.clone(), Duration::from_secs(30)),
                                );
                            }
                        }
                    }

                    tracing::warn!("An error has occurred in handler! {}", e.to_string());
                    tracing::warn!("{}", e.backtrace());
                };
            }

            failed_sender.closed().await;
        });

        let wait_task = tokio::spawn(async move {
            while let Some(event) = wait_receiver.recv().await {
                tokio::time::sleep(event.wait_duration()).await;
                let _ = handler_sender.send(event.clone());
            }

            handler_sender.closed().await;
        });

        loop {
            if tokio::select! {
                event = failed_receiver.recv() => {
                    if let Some(event) = event {
                        wait_sender.send(event).is_err()
                    } else {
                        true
                    }
                },
                event = receiver.recv() => {
                    if let Some(event) = event {
                        handler_sender_clone.send(event).is_err()
                    } else {
                        true
                    }
                },
            } {
                break;
            };
        }

        wait_task.abort();
        handler_task.abort();

        wait_sender.closed().await;
        handler_sender_clone.closed().await;
    }

    pub fn send(&self, event: Event) -> bool {
        let Some(ref sender) = self.sender else {
            return false;
        };

        sender
            .send(event)
            .is_ok()
    }

    pub async fn dispose(mut self) -> std::result::Result<(), tokio::task::JoinError> {
        self.sender = None;
        self.event_task.await
    }

    pub fn new(
        client: Client,
        pool: Pool<Manager<SqliteConnection>>,
        guild_settings: Vec<GuildSettings>,
        request_sleep_duration: Duration,
    ) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<Event>();

        let event_task = tokio::spawn(Self::handler_loop(
            receiver,
            client,
            pool,
            guild_settings,
            request_sleep_duration,
        ));

        Self {
            sender: Some(sender),
            event_task,
        }
    }
}

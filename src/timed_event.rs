use std::time::Duration;

use twilight_gateway::Event;

pub struct TimedEvent {
    event: Event,
    wait_duration: Duration,
}

impl TimedEvent {
    pub fn new(event: Event, wait_duration: Duration) -> Self {
        Self {
            event,
            wait_duration,
        }
    }

    pub fn wait_duration(&self) -> Duration {
        self.wait_duration
    }
}

impl std::ops::Deref for TimedEvent {
    type Target = Event;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

impl std::ops::DerefMut for TimedEvent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.event
    }
}
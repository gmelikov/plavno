use std::{
    fmt,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct RateLimited {
    service: &'static str,
    started: Instant,
    wait: Duration,
}

impl RateLimited {
    pub fn new(wait: Duration) -> Self {
        Self::for_service("Yandex", wait)
    }
    pub fn for_service(service: &'static str, wait: Duration) -> Self {
        Self {
            service,
            started: Instant::now(),
            wait,
        }
    }
    pub fn active(&self) -> bool {
        self.started.elapsed() < self.wait
    }
}

impl fmt::Display for RateLimited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} rate limit (HTTP 429); retry in {}s", self.service, {
            let wait = self.wait.saturating_sub(self.started.elapsed());
            wait.as_secs()
                .saturating_add(u64::from(wait.subsec_nanos() != 0))
        })
    }
}

impl std::error::Error for RateLimited {}

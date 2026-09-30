mod config;
mod message;

#[cfg(test)]
mod tests;

pub use config::*;
pub use message::*;

mod provider;
mod resilience;
mod stream;

pub use provider::*;
pub use resilience::*;
pub use stream::*;
fn is_local_endpoint(base_url: &str) -> bool {
    let authority = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .unwrap_or_default()
            .trim_start_matches('[')
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

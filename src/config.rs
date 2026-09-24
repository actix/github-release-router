use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_database")]
    pub database: String,
    pub sources: BTreeMap<String, Source>,
    pub destinations: BTreeMap<String, Destination>,
    pub routes: Vec<Route>,
}

fn default_bind() -> String {
    "0.0.0.0:8080".into()
}
fn default_database() -> String {
    "data/router.sqlite".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub env: Option<String>,
    pub file: Option<String>,
}

impl Secret {
    pub fn read(&self) -> Result<String> {
        let value = match (&self.env, &self.file) {
            (Some(name), None) => {
                std::env::var(name).context("secret environment variable is unavailable")?
            }
            (None, Some(path)) => {
                std::fs::read_to_string(path).context("secret file is unavailable")?
            }
            _ => bail!("specify exactly one of env or file for each secret"),
        };
        let value = value.trim_end_matches(['\r', '\n']).to_owned();
        ensure!(!value.is_empty(), "secret is empty");
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub secret: Secret,
    pub repositories: Vec<String>,
    #[serde(skip)]
    pub key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub webhook: Secret,
    #[serde(skip)]
    pub url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    pub repository: String,
    pub tag_prefix: Option<String>,
    #[serde(default)]
    pub include_prereleases: bool,
    pub destinations: Vec<String>,
}

fn repository_valid(name: &str) -> bool {
    let parts = name.split('/').collect::<Vec<_>>();
    parts.len() == 2
        && parts.iter().all(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let input = std::fs::read_to_string(path).context("cannot read configuration")?;
        // TOML diagnostics can contain input lines; do not echo possible credentials.
        let mut config: Self = toml::from_str(&input).map_err(|err: toml::de::Error| {
            let line = err.span().map(|span| input[..span.start].bytes().filter(|b| *b == b'\n').count() + 1);
            anyhow::anyhow!("invalid configuration TOML or unknown field near line {line:?}; check config.example.toml")
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&mut self) -> Result<()> {
        self.bind
            .parse::<std::net::SocketAddr>()
            .context("bind must be an IP address and port")?;
        ensure!(!self.database.is_empty(), "database path is empty");
        ensure!(
            !self.sources.is_empty() && !self.destinations.is_empty() && !self.routes.is_empty(),
            "sources, destinations, and routes must not be empty"
        );

        for (name, source) in &mut self.sources {
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
                "invalid source name"
            );
            ensure!(
                !source.repositories.is_empty()
                    && source.repositories.iter().all(|r| repository_valid(r)),
                "source {name}: invalid repository allowlist"
            );
            source.key = source
                .secret
                .read()
                .with_context(|| format!("source {name}"))?;
        }

        for (name, destination) in &mut self.destinations {
            destination.url = destination
                .webhook
                .read()
                .with_context(|| format!("destination {name}"))?;
            let url = reqwest::Url::parse(&destination.url)
                .map_err(|_| anyhow::anyhow!("destination {name}: invalid webhook URL"))?;
            let segments = url
                .path_segments()
                .map(|s| s.collect::<Vec<_>>())
                .unwrap_or_default();
            let credentials = match segments.as_slice() {
                ["api", "webhooks", id, token] | ["api", "v10", "webhooks", id, token] => {
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && !token.is_empty()
                }
                _ => false,
            };
            ensure!(
                url.scheme() == "https"
                    && url.host_str() == Some("discord.com")
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.port().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && credentials,
                "destination {name}: expected https://discord.com/api/v10/webhooks/ID/TOKEN"
            );
        }

        let mut names = std::collections::BTreeSet::new();
        for route in &self.routes {
            ensure!(
                !route.name.is_empty() && names.insert(&route.name),
                "route names must be nonempty and unique"
            );
            ensure!(
                repository_valid(&route.repository),
                "route {}: invalid repository",
                route.name
            );
            ensure!(
                self.sources
                    .values()
                    .any(|s| s.repositories.contains(&route.repository)),
                "route {}: repository is absent from all source allowlists",
                route.name
            );
            ensure!(
                !route.destinations.is_empty()
                    && route
                        .destinations
                        .iter()
                        .all(|d| self.destinations.contains_key(d)),
                "route {}: empty or unknown destinations",
                route.name
            );
        }
        Ok(())
    }
}

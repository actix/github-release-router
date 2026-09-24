use std::collections::BTreeMap;

use crate::{config::Config, github::Release};

pub fn destinations(
    config: &Config,
    repository: &str,
    release: &Release,
) -> BTreeMap<String, Vec<String>> {
    let mut matches = BTreeMap::<String, Vec<String>>::new();
    for route in &config.routes {
        if route.repository == repository
            && (!release.prerelease || route.include_prereleases)
            && route
                .tag_prefix
                .as_ref()
                .is_none_or(|prefix| release.tag_name.starts_with(prefix))
        {
            for destination in &route.destinations {
                matches
                    .entry(destination.clone())
                    .or_default()
                    .push(route.name.clone());
            }
        }
    }
    matches
}

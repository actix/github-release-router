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
            && (route.include_tags.is_empty()
                || route
                    .include_tags
                    .iter()
                    .any(|pattern| pattern.is_match(&release.tag_name)))
            && !route
                .exclude_tags
                .iter()
                .any(|pattern| pattern.is_match(&release.tag_name))
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

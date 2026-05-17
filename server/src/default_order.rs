use std::collections::HashMap;

use anyhow::{Context, Result};
use regex::Regex;

use crate::canonical::canonical_key_with_aliases;
use crate::config::CurationConfig;

#[derive(Default)]
pub struct Curation {
    pub rank: HashMap<String, usize>,
    pub aliases: HashMap<String, String>,
    pub display_overrides: HashMap<String, String>,
    pub logo_overrides: HashMap<String, String>,
    pub provider_boosts: Vec<(Regex, i32)>,
}

impl Curation {
    pub fn from_config(cfg: &CurationConfig) -> Result<Self> {
        let aliases = cfg.aliases.clone();
        let display_overrides = cfg.display_overrides.clone();
        let logo_overrides = cfg.logo_overrides.clone();
        let provider_boosts = cfg
            .provider_boosts
            .iter()
            .map(|pb| {
                Regex::new(&pb.pattern)
                    .with_context(|| format!("compiling provider_boosts pattern: {}", pb.pattern))
                    .map(|re| (re, pb.score))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut rank: HashMap<String, usize> = HashMap::new();
        for (i, name) in cfg.order.iter().enumerate() {
            let k = canonical_key_with_aliases(name, &aliases);
            if !k.is_empty() {
                rank.entry(k).or_insert(i);
            }
        }
        Ok(Self { rank, aliases, display_overrides, logo_overrides, provider_boosts })
    }

    pub fn rank_of(&self, channel_key: &str) -> Option<usize> {
        self.rank.get(channel_key).copied()
    }
}

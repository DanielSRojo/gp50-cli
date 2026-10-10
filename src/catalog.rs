//! Model and parameter catalog (`fxid_ring.json`, copied unchanged from
//! drewmerc302/valeton-gp50, MIT). Keyed by fxid = (category << 24) | model index.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Entry {
    name: Option<String>,
    fxtitle: Option<String>,
    pub origin: Option<String>,
    #[serde(default)]
    pub params: Vec<ParamDef>,
}

impl Entry {
    pub fn model_name(&self) -> Option<String> {
        [&self.name, &self.fxtitle]
            .into_iter()
            .flatten()
            .find(|s| !s.is_empty())
            .cloned()
    }
}

#[derive(Deserialize)]
pub struct ParamDef {
    pub name: String,
    #[serde(rename = "algId")]
    pub alg_id: usize,
    #[serde(default)]
    pub toggle: bool,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub min: f64,
    #[serde(default = "hundred")]
    pub max: f64,
}

fn hundred() -> f64 {
    100.0
}

static CATALOG: LazyLock<HashMap<u32, Entry>> = LazyLock::new(|| {
    let raw: HashMap<String, Entry> =
        serde_json::from_str(include_str!("fxid_ring.json")).expect("embedded catalog is valid");
    raw.into_iter()
        .map(|(k, v)| (k.parse().expect("numeric fxid"), v))
        .collect()
});

pub fn get(fxid: u32) -> Option<&'static Entry> {
    CATALOG.get(&fxid)
}

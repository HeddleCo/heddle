// SPDX-License-Identifier: Apache-2.0
//! Configuration values shared by local repositories and hosted clients.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsMonitorMode {
    #[default]
    Off,
    Auto,
    Native,
    Watchman,
}

impl FsMonitorMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "0" | "off" | "false" | "disabled" => Some(Self::Off),
            "1" | "auto" | "true" | "enabled" => Some(Self::Auto),
            "native" | "local" => Some(Self::Native),
            "watchman" => Some(Self::Watchman),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Json,
    #[default]
    Text,
}

impl<'de> Deserialize<'de> for OutputFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = OutputFormat;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("'text' or 'json'")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<OutputFormat, E> {
                match value {
                    "text" => Ok(OutputFormat::Text),
                    "json" => Ok(OutputFormat::Json),
                    other => Err(E::custom(format!(
                        "invalid output.format: '{other}' — valid values are 'text' or 'json'"
                    ))),
                }
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

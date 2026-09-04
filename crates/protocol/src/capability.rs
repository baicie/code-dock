//! 权限能力清单（§8.3.6）。
//!
//! 权限必须包含具体资源范围，不能只写"允许 fs.read"（见 [`crate::tool::Permission`]）。

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    FsRead,
    FsWrite,
    FsDelete,
    ProcessExecute,
    ProcessKill,
    NetworkConnect,
    GitRead,
    GitWrite,
    GitPush,
    SecretsRead,
    BrowserRead,
    BrowserControl,
    SystemRead,
    SystemWrite,
    ExternalRead,
    ExternalWrite,
}

impl Capability {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Capability::FsRead => "fs.read",
            Capability::FsWrite => "fs.write",
            Capability::FsDelete => "fs.delete",
            Capability::ProcessExecute => "process.execute",
            Capability::ProcessKill => "process.kill",
            Capability::NetworkConnect => "network.connect",
            Capability::GitRead => "git.read",
            Capability::GitWrite => "git.write",
            Capability::GitPush => "git.push",
            Capability::SecretsRead => "secrets.read",
            Capability::BrowserRead => "browser.read",
            Capability::BrowserControl => "browser.control",
            Capability::SystemRead => "system.read",
            Capability::SystemWrite => "system.write",
            Capability::ExternalRead => "external.read",
            Capability::ExternalWrite => "external.write",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Capability {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fs.read" => Ok(Capability::FsRead),
            "fs.write" => Ok(Capability::FsWrite),
            "fs.delete" => Ok(Capability::FsDelete),
            "process.execute" => Ok(Capability::ProcessExecute),
            "process.kill" => Ok(Capability::ProcessKill),
            "network.connect" => Ok(Capability::NetworkConnect),
            "git.read" => Ok(Capability::GitRead),
            "git.write" => Ok(Capability::GitWrite),
            "git.push" => Ok(Capability::GitPush),
            "secrets.read" => Ok(Capability::SecretsRead),
            "browser.read" => Ok(Capability::BrowserRead),
            "browser.control" => Ok(Capability::BrowserControl),
            "system.read" => Ok(Capability::SystemRead),
            "system.write" => Ok(Capability::SystemWrite),
            "external.read" => Ok(Capability::ExternalRead),
            "external.write" => Ok(Capability::ExternalWrite),
            other => Err(format!("未知能力: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn roundtrip() {
        for c in [
            Capability::FsRead,
            Capability::ProcessExecute,
            Capability::GitPush,
            Capability::ExternalWrite,
        ] {
            assert_eq!(Capability::from_str(c.as_str()).unwrap(), c);
        }
    }
}

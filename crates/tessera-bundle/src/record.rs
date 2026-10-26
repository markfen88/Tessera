//! Canonical JSON compatible with the original bundle record.
use crate::{Error, Paths, Result, account, ensure, no_symlink};
use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

pub const DIRECTORY: &str = "/var/lib/tessera-bundle";
pub const FILE: &str = "/var/lib/tessera-bundle/record.json";
/// The flat configuration the speaker profile seeds for the sound card.
pub const SEED: &str = "/opt/camilladsp/configs/Speaker.yml";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Record {
    pub version: String,
    pub commit: String,
    pub profile: String,
    pub complete: bool,
    pub restart_pending: bool,
    /// The run of `tesserad` that `restart_pending` is about: systemd's `InvocationID` of the
    /// old daemon. A later run of the installer clears the flag when the daemon has another
    /// (it was restarted since) and says so. Absent from records written without a pending
    /// restart and by earlier installers.
    pub daemon_invocation: String,
    pub files: Vec<String>,
    pub users_modified: Vec<String>,
    pub groups_created: Vec<String>,
    pub users_created: Vec<String>,
    pub packages: Vec<String>,
    pub dependencies_installed: Vec<String>,
    pub timestamp: String,
    pub purge_pending: bool,
    pub keep_users: bool,
    pub removed: bool,
}

fn recorded_path(value: &str) -> bool {
    let plain = |tail: &str, extension: &str| !tail.contains('/') && tail.ends_with(extension);
    !value.contains("..")
        && (value == FILE
            // The one file of the CamillaDSP data directory the installer creates (the seed
            // configuration): uninstall --purge removes it and nothing else there.
            || value == SEED
            || value
                .strip_prefix("/etc/tessera/config.d/60-")
                .is_some_and(|tail| plain(tail, ".toml"))
            || value
                .strip_prefix("/etc/tessera/web.d/60-")
                .is_some_and(|tail| plain(tail, ".toml"))
            || value
                .strip_prefix("/etc/systemd/system/tesserad.service.d/")
                .is_some_and(|tail| plain(tail, ".conf"))
            || value
                .strip_prefix("/etc/systemd/system/tessera-web.service.d/60-")
                .is_some_and(|tail| plain(tail, ".conf")))
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/_.:@+-".contains(&c))
}

impl Record {
    pub fn validate(&self) -> Result<()> {
        ensure(
            self.files.iter().all(|value| recorded_path(value)),
            "invalid recorded path",
        )?;
        ensure(
            self.users_modified.iter().all(|value| {
                value.split_once(':').is_some_and(|(user, group)| {
                    account(user) && matches!(group, "tessera" | "tessera-admin")
                })
            }),
            "invalid membership",
        )?;
        ensure(
            self.groups_created
                .iter()
                .all(|value| matches!(value.as_str(), "tessera" | "tessera-admin" | "camilla")),
            "invalid group",
        )?;
        ensure(
            self.users_created
                .iter()
                .all(|value| matches!(value.as_str(), "tessera" | "camilla")),
            "invalid user",
        )?;
        ensure(
            self.packages.iter().all(|value| {
                matches!(
                    value.as_str(),
                    "tessera" | "tessera-alsa" | "tessera-web" | "tessera-dsp"
                )
            }),
            "invalid package",
        )?;
        ensure(
            self.dependencies_installed.iter().all(|value| {
                value
                    .as_bytes()
                    .first()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    && value.bytes().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || b"+.-".contains(&c)
                    })
            }),
            "invalid dependency",
        )?;
        for value in [&self.version, &self.commit, &self.profile] {
            ensure(
                value.is_empty() || crate::manifest::token(value),
                "invalid record metadata",
            )?;
        }
        ensure(
            self.daemon_invocation.is_empty()
                || (self.daemon_invocation.len() == 32
                    && self
                        .daemon_invocation
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit())),
            "invalid daemon invocation",
        )?;
        Ok(())
    }

    pub fn read(paths: &Paths) -> Result<Option<Self>> {
        let directory = paths.at(DIRECTORY);
        let file = paths.at(FILE);
        no_symlink(&directory)?;
        no_symlink(&file)?;
        if directory.exists() {
            let metadata = fs::metadata(&directory)?;
            ensure(
                metadata.uid() == paths.uid,
                "record directory has the wrong owner",
            )?;
            ensure(
                metadata.permissions().mode() & 0o7777 == 0o700,
                "record directory must have mode 0700",
            )?;
        }
        if !file.exists() {
            return Ok(None);
        }
        let record: Self = serde_json::from_slice(&fs::read(file)?)?;
        record.validate()?;
        Ok(Some(record))
    }

    pub fn encode(&self) -> Result<String> {
        self.validate()?;
        let mut text = String::from("{\n");
        if self.purge_pending {
            text.push_str(&format!(
                "\"purge_pending\": true,\n\"keep_users\": {},\n",
                self.keep_users
            ));
        }
        if self.removed {
            text.push_str("\"complete\": false,\n");
            // What a purge after a plain removal still has to find (the seed configuration).
            if !self.files.is_empty() {
                array(&mut text, "files", &self.files)?;
            }
        } else {
            text.push_str(&format!("\"version\": {},\n\"commit\": {},\n\"profile\": {},\n\"complete\": {},\n\"restart_pending\": {},\n",
                serde_json::to_string(&self.version)?, serde_json::to_string(&self.commit)?,
                serde_json::to_string(&self.profile)?, self.complete, self.restart_pending));
            if !self.daemon_invocation.is_empty() {
                text.push_str(&format!(
                    "\"daemon_invocation\": {},\n",
                    serde_json::to_string(&self.daemon_invocation)?
                ));
            }
            array(&mut text, "files", &self.files)?;
        }
        // Preserve the legacy install order and the legacy removal order.
        if self.removed {
            array(&mut text, "packages", &self.packages)?;
        }
        array(&mut text, "users_modified", &self.users_modified)?;
        array(&mut text, "groups_created", &self.groups_created)?;
        array(&mut text, "users_created", &self.users_created)?;
        if !self.removed {
            array(&mut text, "packages", &self.packages)?;
        }
        array(
            &mut text,
            "dependencies_installed",
            &self.dependencies_installed,
        )?;
        if self.removed {
            text.push_str("\"removed\": true\n}\n");
        } else {
            text.push_str(&format!(
                "\"timestamp\": {}\n}}\n",
                serde_json::to_string(&self.timestamp)?
            ));
        }
        Ok(text)
    }

    pub fn write(&self, paths: &Paths) -> Result<()> {
        let directory = paths.at(DIRECTORY);
        no_symlink(&directory)?;
        no_symlink(&paths.at(FILE))?;
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        // create_new prevents replacement/following of an attacker-chosen temp.
        for serial in 0..100 {
            let temp = directory.join(format!("record.json.{}.{serial}", std::process::id()));
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            let result = (|| {
                file.write_all(self.encode()?.as_bytes())?;
                file.sync_all()?;
                fs::rename(&temp, paths.at(FILE))?;
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temp);
            }
            return result;
        }
        Err(Error("cannot allocate temporary record".into()))
    }
}

fn array(text: &mut String, key: &str, values: &[String]) -> Result<()> {
    let mut values = values.to_vec();
    values.sort();
    values.dedup();
    text.push_str(&format!("\"{key}\": [\n"));
    for (index, value) in values.iter().enumerate() {
        text.push_str(&format!(
            "  {}{}\n",
            serde_json::to_string(value)?,
            if index + 1 < values.len() { "," } else { "" }
        ));
    }
    text.push_str("],\n");
    Ok(())
}

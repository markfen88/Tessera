//! The CamillaDSP stage of the speaker profile, as plain text transformations:
//! which sound card the profile names, which `[[bridge]]` tables of an earlier
//! tessera-only speaker the profile owns, and the state file CamillaDSP starts
//! from. Nothing here touches the file system or runs a program.

/// A sound card selector as the profile takes it on `--audio-card`.
#[derive(Debug, PartialEq, Eq)]
pub struct Card {
    /// `hw:CARD=<name>[,DEV=<n>]` with the optional `plug` prefix kept.
    pub playback: String,
    /// The card's name, for `amixer -c` and `/proc/asound/<name>`.
    pub name: String,
    /// The device number (`DEV=`, 0 by default).
    pub device: u32,
    /// The selector goes through the `plug` layer, which converts any format.
    pub plug: bool,
}

/// `hw:CARD=Name[,DEV=n]` (optionally `plughw:CARD=...`): the card's name and the playback
/// device CamillaDSP opens, which wants the device number spelled out (`DEV=0` by
/// default, as the profile's bridge would use). Anything else (`default`, `hw:0`, a
/// `plug:` chain) names no card the installer can inspect: `None`, and the selector
/// is used as it is.
#[must_use]
pub fn card(selector: &str) -> Option<Card> {
    let (plug, rest) = match selector.strip_prefix("plug") {
        Some(rest) => ("plug", rest),
        None => ("", selector),
    };
    let rest = rest.strip_prefix("hw:CARD=")?;
    let (name, device) = match rest.split_once(",DEV=") {
        Some((name, device)) => (name, Some(device)),
        None => (rest, None),
    };
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.+-".contains(&c));
    let device_ok =
        device.is_none_or(|d| (1..=3).contains(&d.len()) && d.bytes().all(|c| c.is_ascii_digit()));
    if !name_ok || !device_ok {
        return None;
    }
    let device = device.unwrap_or("0");
    Some(Card {
        playback: format!("{plug}hw:CARD={name},DEV={device}"),
        name: name.to_owned(),
        device: device.parse().ok()?,
        plug: !plug.is_empty(),
    })
}

/// The card index in `/proc/asound/<name>`'s link target (`card3`).
#[must_use]
pub fn card_index(link_target: &str) -> Option<u32> {
    let digits = link_target.strip_prefix("card")?;
    ((1..=3).contains(&digits.len()) && digits.bytes().all(|c| c.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// How many ALSA card indexes the portal's unit may open the control device of: 0 to 31,
/// the kernel's limit on cards.
pub const CARD_LIMIT: u32 = 32;

/// The lines of the portal's unit drop-in that let `amixer` reach the amplifier while
/// `PrivateDevices=yes` stays.
///
/// `amixer -c <name>` finds a card by its id by opening the control devices one after
/// the other, so a unit that may open one `controlC<n>` only (the index at install time)
/// fails with "Invalid card number" as soon as the card has another index. These lines
/// open `controlC0` to `controlC31` instead. `DeviceAllow` takes no wildcard in a path,
/// and a node that does not exist is ignored. The alternative `DeviceAllow=char-alsa rw`
/// would open the PCM, sequencer and timer nodes as well, so the portal (reachable over
/// the network, in the group that owns the nodes) could play and record.
#[must_use]
pub fn control_devices() -> String {
    let mut lines = String::from(
        "# The Volume pane runs amixer, which finds the card by its name by opening each card's\n\
         # control device, so the unit may open every card index (controlC0 to controlC31; a node\n\
         # that does not exist is ignored) and the card may take another index after a reboot.\n\
         # DeviceAllow takes no wildcard in a path, and DeviceAllow=char-alsa would open the PCM,\n\
         # sequencer and timer devices too, which would let the portal play and record.\n\
         BindPaths=-/dev/snd\n",
    );
    for index in 0..CARD_LIMIT {
        lines.push_str(&format!("DeviceAllow=/dev/snd/controlC{index} rw\n"));
    }
    lines.push_str("SupplementaryGroups=audio\n");
    lines
}

/// The first of the mixer controls the portal's Volume card can drive that the card
/// lists in `amixer scontrols` output (`Simple mixer control 'Digital',0`).
#[must_use]
pub fn volume_control(scontrols: &str) -> Option<&'static str> {
    ["Digital", "Master", "PCM", "Speaker"]
        .into_iter()
        .find(|control| scontrols.contains(&format!("'{control}',0")))
}

/// The profile's sample rate: the first `sample_rate = <4 to 6 digits>` line of the
/// speaker configuration, else 48000.
#[must_use]
pub fn sample_rate(config: &str) -> u32 {
    config
        .lines()
        .find_map(|line| {
            let digits = line
                .strip_prefix("sample_rate")?
                .trim_start()
                .strip_prefix('=')?;
            let digits = digits.trim_start();
            let end = digits
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(digits.len());
            ((4..=6).contains(&end))
                .then(|| digits[..end].parse().ok())
                .flatten()
        })
        .unwrap_or(48000)
}

/// What a speaker configuration's `[[bridge]]` tables say.
#[derive(Debug, PartialEq, Eq)]
pub struct Bridges {
    /// Every `[[bridge]]` table.
    pub total: usize,
    /// Those named `speaker`, the one the bundle's own profile writes.
    pub ours: usize,
}

/// The table a line opens (`[[bridge]]`), without a trailing comment.
fn header(line: &str) -> Option<&str> {
    line.starts_with('[')
        .then(|| line.split('#').next().unwrap_or(line).trim_end())
}

fn is_speaker_name(line: &str) -> bool {
    line.strip_prefix("name")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix('='))
        .is_some_and(|value| value.trim_start().starts_with("\"speaker\""))
}

/// Count the bridges of `config`. A `name = "speaker"` line counts for the `[[bridge]]`
/// table it is in.
#[must_use]
pub fn bridges(config: &str) -> Bridges {
    let mut found = Bridges { total: 0, ours: 0 };
    let mut inside = false;
    for line in config.lines() {
        if let Some(table) = header(line) {
            inside = table == "[[bridge]]";
            found.total += usize::from(inside);
        } else if inside && is_speaker_name(line) {
            found.ours += 1;
        }
    }
    found
}

/// The comment lines at the end of a table's text, from the first one after its last
/// setting (blank lines between are dropped with the table).
fn trailing_comments(block: &str) -> &str {
    let mut first = None;
    let mut offset = 0;
    for line in block.split_inclusive('\n') {
        let text = line.trim();
        if text.starts_with('#') {
            first.get_or_insert(offset);
        } else if !text.is_empty() {
            first = None;
        }
        offset += line.len();
    }
    first.map_or("", |at| &block[at..])
}

/// `config` without the `[[bridge]]` tables named `speaker` (a table runs from its header
/// to the next one, its `[bridge.*]` sub-tables included); everything else is kept, and so
/// are the comment lines that lead into the next table.
#[must_use]
pub fn without_speaker_bridge(config: &str) -> String {
    let mut out = String::new();
    let mut block = String::new();
    let mut table = String::new();
    let mut ours = false;
    let flush = |out: &mut String, block: &mut String, table: &str, ours: &mut bool| {
        if block.is_empty() {
            return;
        }
        if table == "[[bridge]]" && *ours {
            // The comments that lead into the next table are the operator's.
            out.push_str(trailing_comments(block));
        } else {
            out.push_str(block);
        }
        block.clear();
        *ours = false;
    };
    // Inside a sub-table of the bridge (`[bridge.options]`) the bridge's block goes on: a
    // removed bridge must not leave its sub-tables behind.
    let mut sub = false;
    for line in config.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if let Some(next) = header(text) {
            if table == "[[bridge]]"
                && (next.starts_with("[bridge.") || next.starts_with("[[bridge."))
            {
                sub = true;
            } else {
                flush(&mut out, &mut block, &table, &mut ours);
                table = next.to_owned();
                sub = false;
            }
        }
        block.push_str(line);
        if table == "[[bridge]]" && !sub && is_speaker_name(text) {
            ours = true;
        }
    }
    flush(&mut out, &mut block, &table, &mut ours);
    out
}

/// Does a CamillaDSP state file name no configuration yet (empty, or `config_path` is
/// null, `~` or empty)?
#[must_use]
pub fn state_unset(state: &str) -> bool {
    if state.trim().is_empty() {
        return true;
    }
    state.lines().any(|line| {
        line.strip_prefix("config_path:")
            .is_some_and(|value| matches!(value.trim(), "" | "null" | "~"))
    })
}

/// The state file with its `config_path` line replaced by `path`.
#[must_use]
pub fn state_with_config(state: &str, path: &str) -> String {
    let mut out = String::new();
    for line in state.split_inclusive('\n') {
        if line.starts_with("config_path:") {
            out.push_str(&format!("config_path: {path}\n"));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// The configuration the state file names (`config_path:`), without quotes. `None` when it
/// names none (absent, null, `~` or empty).
#[must_use]
pub fn state_config(state: &str) -> Option<String> {
    state.lines().find_map(|line| {
        let value = line.strip_prefix("config_path:")?.trim();
        let value = value.trim_matches(['"', '\'']);
        (!matches!(value, "" | "null" | "~")).then(|| value.to_owned())
    })
}

/// The port of the EQ engine's GUI, which the packaged `camillagui.yml` sets and the network
/// helper's firewall guards.
pub const GUI_PORT: u16 = 5005;

/// The GUI's loopback address: the packaged default, reachable from this machine only.
pub const GUI_LOOPBACK: &str = "127.0.0.1";

/// The value of `bind_address:` in the GUI's configuration, without quotes.
#[must_use]
pub fn gui_bind(config: &str) -> Option<String> {
    config.lines().find_map(|line| {
        let value = line.strip_prefix("bind_address:")?;
        let value = value.split('#').next().unwrap_or(value).trim();
        Some(value.trim_matches(['"', '\'']).to_owned())
    })
}

/// Does `address` accept connections on every interface?
#[must_use]
pub fn wildcard(address: &str) -> bool {
    matches!(address, "" | "0.0.0.0" | "::" | "[::]" | "*")
}

/// The GUI's configuration with its `bind_address:` line set to `address`; everything else,
/// comments included, stays. A configuration without the line gets it at the end.
#[must_use]
pub fn with_gui_bind(config: &str, address: &str) -> String {
    let mut out = String::new();
    let mut done = false;
    for line in config.split_inclusive('\n') {
        if !done && line.starts_with("bind_address:") {
            out.push_str(&format!("bind_address: \"{address}\"\n"));
            done = true;
        } else {
            out.push_str(line);
        }
    }
    if !done {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("bind_address: \"{address}\"\n"));
    }
    out
}

/// The first global IPv4 address of `ip -o -4 addr show dev <port> scope global`.
#[must_use]
pub fn first_address(listing: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        fields.find(|field| *field == "inet")?;
        let (address, _) = fields.next()?.split_once('/')?;
        address
            .parse::<std::net::Ipv4Addr>()
            .ok()
            .map(|address| address.to_string())
    })
}

/// The ports that carry a default route in `ip -o -4 route show default`, in order, once each.
#[must_use]
pub fn default_ports(routes: &str) -> Vec<String> {
    let mut ports: Vec<String> = Vec::new();
    for line in routes.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("default") {
            continue;
        }
        if let Some(port) = fields.skip_while(|field| *field != "dev").nth(1) {
            if !ports.iter().any(|known| known == port) {
                ports.push(port.to_owned());
            }
        }
    }
    ports
}

/// A state file that names `path` and leaves volume and mute at their defaults.
#[must_use]
pub fn new_state(path: &str) -> String {
    format!(
        "config_path: {path}\nmute:\n- false\n- false\n- false\n- false\n- false\nvolume:\n- 0.0\n- 0.0\n- 0.0\n- 0.0\n- 0.0\n"
    )
}

/// The playback format of the seeded configuration when the card cannot be asked.
pub const DEFAULT_FORMAT: &str = "S32_LE";

/// The flat starting configuration for `rate`, the playback device and the format it
/// is played in, from the bundle's template.
#[must_use]
pub fn seed(template: &str, rate: u32, playback: &str, format: &str) -> String {
    template
        .replace("@RATE@", &rate.to_string())
        .replace("@PLAYBACK_DEVICE@", playback)
        .replace("@PLAYBACK_FORMAT@", format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_get_their_device_number_spelled_out() {
        for (selector, name, playback, device, plug) in [
            (
                "hw:CARD=DigiAMP",
                "DigiAMP",
                "hw:CARD=DigiAMP,DEV=0",
                0,
                false,
            ),
            (
                "hw:CARD=DigiAMP,DEV=1",
                "DigiAMP",
                "hw:CARD=DigiAMP,DEV=1",
                1,
                false,
            ),
            (
                "plughw:CARD=sndrpihifiberry",
                "sndrpihifiberry",
                "plughw:CARD=sndrpihifiberry,DEV=0",
                0,
                true,
            ),
            (
                "hw:CARD=USB-Audio_2.0+x",
                "USB-Audio_2.0+x",
                "hw:CARD=USB-Audio_2.0+x,DEV=0",
                0,
                false,
            ),
        ] {
            let card = card(selector).unwrap();
            assert_eq!(
                (
                    card.name.as_str(),
                    card.playback.as_str(),
                    card.device,
                    card.plug
                ),
                (name, playback, device, plug),
                "{selector}"
            );
        }
        for other in [
            "default",
            "hw:0",
            "hw:CARD=",
            "hw:CARD=a,DEV=",
            "hw:CARD=a,DEV=x",
            "hw:CARD=a,DEV=1234",
            "hw:CARD=a b",
            "hw:CARD=a,DEV=0,SUBDEV=1",
            "plug:hw:CARD=a",
            "sysdefault:CARD=a",
        ] {
            assert_eq!(card(other), None, "{other}");
        }
    }

    #[test]
    fn card_index_and_volume_control_read_the_systems_words() {
        assert_eq!(card_index("card3"), Some(3));
        assert_eq!(card_index("card12"), Some(12));
        for bad in ["", "card", "cardx", "card1234", "3", "../card3"] {
            assert_eq!(card_index(bad), None, "{bad}");
        }
        let listing = "Simple mixer control 'PCM',0\nSimple mixer control 'Digital',0\n";
        assert_eq!(
            volume_control(listing),
            Some("Digital"),
            "the list's order is ours"
        );
        assert_eq!(
            volume_control("Simple mixer control 'Speaker',0\n"),
            Some("Speaker")
        );
        assert_eq!(volume_control("Simple mixer control 'Mic',0\n"), None);
        assert_eq!(volume_control("Simple mixer control 'Digital',1\n"), None);
        assert_eq!(volume_control(""), None);
    }

    #[test]
    fn the_portal_may_open_every_control_device_and_nothing_else_of_the_card() {
        let lines = control_devices();
        let allowed: Vec<_> = lines
            .lines()
            .filter_map(|line| line.strip_prefix("DeviceAllow="))
            .collect();
        let expected: Vec<_> = (0..32)
            .map(|index| format!("/dev/snd/controlC{index} rw"))
            .collect();
        assert_eq!(allowed, expected);
        // Only control devices: no PCM, sequencer or timer node, no device group.
        for directive in lines.lines().filter(|line| !line.starts_with('#')) {
            for other in ["pcm", "char-", "seq", "timer"] {
                assert!(!directive.contains(other), "{directive}");
            }
        }
        assert!(lines.contains("\nBindPaths=-/dev/snd\n"));
        assert!(lines.ends_with("\nSupplementaryGroups=audio\n"));
        // The decision is written where an administrator reads the unit.
        assert!(lines.starts_with("# The Volume pane runs amixer"));
        assert!(lines.contains("char-alsa"));
        // The documented example drop-in opens exactly the same devices.
        let directives = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|line| {
                    ["DeviceAllow=", "BindPaths=", "SupplementaryGroups="]
                        .iter()
                        .any(|key| line.starts_with(key))
                })
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(
            directives(include_str!("../../tessera-web/dist/appliance-paths.conf")),
            directives(&lines)
        );
        // No index of the install is baked in.
        assert!(!lines.contains("controlC32"));
    }

    #[test]
    fn the_rate_comes_from_the_profile_or_is_48000() {
        assert_eq!(
            sample_rate("[[device]]\nid = \"main\"\nsample_rate = 96000\n"),
            96000
        );
        assert_eq!(sample_rate("sample_rate=44100   # cd\n"), 44100);
        assert_eq!(
            sample_rate("sample_rate = 48000\nsample_rate = 96000\n"),
            48000
        );
        for none in [
            "",
            "sample_rate = 48\n",
            "sample_rate = 1234567\n",
            "rx_sample_rate = 96000\n",
            "sample_rate = fast\n",
        ] {
            assert_eq!(sample_rate(none), 48000, "{none:?}");
        }
    }

    const SPEAKER: &str = "[[device]]\nid = \"main\"\nrx_channels = 2\n\n[[bridge]]\nname = \"speaker\"\ndevice = \"main\"\nalsa_pcm = \"hw:CARD=x\"\n\n# operator note\n[clock]\npriority = 5\n";

    #[test]
    fn the_speaker_bridge_is_found_and_removed_without_touching_the_rest() {
        assert_eq!(bridges(SPEAKER), Bridges { total: 1, ours: 1 });
        assert_eq!(
            without_speaker_bridge(SPEAKER),
            "[[device]]\nid = \"main\"\nrx_channels = 2\n\n# operator note\n[clock]\npriority = 5\n"
        );
        // CRLF files keep their line endings.
        let crlf = SPEAKER.replace('\n', "\r\n");
        assert_eq!(bridges(&crlf), Bridges { total: 1, ours: 1 });
        assert_eq!(
            without_speaker_bridge(&crlf),
            "[[device]]\r\nid = \"main\"\r\nrx_channels = 2\r\n\r\n# operator note\r\n[clock]\r\npriority = 5\r\n"
        );
        // Nothing to remove: the text is unchanged.
        let none = "[[device]]\nid = \"main\"\n";
        assert_eq!(bridges(none), Bridges { total: 0, ours: 0 });
        assert_eq!(without_speaker_bridge(none), none);
        assert_eq!(without_speaker_bridge(""), "");
    }

    #[test]
    fn a_removed_bridge_takes_its_sub_tables_with_it() {
        let config = "[[device]]\nid = \"main\"\n\n[[bridge]]\nname = \"speaker\"\ndevice = \"main\"\n\n[bridge.options]\nname = \"speaker\"\nperiods = 4\n\n[clock]\npriority = 5\n";
        assert_eq!(bridges(config), Bridges { total: 1, ours: 1 });
        assert_eq!(
            without_speaker_bridge(config),
            "[[device]]\nid = \"main\"\n\n[clock]\npriority = 5\n"
        );
        // The sub-tables of a bridge that stays stay with it.
        let other = "[[bridge]]\nname = \"line-out\"\n\n[bridge.options]\nperiods = 4\n";
        assert_eq!(without_speaker_bridge(other), other);
    }

    #[test]
    fn only_bridges_named_speaker_are_ours() {
        let mixed = "[[bridge]]\nname = \"speaker\"\ndevice = \"main\"\n\n[[bridge]]\nname = \"line-out\"\ndevice = \"main\"\n";
        assert_eq!(bridges(mixed), Bridges { total: 2, ours: 1 });
        assert_eq!(
            without_speaker_bridge(mixed),
            "[[bridge]]\nname = \"line-out\"\ndevice = \"main\"\n"
        );
        // A name outside a [[bridge]] table is not a bridge name.
        let other = "[[device]]\nname = \"speaker\"\n\n[[bridge]]\nname = \"other\"\n";
        assert_eq!(bridges(other), Bridges { total: 1, ours: 0 });
        assert_eq!(without_speaker_bridge(other), other);
        assert_eq!(
            bridges("[[bridge]]\nname =   \"speaker\"   # ours\n").ours,
            1
        );
        assert_eq!(bridges("[[bridge]]\nname = \"speaker-2\"\n").ours, 0);
        assert_eq!(bridges("[[bridge]]\nname = 'speaker'\n").ours, 0);
        // A comment after the header does not hide the table.
        assert_eq!(
            bridges("[[bridge]] # kept\nname = \"speaker\"\n"),
            Bridges { total: 1, ours: 1 }
        );
    }

    #[test]
    fn a_state_file_names_no_configuration_until_one_is_set() {
        for unset in [
            "",
            "\n",
            "config_path: null\nmute:\n- false\n",
            "config_path: ~\n",
            "config_path:\nvolume:\n- 0.0\n",
            "config_path:   null  \n",
        ] {
            assert!(state_unset(unset), "{unset:?}");
        }
        for set in [
            "config_path: /opt/camilladsp/configs/Mine.yml\n",
            "config_path: \"/x.yml\"\n",
            "mute:\n- false\n",
        ] {
            assert!(!state_unset(set), "{set:?}");
        }
        let state = "config_path: null\nmute:\n- false\nvolume:\n- 0.0\n";
        assert_eq!(
            state_with_config(state, "/opt/camilladsp/configs/Speaker.yml"),
            "config_path: /opt/camilladsp/configs/Speaker.yml\nmute:\n- false\nvolume:\n- 0.0\n"
        );
        let fresh = new_state("/opt/camilladsp/configs/Speaker.yml");
        assert!(fresh.starts_with("config_path: /opt/camilladsp/configs/Speaker.yml\n"));
        assert_eq!(fresh.matches("- false").count(), 5);
        assert_eq!(fresh.matches("- 0.0").count(), 5);
    }

    #[test]
    fn the_state_file_names_its_configuration_without_quotes() {
        assert_eq!(
            state_config("config_path: /opt/camilladsp/configs/Flat.yml\nmute:\n- false\n"),
            Some("/opt/camilladsp/configs/Flat.yml".into())
        );
        assert_eq!(
            state_config("config_path: \"/x.yml\"\n"),
            Some("/x.yml".into())
        );
        for none in [
            "",
            "mute:\n- false\n",
            "config_path: null\n",
            "config_path: ~\n",
            "config_path:\n",
        ] {
            assert_eq!(state_config(none), None, "{none:?}");
        }
    }

    #[test]
    fn the_gui_binds_where_the_installer_says_and_nothing_else_changes() {
        let config = "---\n# kept\ncamilla_port: 1234\nbind_address: \"0.0.0.0\"   # was all interfaces\nport: 5005\n";
        assert_eq!(gui_bind(config), Some("0.0.0.0".into()));
        assert_eq!(
            with_gui_bind(config, "192.0.2.208"),
            "---\n# kept\ncamilla_port: 1234\nbind_address: \"192.0.2.208\"\nport: 5005\n"
        );
        assert_eq!(
            gui_bind("bind_address: 127.0.0.1\n"),
            Some("127.0.0.1".into())
        );
        assert_eq!(gui_bind("port: 5005\n"), None);
        // A configuration without the line gets one; a comment about it is not the line.
        assert_eq!(
            with_gui_bind("port: 5005", "127.0.0.1"),
            "port: 5005\nbind_address: \"127.0.0.1\"\n"
        );
        assert_eq!(gui_bind("# bind_address: 0.0.0.0\n"), None);
        for open in ["0.0.0.0", "::", "[::]", "*", ""] {
            assert!(wildcard(open), "{open}");
        }
        for specific in ["127.0.0.1", "192.0.2.208", "::1", "speaker.local"] {
            assert!(!wildcard(specific), "{specific}");
        }
    }

    #[test]
    fn the_management_address_comes_from_the_ports_that_carry_a_default_route() {
        let one = "default via 192.0.2.1 dev eth0 proto dhcp src 192.0.2.208 metric 100\n";
        assert_eq!(default_ports(one), ["eth0"]);
        // The Pi of HW-2: a wired and a Wi-Fi default; a port is named once.
        let two = format!(
            "{one}default via 198.51.100.1 dev wlan0 proto dhcp src 198.51.100.134 metric 600\ndefault via 192.0.2.2 dev eth0 metric 200\n"
        );
        assert_eq!(default_ports(&two), ["eth0", "wlan0"]);
        assert!(default_ports("192.0.2.0/24 dev eth0 proto kernel scope link\n").is_empty());
        assert!(default_ports("").is_empty());
        let addresses = "2: eth0    inet 192.0.2.208/24 brd 192.0.2.255 scope global dynamic noprefixroute eth0\\       valid_lft 85041sec preferred_lft 85041sec\n2: eth0    inet 192.0.2.9/24 scope global secondary eth0\\       valid_lft forever\n";
        assert_eq!(first_address(addresses), Some("192.0.2.208".into()));
        assert_eq!(
            first_address("2: eth0    inet6 fe80::1/64 scope link\n"),
            None
        );
        assert_eq!(first_address(""), None);
    }

    #[test]
    fn the_seed_fills_the_rate_the_playback_device_and_its_format() {
        let template = "samplerate: @RATE@\nplayback:\n  device: @PLAYBACK_DEVICE@\n";
        assert_eq!(
            seed(template, 96000, "hw:CARD=DigiAMP,DEV=0", DEFAULT_FORMAT),
            "samplerate: 96000\nplayback:\n  device: hw:CARD=DigiAMP,DEV=0\n"
        );
        // The format goes to the playback side only; capture keeps the network PCM's.
        let template = include_str!("../../../packaging/dsp/dist/Speaker.yml.in");
        let seeded = seed(template, 48000, "hw:CARD=DAC,DEV=0", "S16_LE");
        assert!(!seeded.contains('@'), "every marker is filled");
        assert!(seeded.contains("    device: hw:CARD=DAC,DEV=0\n    format: S16_LE\n"));
        assert!(seeded.contains("    device: tessera\n    format: S32_LE\n"));
        assert_eq!(seeded.matches("format: S16_LE").count(), 1);
    }
}

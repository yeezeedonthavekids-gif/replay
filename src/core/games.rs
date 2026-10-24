//! Decides whether the foreground application is a game.
//!
//! Order of precedence: the user's "never record" list, the user's "always record" list,
//! a bundled list of known game executables, then a heuristic (a fullscreen or borderless
//! window that is not a known non-game app). Windowed games that are not on either list are
//! not detected automatically; the user adds them once in Settings.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    UserListed,
    Known,
    Fullscreen,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameMatch {
    pub exe: String,
    pub name: String,
    pub reason: Reason,
}

/// Known game executables (lower case) and display names.
const KNOWN: &[(&str, &str)] = &[
    ("fortniteclient-win64-shipping.exe", "Fortnite"),
    ("rocketleague.exe", "Rocket League"),
    ("valorant-win64-shipping.exe", "VALORANT"),
    ("cs2.exe", "Counter-Strike 2"),
    ("csgo.exe", "Counter-Strike GO"),
    ("league of legends.exe", "League of Legends"),
    ("r5apex.exe", "Apex Legends"),
    ("r5apex_dx12.exe", "Apex Legends"),
    ("overwatch.exe", "Overwatch 2"),
    ("dota2.exe", "Dota 2"),
    ("tslgame.exe", "PUBG"),
    ("cod.exe", "Call of Duty"),
    ("modernwarfare.exe", "Call of Duty Modern Warfare"),
    ("blackopscoldwar.exe", "Call of Duty Black Ops Cold War"),
    ("rainbowsix.exe", "Rainbow Six Siege"),
    ("rainbowsix_vulkan.exe", "Rainbow Six Siege"),
    ("escapefromtarkov.exe", "Escape from Tarkov"),
    ("rustclient.exe", "Rust"),
    ("destiny2.exe", "Destiny 2"),
    ("gta5.exe", "Grand Theft Auto V"),
    ("gta5_enhanced.exe", "Grand Theft Auto V"),
    ("rdr2.exe", "Red Dead Redemption 2"),
    ("minecraft.windows.exe", "Minecraft"),
    ("robloxplayerbeta.exe", "Roblox"),
    ("eldenring.exe", "Elden Ring"),
    ("cyberpunk2077.exe", "Cyberpunk 2077"),
    ("witcher3.exe", "The Witcher 3"),
    ("bg3.exe", "Baldur's Gate 3"),
    ("bg3_dx11.exe", "Baldur's Gate 3"),
    ("palworld-win64-shipping.exe", "Palworld"),
    ("fallguys_client_game.exe", "Fall Guys"),
    ("deadbydaylight-win64-shipping.exe", "Dead by Daylight"),
    ("marvel-win64-shipping.exe", "Marvel Rivals"),
    ("discovery.exe", "THE FINALS"),
    ("huntgame.exe", "Hunt Showdown"),
    ("warframe.x64.exe", "Warframe"),
    ("pathofexile.exe", "Path of Exile"),
    ("pathofexile_x64.exe", "Path of Exile"),
    ("pathofexilesteam.exe", "Path of Exile"),
    ("diablo iv.exe", "Diablo IV"),
    ("wow.exe", "World of Warcraft"),
    ("hearthstone.exe", "Hearthstone"),
    ("sc2_x64.exe", "StarCraft II"),
    ("sotgame.exe", "Sea of Thieves"),
    ("haloinfinite.exe", "Halo Infinite"),
    ("mcc-win64-shipping.exe", "Halo MCC"),
    ("forzahorizon5.exe", "Forza Horizon 5"),
    ("forzahorizon4.exe", "Forza Horizon 4"),
    ("fsd-win64-shipping.exe", "Deep Rock Galactic"),
    ("helldivers2.exe", "Helldivers 2"),
    ("valheim.exe", "Valheim"),
    ("terraria.exe", "Terraria"),
    ("stardew valley.exe", "Stardew Valley"),
    ("among us.exe", "Among Us"),
    ("phasmophobia.exe", "Phasmophobia"),
    ("lethal company.exe", "Lethal Company"),
    ("brawlhalla.exe", "Brawlhalla"),
    ("genshinimpact.exe", "Genshin Impact"),
    ("starrail.exe", "Honkai Star Rail"),
    ("zenlesszonezero.exe", "Zenless Zone Zero"),
    ("narakabladepoint.exe", "Naraka Bladepoint"),
    ("bf2042.exe", "Battlefield 2042"),
    ("bf1.exe", "Battlefield 1"),
    ("bfv.exe", "Battlefield V"),
    ("starfield.exe", "Starfield"),
    ("fallout4.exe", "Fallout 4"),
    ("skyrimse.exe", "Skyrim"),
    ("monsterhunterwilds.exe", "Monster Hunter Wilds"),
    ("monsterhunterworld.exe", "Monster Hunter World"),
    ("monsterhunterrise.exe", "Monster Hunter Rise"),
    ("sekiro.exe", "Sekiro"),
    ("darksoulsiii.exe", "Dark Souls III"),
    ("hades2.exe", "Hades II"),
    ("hollow_knight.exe", "Hollow Knight"),
    ("celeste.exe", "Celeste"),
    ("osu!.exe", "osu!"),
    ("geometrydash.exe", "Geometry Dash"),
    ("polaris-win64-shipping.exe", "Tekken 8"),
    ("streetfighter6.exe", "Street Fighter 6"),
    ("gow.exe", "God of War"),
    ("re4.exe", "Resident Evil 4"),
    ("eafc25.exe", "EA SPORTS FC 25"),
    ("fc25.exe", "EA SPORTS FC 25"),
    ("fc24.exe", "EA SPORTS FC 24"),
    ("nba2k25.exe", "NBA 2K25"),
    ("efootball.exe", "eFootball"),
    ("portalwars-win64-shipping.exe", "Splitgate"),
    ("thespikeclient-win64-shipping.exe", "The Spike"),
];

/// Applications that commonly run fullscreen but are not games.
const NOT_GAMES: &[&str] = &[
    "explorer.exe", "chrome.exe", "msedge.exe", "firefox.exe", "opera.exe", "opera_gx.exe",
    "brave.exe", "vivaldi.exe", "iexplore.exe", "arc.exe", "vlc.exe", "mpc-hc64.exe",
    "mpc-hc.exe", "mpc-be64.exe", "potplayermini64.exe", "potplayer.exe", "mpv.exe",
    "wmplayer.exe", "microsoft.media.player.exe", "video.ui.exe", "powerpnt.exe", "teams.exe",
    "ms-teams.exe", "zoom.exe", "discord.exe", "obs64.exe", "medal.exe", "applicationframehost.exe",
    "shellexperiencehost.exe", "searchhost.exe", "searchapp.exe", "startmenuexperiencehost.exe",
    "lockapp.exe", "textinputhost.exe", "steam.exe", "steamwebhelper.exe", "epicgameslauncher.exe",
    "battle.net.exe", "eadesktop.exe", "riotclientservices.exe", "riotclientux.exe", "ubisoftconnect.exe",
    "mstsc.exe", "vmware.exe", "vmware-vmx.exe", "virtualboxvm.exe", "code.exe", "devenv.exe",
    "spotify.exe", "wwahost.exe", "photos.exe", "microsoft.photos.exe", "photosapp.exe",
    "taskmgr.exe", "replay.exe", "acrobat.exe", "acrord32.exe", "winword.exe", "excel.exe",
    "netflix.exe", "plex.exe", "kodi.exe", "nvidia app.exe", "nvidia share.exe",
    "gamebar.exe", "xboxpcapp.exe", "screenclippinghost.exe", "snippingtool.exe", "magnify.exe",
];

pub fn known_name(exe_lower: &str) -> Option<&'static str> {
    KNOWN.iter().find(|(e, _)| *e == exe_lower).map(|(_, n)| *n)
}

/// A readable name from an executable name: "RocketLeague.exe" -> "RocketLeague",
/// "FooGame-Win64-Shipping.exe" -> "FooGame".
pub fn pretty_name(exe: &str) -> String {
    let mut n = exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(exe);
    for suffix in ["-Win64-Shipping", "-win64-shipping", "_dx12", "_DX12", "-Win64", "_x64", ".x64"] {
        if let Some(s) = n.strip_suffix(suffix) {
            n = s;
        }
    }
    if n.is_empty() {
        "Game".into()
    } else {
        n.to_string()
    }
}

/// `exe` is the file name of the foreground process (any case); `fullscreen` is whether its
/// window covers its whole monitor.
pub fn classify(exe: &str, fullscreen: bool, user_games: &[String], ignored: &[String]) -> Option<GameMatch> {
    let lower = exe.to_lowercase();
    if lower.is_empty() || ignored.iter().any(|g| *g == lower) {
        return None;
    }
    let make = |name: String, reason| Some(GameMatch { exe: exe.to_string(), name, reason });
    if user_games.iter().any(|g| *g == lower) {
        let name = known_name(&lower).map(String::from).unwrap_or_else(|| pretty_name(exe));
        return make(name, Reason::UserListed);
    }
    if let Some(name) = known_name(&lower) {
        return make(name.to_string(), Reason::Known);
    }
    if fullscreen && !NOT_GAMES.contains(&lower.as_str()) {
        return make(pretty_name(exe), Reason::Fullscreen);
    }
    None
}

/// Makes a string safe to use as a Windows file or folder name.
pub fn sanitize_filename(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c })
        .collect();
    out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let out = out.trim_end_matches(['.', ' ']).to_string();
    let reserved = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "LPT1", "LPT2", "LPT3"];
    if out.is_empty() || reserved.contains(&out.to_uppercase().as_str()) {
        "Clip".into()
    } else {
        out.chars().take(80).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence() {
        let none: Vec<String> = vec![];
        let m = classify("RocketLeague.exe", false, &none, &none).unwrap();
        assert_eq!((m.name.as_str(), m.reason), ("Rocket League", Reason::Known));
        assert!(classify("RocketLeague.exe", true, &none, &["rocketleague.exe".into()]).is_none());
        assert!(classify("chrome.exe", true, &none, &none).is_none());
        let m = classify("chrome.exe", false, &["chrome.exe".into()], &none).unwrap();
        assert_eq!(m.reason, Reason::UserListed);
        let m = classify("SomeIndie-Win64-Shipping.exe", true, &none, &none).unwrap();
        assert_eq!((m.name.as_str(), m.reason), ("SomeIndie", Reason::Fullscreen));
        assert!(classify("SomeIndie.exe", false, &none, &none).is_none());
    }

    #[test]
    fn known_list_is_lowercase_and_unique() {
        for (i, (e, _)) in KNOWN.iter().enumerate() {
            assert_eq!(*e, e.to_lowercase());
            assert!(e.ends_with(".exe"));
            assert!(!KNOWN[..i].iter().any(|(o, _)| o == e), "duplicate {e}");
        }
        for e in NOT_GAMES {
            assert_eq!(*e, e.to_lowercase());
        }
    }

    #[test]
    fn filenames() {
        assert_eq!(sanitize_filename("Baldur's Gate 3"), "Baldur's Gate 3");
        assert_eq!(sanitize_filename("a/b:c*?"), "a b c");
        assert_eq!(sanitize_filename("con"), "Clip");
        assert_eq!(sanitize_filename("name. "), "name");
    }
}

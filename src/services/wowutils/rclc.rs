//! RCLootCouncil's loot history (`RCLootCouncilLootDB` in `RCLootCouncil.lua`) turned
//! into the awards the WowUtils Bridge 1.1.0 sends to a raid group's loot history.
//!
//! Every field, default and the award hash (`h`) follow the Bridge, so the site sees the
//! same rows from either app and doesn't take them for changed ones.

use crate::core::lua::{self, Lua, Table};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// Awards older than this (2024-01-01) aren't sent.
const OLDEST_SERVERTIME: i64 = 1_704_067_200;

static VALID_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+-[0-9]+$").unwrap());
static ITEM_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"item:([0-9]+)").unwrap());
static ITEM_STRING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"item:[0-9:-]+").unwrap());
static ITEM_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[(.+)\]").unwrap());
static LINK_PARTS: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [r"\|H[^|]*\|h", r"\|c[0-9a-fA-F]{8}", r"\|cn[^:]*:"].map(|p| Regex::new(p).unwrap())
});

/// A value from RCLootCouncil kept with its Lua type (session responses pass them on as is).
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

impl Scalar {
    fn from_lua(v: Option<&Lua>) -> Self {
        match v {
            Some(Lua::Bool(b)) => Scalar::Bool(*b),
            Some(Lua::Num(n)) => Scalar::Num(*n),
            Some(Lua::Str(s)) => Scalar::Str(s.clone()),
            _ => Scalar::Null,
        }
    }

    fn json(&self) -> Value {
        match self {
            Scalar::Null => Value::Null,
            Scalar::Bool(b) => Value::Bool(*b),
            Scalar::Num(n) => number(*n),
            Scalar::Str(s) => Value::String(s.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionResponse {
    pub name: String,
    pub ilvl: Scalar,
    pub class: Scalar,
    pub response: Scalar,
    pub roll: Scalar,
    pub votes: Scalar,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Award {
    pub player: String,
    pub date: String,
    pub time: String,
    pub id: String,
    pub item_id: Option<i64>,
    pub item_string: String,
    pub response: String,
    pub votes: f64,
    pub class: String,
    pub instance: String,
    pub boss: String,
    pub gear1: String,
    pub gear2: String,
    pub response_id: String,
    pub is_award_reason: String,
    pub roll_type: String,
    pub sub_type: String,
    pub equip_loc: String,
    pub note: String,
    pub owner: String,
    pub item_name: String,
    pub servertime: String,
    pub session_responses: Vec<SessionResponse>,
    /// Content hash the site keys the award's version on.
    pub h: String,
}

impl Award {
    /// The Bridge leaves out awards with an odd id, without an item, or older than 2024.
    pub fn valid(&self) -> bool {
        VALID_ID.is_match(&self.id)
            && self.item_id.is_some()
            && self.servertime().is_some_and(|t| t >= OLDEST_SERVERTIME)
    }

    pub fn servertime(&self) -> Option<i64> {
        self.servertime.parse().ok()
    }

    /// The character name without the realm; that's what the group roster is matched on.
    pub fn player_name(&self) -> &str {
        self.player.split('-').next().unwrap_or_default()
    }

    /// One entry of `/loot-history/offer`.
    pub fn offer_json(&self) -> Value {
        let mut o = json!({ "id": self.id, "h": self.h, "player": self.player });
        if !self.session_responses.is_empty() {
            o["sr"] = Value::Bool(true);
        }
        o
    }

    /// One entry of `/loot-history`.
    pub fn upload_json(&self) -> Value {
        let mut o = Map::new();
        let s = |v: &str| Value::String(v.to_string());
        o.insert("player".into(), s(&self.player));
        o.insert("date".into(), s(&self.date));
        o.insert("time".into(), s(&self.time));
        o.insert("id".into(), s(&self.id));
        o.insert("itemID".into(), self.item_id.map_or(Value::Null, Value::from));
        o.insert("itemString".into(), s(&self.item_string));
        o.insert("response".into(), s(&self.response));
        o.insert("votes".into(), number(self.votes));
        o.insert("class".into(), s(&self.class));
        o.insert("instance".into(), s(&self.instance));
        o.insert("boss".into(), s(&self.boss));
        o.insert("gear1".into(), s(&self.gear1));
        o.insert("gear2".into(), s(&self.gear2));
        o.insert("responseID".into(), s(&self.response_id));
        o.insert("isAwardReason".into(), s(&self.is_award_reason));
        o.insert("rollType".into(), s(&self.roll_type));
        o.insert("subType".into(), s(&self.sub_type));
        o.insert("equipLoc".into(), s(&self.equip_loc));
        o.insert("note".into(), s(&self.note));
        o.insert("owner".into(), s(&self.owner));
        o.insert("itemName".into(), s(&self.item_name));
        o.insert("servertime".into(), s(&self.servertime));
        o.insert("h".into(), s(&self.h));
        if !self.session_responses.is_empty() {
            let list = self
                .session_responses
                .iter()
                .map(|r| {
                    json!({
                        "name": r.name,
                        "ilvl": r.ilvl.json(),
                        "class": r.class.json(),
                        "response": r.response.json(),
                        "roll": r.roll.json(),
                        "votes": r.votes.json(),
                        "note": r.note,
                    })
                })
                .collect();
            o.insert("sessionResponses".into(), Value::Array(list));
        }
        Value::Object(o)
    }

    /// `v1` and every field the site compares, each after a 0x1F separator; SHA-256,
    /// first 16 hex digits.
    fn hash(&self) -> String {
        let item_id = self.item_id.map(|i| i.to_string()).unwrap_or_default();
        let votes = num_text(self.votes);
        let fields: [&str; 22] = [
            &self.player, &self.date, &self.time, &self.id, &item_id, &self.item_string, &self.response, &votes,
            &self.class, &self.instance, &self.boss, &self.gear1, &self.gear2, &self.response_id,
            &self.is_award_reason, &self.roll_type, &self.sub_type, &self.equip_loc, &self.note, &self.owner,
            &self.item_name, &self.servertime,
        ];
        let mut text = String::from("v1");
        for f in fields {
            text.push('\u{1f}');
            text.push_str(f);
        }
        crate::core::sha256_hex(text.as_bytes())[..16].to_string()
    }
}

/// A number the way .NET writes it: whole numbers without a fraction, others shortest.
fn num_text(v: f64) -> String {
    if is_whole(v) {
        (v as i64).to_string()
    } else {
        v.to_string()
    }
}

fn is_whole(v: f64) -> bool {
    v.is_finite() && v.abs() <= 9.2e18 && v.floor() == v
}

fn number(v: f64) -> Value {
    if is_whole(v) {
        Value::from(v as i64)
    } else {
        serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
    }
}

/// What reading one `RCLootCouncil.lua` gave.
pub enum Read {
    /// RCLootCouncil keeps no loot history in this file (not used yet on this account).
    NoHistory,
    Awards(Vec<Award>),
}

pub fn read(src: &[u8]) -> Result<Read, lua::ParseError> {
    let Some(Lua::Table(db)) = lua::read_global(src, "RCLootCouncilLootDB")? else {
        return Ok(Read::NoHistory);
    };
    let Some(Lua::Table(realms)) = db.get("factionrealm") else {
        return Ok(Read::NoHistory);
    };
    let mut out = Vec::new();
    for (_, players) in &realms.keyed {
        let Lua::Table(players) = players else { continue };
        for (player, entries) in &players.keyed {
            let (Lua::Str(player), Lua::Table(entries)) = (player, entries) else { continue };
            for e in list(entries) {
                if let Lua::Table(e) = e {
                    out.push(award(player, e));
                }
            }
        }
    }
    Ok(Read::Awards(out))
}

/// The entries of a Lua list, whether WoW wrote them positionally or as `[n] =`.
fn list(t: &Table) -> Vec<&Lua> {
    let mut keyed: Vec<(i64, &Lua)> =
        t.keyed.iter().filter_map(|(k, v)| if let Lua::Num(n) = k { Some((*n as i64, v)) } else { None }).collect();
    keyed.sort_by_key(|x| x.0);
    t.positional.iter().chain(keyed.into_iter().map(|x| x.1)).collect()
}

fn award(player: &str, e: &Table) -> Award {
    let text = |k: &str, default: &str| match e.get(k) {
        Some(Lua::Str(s)) => s.clone(),
        _ => default.to_string(),
    };
    // Numbers and booleans as text, for the fields RCLootCouncil has written both ways.
    let scalar_text = |k: &str, default: &str| match e.get(k) {
        Some(Lua::Str(s)) => s.clone(),
        Some(Lua::Num(n)) => num_text(*n),
        Some(Lua::Bool(b)) => b.to_string(),
        _ => default.to_string(),
    };
    let truthy = |k: &str| match e.get(k) {
        None | Some(Lua::Nil) => false,
        Some(Lua::Bool(b)) => *b,
        Some(_) => true,
    };
    let whole = |k: &str| match e.get(k) {
        Some(Lua::Num(n)) if is_whole(*n) => Some(*n as i64),
        _ => None,
    };
    let loot_won = text("lootWon", "");
    let id = text("id", "");
    let roll_type = if truthy("tokenRoll") {
        "token"
    } else if truthy("relicRoll") {
        "relic"
    } else {
        "normal"
    };
    let sub_type = if truthy("tierToken") {
        "Armor Token".to_string()
    } else {
        whole("iClass").zip(whole("iSubClass")).and_then(|(c, s)| sub_type_name(c, s)).unwrap_or_default().to_string()
    };
    let mut a = Award {
        player: player.to_string(),
        date: text("date", ""),
        time: text("time", ""),
        item_id: ITEM_ID.captures(&loot_won).and_then(|c| c[1].parse().ok()),
        item_string: item_string(&loot_won),
        response: text("response", ""),
        votes: match e.get("votes") {
            Some(Lua::Num(n)) => *n,
            _ => 0.0,
        },
        class: text("class", ""),
        instance: text("instance", ""),
        boss: text("boss", ""),
        gear1: gear(e.get("itemReplaced1")),
        gear2: gear(e.get("itemReplaced2")),
        response_id: scalar_text("responseID", ""),
        is_award_reason: scalar_text("isAwardReason", "false"),
        roll_type: roll_type.into(),
        sub_type,
        equip_loc: String::new(),
        note: text("note", ""),
        owner: text("owner", "Unknown"),
        item_name: ITEM_NAME.captures(&loot_won).map(|c| c[1].to_string()).unwrap_or_default(),
        servertime: id.split('-').next().unwrap_or_default().to_string(),
        id,
        session_responses: session_responses(e.get("sessionResponses")),
        h: String::new(),
    };
    a.h = a.hash();
    a
}

/// `item:…` out of an item link, without trailing empty fields.
fn item_string(link: &str) -> String {
    let Some(m) = ITEM_STRING.find(link) else { return String::new() };
    let mut s = m.as_str();
    while s.len() > 5 && s.ends_with(':') {
        s = &s[..s.len() - 1];
    }
    s.to_string()
}

/// An item link reduced to the name in brackets: `[Name]`.
fn gear(v: Option<&Lua>) -> String {
    let Some(Lua::Str(s)) = v else { return String::new() };
    let mut s = s.clone();
    for re in LINK_PARTS.iter() {
        s = re.replace_all(&s, "").into_owned();
    }
    s.replace("|h", "").replace("|r", "")
}

fn session_responses(v: Option<&Lua>) -> Vec<SessionResponse> {
    let Some(Lua::Table(t)) = v else { return Vec::new() };
    let mut out: Vec<SessionResponse> = t
        .keyed
        .iter()
        .filter_map(|(k, v)| {
            let (Lua::Str(name), Lua::Table(r)) = (k, v) else { return None };
            Some(SessionResponse {
                name: name.clone(),
                ilvl: Scalar::from_lua(r.get("ilvl")),
                class: Scalar::from_lua(r.get("class")),
                response: Scalar::from_lua(r.get("response")),
                roll: Scalar::from_lua(r.get("roll")),
                votes: Scalar::from_lua(r.get("votes")),
                note: match r.get("note") {
                    Some(Lua::Str(s)) => Some(s.clone()),
                    _ => None,
                },
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// English item subclass names by (class, subclass), as the Bridge sends them.
fn sub_type_name(class: i64, sub: i64) -> Option<&'static str> {
    const ARMOR: &[&str] = &[
        "Miscellaneous", "Cloth", "Leather", "Mail", "Plate", "Cosmetic", "Shields", "Librams", "Idols", "Totems",
        "Sigils", "Relic",
    ];
    const WEAPON: &[&str] = &[
        "One-Handed Axes", "Two-Handed Axes", "Bows", "Guns", "One-Handed Maces", "Two-Handed Maces", "Polearms",
        "One-Handed Swords", "Two-Handed Swords", "Warglaives", "Staves", "Bear Claws", "Cat Claws", "Fist Weapons",
        "Miscellaneous", "Daggers", "Thrown", "Spears", "Crossbows", "Wands", "Fishing Poles",
    ];
    const MISC: &[&str] = &["Junk", "Reagent", "Companion Pets", "Holiday", "Other", "Mount", "Mount Equipment"];
    const RECIPE: &[&str] = &[
        "Book", "Leatherworking", "Tailoring", "Engineering", "Blacksmithing", "Cooking", "Alchemy", "First Aid",
        "Enchanting", "Fishing", "Jewelcrafting", "Inscription",
    ];
    const CLASSES: &[&str] = &[
        "", "Warrior", "Paladin", "Hunter", "Rogue", "Priest", "Death Knight", "Shaman", "Mage", "Warlock", "Monk",
        "Druid", "Demon Hunter",
    ];
    if (class, sub) == (5, 2) {
        return Some("Context Token");
    }
    let table = match class {
        4 => ARMOR,
        2 => WEAPON,
        15 => MISC,
        9 => RECIPE,
        16 => CLASSES,
        _ => return None,
    };
    usize::try_from(sub).ok().and_then(|i| table.get(i)).copied().filter(|s| !s.is_empty())
}

/// Awards of several accounts by id; an award RCLootCouncil keeps on two accounts counts once.
pub fn by_id(awards: impl IntoIterator<Item = Award>) -> BTreeMap<String, Award> {
    let mut out = BTreeMap::new();
    for a in awards {
        out.entry(a.id.clone()).or_insert(a);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
RCLootCouncilLootDB = {
["factionrealm"] = {
["Alliance - Blackhand"] = {
["Smílíng-Blackmoore"] = {
{
["mapID"] = 3004,
["class"] = "DRUID",
["iSubClass"] = 2,
["boss"] = "Vashnik der Bösartige",
["time"] = "20:48:00",
["iClass"] = 4,
["itemReplaced1"] = "|cnIQ4:|Hitem:250022:8031:::::::90:1480::35:5:6652:13440:13340:13574:12806::::::|h[Samenkapseln der leuchtenden Blüte]|h|r",
["instance"] = "Der Giftige Abgrund-Normal",
["owner"] = "Pivodh-Blackhand",
["response"] = "Upgrade MIN",
["lootWon"] = "|cnIQ4:|Hitem:268246::::::::90:1480::3:4:6652:13662:13333:12835::::::|h[Schiftung des schäumenden Giftes]|h|r",
["id"] = "1787770170-39",
["date"] = "2026/08/26",
["responseID"] = 3,
["votes"] = 0,
["isAwardReason"] = false,
["sessionResponses"] = {
["Zed-Blackhand"] = { ["ilvl"] = 271.5, ["response"] = "BiS", ["note"] = "pls" },
["Abe-Blackhand"] = { ["roll"] = 42 },
},
}, -- [1]
},
},
},
}
"#;

    #[test]
    fn award_fields() {
        let Read::Awards(a) = read(SAMPLE.as_bytes()).unwrap() else { panic!("no history") };
        let a = &a[0];
        assert_eq!(a.player, "Smílíng-Blackmoore");
        assert_eq!(a.player_name(), "Smílíng");
        assert_eq!(a.item_id, Some(268246));
        assert_eq!(a.item_string, "item:268246::::::::90:1480::3:4:6652:13662:13333:12835");
        assert_eq!(a.item_name, "Schiftung des schäumenden Giftes");
        assert_eq!(a.gear1, "[Samenkapseln der leuchtenden Blüte]");
        assert_eq!(a.sub_type, "Leather");
        assert_eq!(a.response_id, "3");
        assert_eq!(a.is_award_reason, "false");
        assert_eq!(a.roll_type, "normal");
        assert_eq!(a.servertime, "1787770170");
        assert_eq!(a.owner, "Pivodh-Blackhand");
        assert!(a.valid());
        assert_eq!(a.h.len(), 16);
        let names: Vec<_> = a.session_responses.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Abe-Blackhand", "Zed-Blackhand"]);
        let j = a.upload_json();
        assert_eq!(j["votes"], 0);
        assert_eq!(j["sessionResponses"][1]["ilvl"], 271.5);
        assert_eq!(j["sessionResponses"][0]["note"], Value::Null);
        assert_eq!(a.offer_json()["sr"], true);
    }

    #[test]
    fn hash_is_stable() {
        let Read::Awards(a) = read(SAMPLE.as_bytes()).unwrap() else { panic!() };
        // Session responses don't count; any compared field does.
        let mut b = a[0].clone();
        b.session_responses.clear();
        assert_eq!(b.hash(), a[0].h);
        b.note = "x".into();
        assert_ne!(b.hash(), a[0].h);
    }

    #[test]
    fn no_history() {
        assert!(matches!(read(b"RCLootCouncilDB = {}").unwrap(), Read::NoHistory));
    }

    /// `RCLC_SV=<path to RCLootCouncil.lua> cargo test -- --ignored real_rclc_file`
    #[test]
    #[ignore]
    fn real_rclc_file() {
        let src = std::fs::read(std::env::var("RCLC_SV").unwrap()).unwrap();
        let Read::Awards(a) = read(&src).unwrap() else { panic!("no history") };
        let valid = a.iter().filter(|a| a.valid()).count();
        eprintln!("{} awards, {valid} valid; first: {}", a.len(), serde_json::to_string_pretty(&a[0].upload_json()).unwrap());
    }
}

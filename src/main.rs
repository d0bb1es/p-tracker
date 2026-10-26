use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

// ───────────────────────── settings & files ─────────────────────────

fn d_exchange() -> String {
    "bybit".into()
}
fn d_move() -> f64 {
    2.0
}
fn d_btc() -> f64 {
    2.0
}
fn d_cool() -> u64 {
    60
}
fn d_max() -> usize {
    40
}
fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct Settings {
    #[serde(default = "d_exchange")]
    exchange: String,
    /// alert when a coin moves at least this % inside a 5-minute candle
    #[serde(default = "d_move")]
    move_pct: f64,
    /// only alert if volume is at least this many times the recent average (0 = off)
    #[serde(default)]
    min_volume_x: f64,
    /// mute DIP alerts while BTC itself is dropping this % or more in 5m (0 = off)
    #[serde(default = "d_btc")]
    btc_filter_pct: f64,
    #[serde(default = "yes")]
    alert_24h_extremes: bool,
    #[serde(default = "d_cool")]
    cooldown_minutes: u64,
    #[serde(default = "d_max")]
    max_coins: usize,
    /// also post a price table on every run
    #[serde(default)]
    post_summary: bool,
    /// your own price targets, keyed by ticker (BTC, SOL ...)
    #[serde(default)]
    levels: HashMap<String, Level>,
}

#[derive(Deserialize, Default)]
struct Level {
    above: Option<f64>,
    below: Option<f64>,
}

/// The coin list + Discord cursor. Changed by "add"/"remove" messages.
#[derive(Serialize, Deserialize)]
struct CoinsFile {
    #[serde(default)]
    coins: Vec<String>,
    #[serde(default)]
    last_msg_id: String,
}

#[derive(Serialize, Deserialize, Default)]
struct CoinState {
    #[serde(default)]
    above_hit: bool,
    #[serde(default)]
    below_hit: bool,
    #[serde(default)]
    last_high_ts: u64,
    #[serde(default)]
    last_low_ts: u64,
    #[serde(default)]
    last_move_candle: u64,
}

type State = BTreeMap<String, CoinState>;

// ───────────────────────── exchange data ─────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Ex {
    Bybit,
    Okx,
}

impl Ex {
    fn name(self) -> &'static str {
        match self {
            Ex::Bybit => "Bybit",
            Ex::Okx => "OKX",
        }
    }
}

struct Ticker {
    last: f64,
    high: f64,
    low: f64,
    open: f64,
}

#[derive(Clone, Copy)]
struct Candle {
    ts: u64,
    open: f64,
    close: f64,
    vol: f64,
}

fn num(v: &Value) -> f64 {
    v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0)
}

fn get_json(c: &Client, url: &str) -> Result<Value> {
    let r = c.get(url).send()?;
    let status = r.status();
    if !status.is_success() {
        bail!("HTTP {} from {}", status, url);
    }
    Ok(r.json()?)
}

fn fetch_tickers(c: &Client, ex: Ex) -> Result<HashMap<String, Ticker>> {
    let mut out = HashMap::new();
    match ex {
        Ex::Bybit => {
            let v = get_json(c, "https://api.bybit.com/v5/market/tickers?category=spot")?;
            let list = v["result"]["list"]
                .as_array()
                .ok_or_else(|| anyhow!("unexpected Bybit response"))?;
            for t in list {
                let Some(sym) = t["symbol"].as_str() else { continue };
                let Some(base) = sym.strip_suffix("USDT") else { continue };
                let last = num(&t["lastPrice"]);
                if last > 0.0 {
                    out.insert(
                        base.to_string(),
                        Ticker {
                            last,
                            high: num(&t["highPrice24h"]),
                            low: num(&t["lowPrice24h"]),
                            open: num(&t["prevPrice24h"]),
                        },
                    );
                }
            }
        }
        Ex::Okx => {
            let v = get_json(c, "https://www.okx.com/api/v5/market/tickers?instType=SPOT")?;
            let list = v["data"]
                .as_array()
                .ok_or_else(|| anyhow!("unexpected OKX response"))?;
            for t in list {
                let Some(id) = t["instId"].as_str() else { continue };
                let Some(base) = id.strip_suffix("-USDT") else { continue };
                let last = num(&t["last"]);
                if last > 0.0 {
                    out.insert(
                        base.to_string(),
                        Ticker {
                            last,
                            high: num(&t["high24h"]),
                            low: num(&t["low24h"]),
                            open: num(&t["open24h"]),
                        },
                    );
                }
            }
        }
    }
    if out.is_empty() {
        bail!("no USDT spot tickers returned");
    }
    Ok(out)
}

/// 5-minute candles, NEWEST FIRST. Index 0 is the candle still forming.
fn fetch_candles(c: &Client, ex: Ex, base: &str) -> Result<Vec<Candle>> {
    let (url, key) = match ex {
        Ex::Bybit => (
            format!("https://api.bybit.com/v5/market/kline?category=spot&symbol={}USDT&interval=5&limit=14", base),
            "list",
        ),
        Ex::Okx => (
            format!("https://www.okx.com/api/v5/market/candles?instId={}-USDT&bar=5m&limit=14", base),
            "data",
        ),
    };
    let v = get_json(c, &url)?;
    let arr = if ex == Ex::Bybit { &v["result"][key] } else { &v[key] };
    let list = arr.as_array().ok_or_else(|| anyhow!("unexpected candle response"))?;
    // both exchanges: [time, open, high, low, close, volume, ...]
    Ok(list
        .iter()
        .map(|k| Candle {
            ts: num(&k[0]) as u64,
            open: num(&k[1]),
            close: num(&k[4]),
            vol: num(&k[5]),
        })
        .collect())
}

/// (move inside the forming candle, move of the last closed candle), in %
fn pcts(c: &[Candle], last: f64) -> Option<(f64, f64)> {
    if c.len() < 3 || c[0].open <= 0.0 || c[1].open <= 0.0 {
        return None;
    }
    Some((
        (last / c[0].open - 1.0) * 100.0,
        (c[1].close / c[1].open - 1.0) * 100.0,
    ))
}

// ───────────────────────── discord commands ─────────────────────────

#[derive(Deserialize)]
struct Msg {
    id: String,
    #[serde(default)]
    content: String,
    author: Author,
}

#[derive(Deserialize)]
struct Author {
    #[serde(default)]
    bot: bool,
}

enum Cmd {
    Add(Vec<String>),
    Remove(Vec<String>),
    List,
}

fn clean_ticker(s: &str) -> Option<String> {
    let mut t = s.trim_start_matches('$').to_uppercase();
    if let Some(x) = t.strip_suffix("USDT") {
        if !x.is_empty() {
            t = x.to_string();
        }
    }
    (t.len() >= 2 && t.len() <= 15 && t.chars().all(|c| c.is_ascii_alphanumeric())).then_some(t)
}

fn parse_cmd(text: &str) -> Option<Cmd> {
    let t = text.trim().trim_start_matches(|c| c == '!' || c == '/' || c == '.');
    let mut it = t
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty());
    let cmd = it.next()?.to_lowercase();
    let syms: Vec<String> = it.filter_map(clean_ticker).collect();
    match cmd.as_str() {
        "add" | "+" => (!syms.is_empty()).then(|| Cmd::Add(syms)),
        "remove" | "rm" | "del" | "delete" | "-" => (!syms.is_empty()).then(|| Cmd::Remove(syms)),
        "list" | "coins" => Some(Cmd::List),
        _ => None,
    }
}

fn run_commands(
    client: &Client,
    token: &str,
    channel: &str,
    cf: &mut CoinsFile,
    market: &HashMap<String, Ticker>,
    max: usize,
    ex_name: &str,
) -> Result<()> {
    let mut url = format!("https://discord.com/api/v10/channels/{}/messages?limit=100", channel);
    if !cf.last_msg_id.is_empty() {
        url.push_str(&format!("&after={}", cf.last_msg_id));
    }
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bot {}", token))
        .send()?;
    if !resp.status().is_success() {
        bail!(
            "Discord returned HTTP {} when reading the commands channel (check the bot token, channel id and bot permissions)",
            resp.status()
        );
    }
    let mut msgs: Vec<Msg> = resp.json()?;
    if msgs.is_empty() {
        return Ok(());
    }
    msgs.sort_by_key(|m| m.id.parse::<u64>().unwrap_or(0));
    if let Some(last) = msgs.last() {
        cf.last_msg_id = last.id.clone();
    }

    let (mut added, mut removed, mut already, mut missing, mut full, mut not_in) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    let (mut handled, mut silent) = (false, 0);

    for m in &msgs {
        if m.author.bot {
            continue;
        }
        if m.content.trim().is_empty() {
            silent += 1;
            continue;
        }
        match parse_cmd(&m.content) {
            Some(Cmd::Add(v)) => {
                handled = true;
                for s in v {
                    if cf.coins.contains(&s) {
                        already.push(s);
                    } else if !market.contains_key(&s) {
                        missing.push(s);
                    } else if cf.coins.len() >= max {
                        full.push(s);
                    } else {
                        cf.coins.push(s.clone());
                        added.push(s);
                    }
                }
            }
            Some(Cmd::Remove(v)) => {
                handled = true;
                for s in v {
                    if let Some(pos) = cf.coins.iter().position(|c| *c == s) {
                        cf.coins.remove(pos);
                        removed.push(s);
                    } else {
                        not_in.push(s);
                    }
                }
            }
            Some(Cmd::List) => handled = true,
            None => {}
        }
    }

    let mut lines: Vec<String> = Vec::new();
    if !added.is_empty() {
        lines.push(format!("✅ Added: {}", added.join(", ")));
    }
    if !removed.is_empty() {
        lines.push(format!("➖ Removed: {}", removed.join(", ")));
    }
    if !already.is_empty() {
        lines.push(format!("ℹ️ Already watching: {}", already.join(", ")));
    }
    if !not_in.is_empty() {
        lines.push(format!("ℹ️ Not in the list: {}", not_in.join(", ")));
    }
    if !missing.is_empty() {
        lines.push(format!(
            "❌ Not found on {} spot (USDT): {}",
            ex_name,
            missing.join(", ")
        ));
    }
    if !full.is_empty() {
        lines.push(format!("🚫 List is full ({} coins max): {}", max, full.join(", ")));
    }
    if silent > 0 && !handled {
        lines.push("⚠️ I can see messages here but not their text. In the Discord Developer Portal open your app → Bot → turn on **Message Content Intent**.".into());
    }
    if handled {
        lines.push(format!("📋 Watching ({}): {}", cf.coins.len(), cf.coins.join(", ")));
    }

    if !lines.is_empty() {
        let r = client
            .post(format!("https://discord.com/api/v10/channels/{}/messages", channel))
            .header("Authorization", format!("Bot {}", token))
            .json(&json!({ "content": lines.join("\n"), "allowed_mentions": { "parse": [] } }))
            .send()?;
        if !r.status().is_success() {
            eprintln!("could not reply in the commands channel: HTTP {} (does the bot have Send Messages?)", r.status());
        }
    }
    Ok(())
}

// ───────────────────────── helpers ─────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fmt_price(p: f64) -> String {
    if p >= 1.0 {
        let s = format!("{:.2}", p);
        let (int, dec) = s.split_once('.').unwrap();
        let mut out = String::new();
        for (i, c) in int.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        format!("${}.{}", out.chars().rev().collect::<String>(), dec)
    } else {
        format!("${:.6}", p)
    }
}

// ───────────────────────── main ─────────────────────────

fn main() -> Result<()> {
    let webhook = env::var("DISCORD_WEBHOOK").context("DISCORD_WEBHOOK env var not set")?;
    let token = env::var("DISCORD_BOT_TOKEN").unwrap_or_default();
    let channel = env::var("DISCORD_COMMANDS_CHANNEL_ID").unwrap_or_default();

    let s: Settings = serde_json::from_str(
        &fs::read_to_string("watchlist.json").context("cannot read watchlist.json")?,
    )
    .context("invalid watchlist.json")?;
    let levels: HashMap<String, (Option<f64>, Option<f64>)> = s
        .levels
        .iter()
        .map(|(k, v)| (k.to_uppercase(), (v.above, v.below)))
        .collect();

    let old_coins_raw = fs::read_to_string("coins.json").unwrap_or_default();
    let mut cf: CoinsFile = serde_json::from_str(&old_coins_raw).unwrap_or(CoinsFile {
        coins: vec!["BTC".into(), "ETH".into(), "SOL".into()],
        last_msg_id: String::new(),
    });
    let old_state_raw = fs::read_to_string("state.json").unwrap_or_default();
    let mut state: State = serde_json::from_str(&old_state_raw).unwrap_or_default();

    let now = now_secs();
    let cool = s.cooldown_minutes * 60;

    let client = Client::builder()
        .user_agent("price-tracker/0.3")
        .timeout(std::time::Duration::from_secs(20))
        .build()?;

    // 1) market data (preferred exchange first, the other as fallback)
    let order = if s.exchange.eq_ignore_ascii_case("okx") {
        [Ex::Okx, Ex::Bybit]
    } else {
        [Ex::Bybit, Ex::Okx]
    };
    let mut errs = Vec::new();
    let mut found = None;
    for ex in order {
        match fetch_tickers(&client, ex) {
            Ok(m) => {
                found = Some((ex, m));
                break;
            }
            Err(e) => errs.push(format!("{}: {}", ex.name(), e)),
        }
    }
    let (ex, market) =
        found.ok_or_else(|| anyhow!("could not reach any exchange API -> {}", errs.join(" | ")))?;
    println!("data source: {} ({} spot pairs)", ex.name(), market.len());

    // 2) add / remove coins from the Discord commands channel
    if !token.is_empty() && !channel.is_empty() {
        if let Err(e) = run_commands(&client, &token, &channel, &mut cf, &market, s.max_coins, ex.name()) {
            eprintln!("commands channel problem: {:#}", e);
        }
        let new_coins_raw = serde_json::to_string_pretty(&cf)? + "\n";
        if new_coins_raw.trim() != old_coins_raw.trim() {
            fs::write("coins.json", new_coins_raw)?; // saved now so commands are never replayed
        }
    } else {
        println!("no bot token / channel id set: Discord commands are off");
    }

    // 3) BTC as the market compass
    let btc: Option<(f64, f64)> = market
        .get("BTC")
        .and_then(|t| fetch_candles(&client, ex, "BTC").ok().and_then(|c| pcts(&c, t.last)));
    let btc_worst = btc.map(|b| b.0.min(b.1)).unwrap_or(0.0);

    // 4) scan every watched coin
    let sym_up = "USDT";
    let mut fields = Vec::new();
    let mut alerts: Vec<String> = Vec::new();
    let mut muted = 0;

    for base in &cf.coins {
        let Some(t) = market.get(base) else { continue };
        let st = state.entry(base.clone()).or_default();
        let price = fmt_price(t.last);
        let change24 = if t.open > 0.0 { (t.last / t.open - 1.0) * 100.0 } else { 0.0 };

        // your own target levels: once per crossing, re-armed when price returns
        if let Some((above, below)) = levels.get(base) {
            if let Some(a) = above {
                if t.last >= *a {
                    if !st.above_hit {
                        alerts.push(format!("🎯 **{}** crossed above `{}` — now **{}**", base, fmt_price(*a), price));
                        st.above_hit = true;
                    }
                } else {
                    st.above_hit = false;
                }
            }
            if let Some(b) = below {
                if t.last <= *b {
                    if !st.below_hit {
                        alerts.push(format!("🎯 **{}** dropped below `{}` — now **{}**", base, fmt_price(*b), price));
                        st.below_hit = true;
                    }
                } else {
                    st.below_hit = false;
                }
            }
        }

        // rolling 24h high / low
        if s.alert_24h_extremes {
            if t.high > 0.0 && t.last >= t.high * 0.9995 && now.saturating_sub(st.last_high_ts) >= cool {
                alerts.push(format!("📈 **{}** is at its 24h high — **{}**", base, price));
                st.last_high_ts = now;
            }
            if t.low > 0.0 && t.last <= t.low * 1.0005 && now.saturating_sub(st.last_low_ts) >= cool {
                alerts.push(format!("📉 **{}** is at its 24h low — **{}**", base, price));
                st.last_low_ts = now;
            }
        }

        // the 5-minute move
        match fetch_candles(&client, ex, base) {
            Ok(cd) => {
                if let Some((cur, prev)) = pcts(&cd, t.last) {
                    let (chg, idx, btc_chg) = if cur.abs() >= prev.abs() {
                        (cur, 0, btc.map(|b| b.0))
                    } else {
                        (prev, 1, btc.map(|b| b.1))
                    };
                    let candle = cd[idx];
                    let older: Vec<f64> = cd.iter().skip(2).map(|x| x.vol).collect();
                    let avg = if older.is_empty() { 0.0 } else { older.iter().sum::<f64>() / older.len() as f64 };
                    let vol_x = if avg > 0.0 { candle.vol / avg } else { 0.0 };
                    let prior = if cd.len() >= 13 && cd[12].close > 0.0 {
                        Some((cd[2].close / cd[12].close - 1.0) * 100.0)
                    } else {
                        None
                    };

                    let enough_volume = s.min_volume_x <= 0.0 || vol_x >= s.min_volume_x;
                    if chg.abs() >= s.move_pct && st.last_move_candle != candle.ts && enough_volume {
                        st.last_move_candle = candle.ts;
                        let market_selloff =
                            chg < 0.0 && base != "BTC" && s.btc_filter_pct > 0.0 && btc_worst <= -s.btc_filter_pct;
                        if market_selloff {
                            muted += 1;
                        } else {
                            let icon = if chg >= 0.0 { "🚀" } else { "🔻" };
                            let mut line = format!(
                                "{} **{}** `{:+.2}%` in 5m — **{}**\n↳ volume `{:.1}x`",
                                icon, base, chg, price, vol_x
                            );
                            if let Some(p) = prior {
                                line.push_str(&format!(" · prior 1h `{:+.1}%`", p));
                            }
                            if base != "BTC" {
                                if let Some(b) = btc_chg {
                                    line.push_str(&format!(" · vs BTC `{:+.2}%`", chg - b));
                                }
                            }
                            if t.high > 0.0 {
                                line.push_str(&format!(" · from 24h high `{:+.1}%`", (t.last / t.high - 1.0) * 100.0));
                            }
                            if chg < 0.0 && prior.map_or(false, |p| p > 0.5) {
                                line.push_str("\n↳ 📍 dip inside an uptrend — possible spot entry, check the chart first");
                            }
                            alerts.push(line);
                        }
                    }
                }
            }
            Err(e) => eprintln!("{}: could not load candles: {:#}", base, e),
        }

        fields.push(json!({
            "name": format!("{}/{}", base, sym_up),
            "value": format!("**{}**\n{} 24h `{:+.2}%`", price, if change24 >= 0.0 { "🟢" } else { "🔴" }, change24),
            "inline": true
        }));
    }

    if muted > 0 {
        println!("muted {} dip alert(s): BTC is falling too ({:+.2}%)", muted, btc_worst);
    }

    // 5) post
    let mut embeds = Vec::new();
    if !alerts.is_empty() {
        embeds.push(json!({
            "title": "🔔 Price Alert",
            "description": alerts.join("\n"),
            "color": 0xF1C40F,
            "footer": { "text": format!("Data: {} spot", ex.name()) }
        }));
    }
    if s.post_summary && !fields.is_empty() {
        for (i, chunk) in fields.chunks(24).enumerate() {
            let mut e = json!({ "fields": chunk, "color": 0x5865F2 });
            if i == 0 {
                e["title"] = json!("📈 Crypto Prices");
            }
            embeds.push(e);
        }
    }

    if embeds.is_empty() {
        println!("no alerts, nothing posted");
    } else {
        client
            .post(&webhook)
            .json(&json!({ "embeds": embeds }))
            .send()?
            .error_for_status()
            .context("Discord webhook rejected the message")?;
        println!("posted ({} alerts)", alerts.len());
    }

    // saved only after a successful post, so a failed post is retried next run
    let new_state_raw = serde_json::to_string_pretty(&state)? + "\n";
    if new_state_raw.trim() != old_state_raw.trim() {
        fs::write("state.json", new_state_raw)?;
    }
    Ok(())
}

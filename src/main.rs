use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::{collections::HashMap, env, fs};

#[derive(Deserialize)]
struct Watchlist {
    #[serde(default = "default_currency")]
    currency: String,
    /// Flag a coin when |1h change| >= this percentage (optional)
    alert_1h_pct: Option<f64>,
    /// CoinGecko coin ids, e.g. "bitcoin", "ethereum"
    coins: Vec<String>,
}

fn default_currency() -> String {
    "usd".into()
}

#[derive(Deserialize)]
struct Market {
    id: String,
    symbol: String,
    name: String,
    current_price: Option<f64>,
    high_24h: Option<f64>,
    low_24h: Option<f64>,
    price_change_percentage_1h_in_currency: Option<f64>,
    price_change_percentage_24h_in_currency: Option<f64>,
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
        format!("{}.{}", out.chars().rev().collect::<String>(), dec)
    } else {
        format!("{:.6}", p)
    }
}

fn arrow(pct: f64) -> &'static str {
    if pct >= 0.0 { "🟢" } else { "🔴" }
}

fn main() -> Result<()> {
    let webhook = env::var("DISCORD_WEBHOOK").context("DISCORD_WEBHOOK env var not set")?;
    let wl: Watchlist = serde_json::from_str(
        &fs::read_to_string("watchlist.json").context("cannot read watchlist.json")?,
    )
    .context("invalid watchlist.json")?;
    if wl.coins.is_empty() {
        bail!("watchlist is empty");
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent("price-tracker/0.1")
        .timeout(std::time::Duration::from_secs(20))
        .build()?;

    // One request for all coins (CoinGecko free tier: ~30 calls/min, no key needed)
    let mut req = client
        .get("https://api.coingecko.com/api/v3/coins/markets")
        .query(&[
            ("vs_currency", wl.currency.as_str()),
            ("ids", wl.coins.join(",").as_str()),
            ("price_change_percentage", "1h,24h"),
            ("per_page", "250"),
        ]);
    if let Ok(key) = env::var("COINGECKO_API_KEY") {
        if !key.is_empty() {
            req = req.header("x-cg-demo-api-key", key);
        }
    }
    let markets: Vec<Market> = req.send()?.error_for_status()?.json()?;
    let by_id: HashMap<&str, &Market> = markets.iter().map(|m| (m.id.as_str(), m)).collect();

    let sym = wl.currency.to_uppercase();
    let mut fields = Vec::new();
    let mut alerts = Vec::new();

    for id in &wl.coins {
        let Some(m) = by_id.get(id.as_str()) else { continue };
        let Some(price) = m.current_price else { continue };
        let c1 = m.price_change_percentage_1h_in_currency.unwrap_or(0.0);
        let c24 = m.price_change_percentage_24h_in_currency.unwrap_or(0.0);

        let mut value = format!(
            "**{} {}**\n{} 1h `{:+.2}%`\n{} 24h `{:+.2}%`",
            fmt_price(price), sym, arrow(c1), c1, arrow(c24), c24
        );
        if let (Some(h), Some(l)) = (m.high_24h, m.low_24h) {
            value.push_str(&format!("\nH `{}` / L `{}`", fmt_price(h), fmt_price(l)));
        }
        fields.push(json!({
            "name": format!("{} ({})", m.name, m.symbol.to_uppercase()),
            "value": value,
            "inline": true
        }));

        if let Some(th) = wl.alert_1h_pct {
            if c1.abs() >= th {
                alerts.push(format!("🚨 **{}** moved `{:+.2}%` in 1h", m.name, c1));
            }
        }
    }

    if fields.is_empty() {
        bail!("no matching coins returned — check the ids in watchlist.json");
    }

    // Discord: max 25 fields per embed, max 10 embeds per message
    let embeds: Vec<_> = fields
        .chunks(24)
        .enumerate()
        .map(|(i, chunk)| {
            let mut e = json!({
                "fields": chunk,
                "color": 0x5865F2,
                "footer": { "text": "Data: CoinGecko" }
            });
            if i == 0 {
                e["title"] = json!("📈 Crypto Prices");
            }
            e
        })
        .collect();

    let mut payload = json!({ "embeds": embeds });
    if !alerts.is_empty() {
        payload["content"] = json!(alerts.join("\n"));
    }

    client
        .post(&webhook)
        .json(&payload)
        .send()?
        .error_for_status()
        .context("Discord webhook rejected the message")?;

    println!("posted {} coins, {} alerts", fields.len(), alerts.len());
    Ok(())
}

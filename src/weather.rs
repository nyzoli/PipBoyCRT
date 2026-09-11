//! WEATHER forrás: Open-Meteo lekérés tokio taskban, 15 percenként.
use crate::modules::weather::WeatherCfg;
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Deserializer};
use std::sync::mpsc::Sender;
use std::time::Duration;
use stream_download::http::reqwest;
use tokio::runtime::Handle;

#[derive(Clone, Debug)]
pub struct HourPoint {
    pub hour: u8,
    pub temp: f32,
    /// Csapadék-valószínűség %-ban.
    pub precip: u8,
    pub code: u8,
    pub wind_kmh: f32,
    pub precip_mm: f32,
}

#[derive(Clone, Debug)]
pub struct DayPoint {
    pub date: chrono::NaiveDate,
    pub tmax: f32,
    pub tmin: f32,
    pub code: u8,
    pub precip_pct: u8,
    pub precip_mm: f32,
    pub wind_max: f32,
    pub uv_max: f32,
}

/// Levegőminőség (Open-Meteo air-quality). Hiányzó pollenfaj kimarad a listából.
#[derive(Clone, Debug, Default)]
pub struct AirSnapshot {
    /// European AQI (EAQI).
    pub aqi: u16,
    pub pm10: f32,
    pub pm2_5: f32,
    pub ozone: f32,
    pub no2: f32,
    /// (faj, szem/m³), csökkenő sorrendben.
    pub pollen: Vec<(&'static str, f32)>,
}

/// EAQI sáv neve: 0–20 good, 20–40 fair, 40–60 moderate, 60–80 poor,
/// 80–100 very poor, e fölött extremely poor.
pub fn aqi_band(aqi: u16) -> &'static str {
    match aqi {
        0..=20 => "good",
        21..=40 => "fair",
        41..=60 => "moderate",
        61..=80 => "poor",
        81..=100 => "very poor",
        _ => "extremely poor",
    }
}

/// Pollenszint szem/m³ alapján, általános (fajfüggetlen) küszöbökkel:
/// 0 none, 1–20 low, 21–80 moderate, 81–200 high, e fölött very high.
pub fn pollen_level(grains: f32) -> &'static str {
    match grains {
        g if g <= 0.0 => "none",
        g if g <= 20.0 => "low",
        g if g <= 80.0 => "moderate",
        g if g <= 200.0 => "high",
        _ => "very high",
    }
}

#[derive(Clone, Debug)]
pub struct WeatherSnapshot {
    pub place: String,
    pub fetched_at: chrono::DateTime<chrono::Local>,
    pub temp: f32,
    pub feels: f32,
    pub humidity: u8,
    pub wind_kmh: f32,
    pub wind_dir: u16,
    pub pressure: f32,
    pub code: u8,
    pub uv: Option<f32>,
    pub is_day: bool,
    pub clouds: u8,
    /// Aktuális csapadék mm-ben.
    pub precip_mm: f32,
    pub sunrise: String,
    pub sunset: String,
    pub hourly: Vec<HourPoint>,
    pub daily: Vec<DayPoint>,
    pub air: Option<AirSnapshot>,
}

impl WeatherSnapshot {
    pub fn is_stale(&self) -> bool {
        chrono::Local::now() - self.fetched_at > chrono::Duration::minutes(45)
    }
}

const REFRESH: u64 = 15 * 60;
const BACKOFF_MIN: u64 = 30;
const BACKOFF_MAX: u64 = 8 * 60;

#[derive(Deserialize)]
struct Api {
    current: Current,
    hourly: Hourly,
    daily: Daily,
}

#[derive(Deserialize)]
struct Current {
    temperature_2m: f32,
    apparent_temperature: f32,
    relative_humidity_2m: f32,
    wind_speed_10m: f32,
    wind_direction_10m: f32,
    surface_pressure: f32,
    weather_code: u8,
    uv_index: Option<f32>,
    #[serde(default)]
    is_day: Option<u8>,
    #[serde(default)]
    cloud_cover: Option<f32>,
    #[serde(default)]
    precipitation: Option<f32>,
}

#[derive(Deserialize)]
struct Hourly {
    time: Vec<String>,
    /// A modellhorizont szélén `null` lehet (Open-Meteo).
    temperature_2m: Vec<Option<f32>>,
    precipitation_probability: Vec<Option<f32>>,
    #[serde(default)]
    precipitation: Vec<Option<f32>>,
    #[serde(default)]
    weather_code: Vec<Option<u8>>,
    #[serde(default)]
    wind_speed_10m: Vec<Option<f32>>,
}

#[derive(Deserialize)]
struct Daily {
    time: Vec<String>,
    /// `null` lehet a modellhorizont szélén.
    temperature_2m_max: Vec<Option<f32>>,
    temperature_2m_min: Vec<Option<f32>>,
    weather_code: Vec<Option<u8>>,
    /// `null` sarkkörön túl, éjféli napnál.
    sunrise: Vec<Option<String>>,
    sunset: Vec<Option<String>>,
    #[serde(default)]
    precipitation_probability_max: Vec<Option<f32>>,
    #[serde(default)]
    precipitation_sum: Vec<Option<f32>>,
    #[serde(default)]
    wind_speed_10m_max: Vec<Option<f32>>,
    #[serde(default)]
    uv_index_max: Vec<Option<f32>>,
}

/// Megkülönbözteti a hiányzó mezőt (`None`) a `null`-tól (`Some(None)`).
fn present_or_null<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<f32>>, D::Error> {
    Option::<f32>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct AirApi {
    current: AirCurrent,
}

#[derive(Deserialize)]
struct AirCurrent {
    european_aqi: Option<f32>,
    pm10: Option<f32>,
    pm2_5: Option<f32>,
    ozone: Option<f32>,
    nitrogen_dioxide: Option<f32>,
    #[serde(default, deserialize_with = "present_or_null")]
    alder_pollen: Option<Option<f32>>,
    #[serde(default, deserialize_with = "present_or_null")]
    birch_pollen: Option<Option<f32>>,
    #[serde(default, deserialize_with = "present_or_null")]
    grass_pollen: Option<Option<f32>>,
    #[serde(default, deserialize_with = "present_or_null")]
    mugwort_pollen: Option<Option<f32>>,
    #[serde(default, deserialize_with = "present_or_null")]
    olive_pollen: Option<Option<f32>>,
    #[serde(default, deserialize_with = "present_or_null")]
    ragweed_pollen: Option<Option<f32>>,
}

pub fn air_url(cfg: &WeatherCfg) -> String {
    format!(
        "https://air-quality-api.open-meteo.com/v1/air-quality?latitude={}&longitude={}\
         &current=european_aqi,pm10,pm2_5,ozone,nitrogen_dioxide,alder_pollen,birch_pollen,grass_pollen,mugwort_pollen,olive_pollen,ragweed_pollen&timezone=auto",
        cfg.lat, cfg.lon
    )
}

pub fn parse_air(json: &str) -> Result<AirSnapshot, String> {
    let a: AirApi = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let c = a.current;
    // Hiányzó faj kimarad, `null` → 0.
    let mut pollen: Vec<(&'static str, f32)> = [
        ("alder", c.alder_pollen),
        ("birch", c.birch_pollen),
        ("grass", c.grass_pollen),
        ("mugwort", c.mugwort_pollen),
        ("olive", c.olive_pollen),
        ("ragweed", c.ragweed_pollen),
    ]
    .into_iter()
    .filter_map(|(n, v)| v.map(|v| (n, v.unwrap_or(0.0).max(0.0))))
    .collect();
    pollen.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(AirSnapshot {
        aqi: c.european_aqi.unwrap_or(0.0).clamp(0.0, 1000.0) as u16,
        pm10: c.pm10.unwrap_or(0.0),
        pm2_5: c.pm2_5.unwrap_or(0.0),
        ozone: c.ozone.unwrap_or(0.0),
        no2: c.nitrogen_dioxide.unwrap_or(0.0),
        pollen,
    })
}

pub fn url(cfg: &WeatherCfg) -> String {
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}\
         &current=temperature_2m,apparent_temperature,relative_humidity_2m,wind_speed_10m,wind_direction_10m,surface_pressure,weather_code,uv_index,is_day,cloud_cover,precipitation\
         &hourly=temperature_2m,precipitation_probability,precipitation,weather_code,wind_speed_10m&forecast_hours=24\
         &daily=temperature_2m_max,temperature_2m_min,weather_code,sunrise,sunset,precipitation_probability_max,precipitation_sum,wind_speed_10m_max,uv_index_max&forecast_days=7&timezone=auto",
        cfg.lat, cfg.lon
    )
}

/// "2026-09-10T06:12" → "06:12"
fn hhmm(s: &str) -> String {
    s.get(11..16).unwrap_or("--:--").to_string()
}

pub fn parse(json: &str, place: &str) -> Result<WeatherSnapshot, String> {
    let a: Api = serde_json::from_str(json).map_err(|e| e.to_string())?;
    // Az opcionális órás sorok hiányozhatnak vagy rövidebbek lehetnek: index szerint, 0 alapértelmezéssel.
    let at = |v: &Vec<Option<f32>>, i: usize| v.get(i).copied().flatten().unwrap_or(0.0);
    let hourly = a
        .hourly
        .time
        .iter()
        .enumerate()
        .map(|(i, t)| HourPoint {
            hour: t.get(11..13).and_then(|h| h.parse().ok()).unwrap_or(0),
            temp: at(&a.hourly.temperature_2m, i),
            precip: at(&a.hourly.precipitation_probability, i).clamp(0.0, 100.0) as u8,
            code: a.hourly.weather_code.get(i).copied().flatten().unwrap_or(255),
            wind_kmh: at(&a.hourly.wind_speed_10m, i),
            precip_mm: at(&a.hourly.precipitation, i),
        })
        .collect();
    let mut daily = Vec::new();
    for (i, d) in a.daily.time.iter().enumerate() {
        daily.push(DayPoint {
            date: NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|e| e.to_string())?,
            tmax: at(&a.daily.temperature_2m_max, i),
            tmin: at(&a.daily.temperature_2m_min, i),
            code: a.daily.weather_code.get(i).copied().flatten().unwrap_or(255),
            precip_pct: at(&a.daily.precipitation_probability_max, i).clamp(0.0, 100.0) as u8,
            precip_mm: at(&a.daily.precipitation_sum, i),
            wind_max: at(&a.daily.wind_speed_10m_max, i),
            uv_max: at(&a.daily.uv_index_max, i),
        });
    }
    let c = a.current;
    Ok(WeatherSnapshot {
        place: place.to_string(),
        fetched_at: Local::now(),
        temp: c.temperature_2m,
        feels: c.apparent_temperature,
        humidity: c.relative_humidity_2m.round() as u8,
        wind_kmh: c.wind_speed_10m,
        wind_dir: c.wind_direction_10m.round() as u16 % 360,
        pressure: c.surface_pressure,
        code: c.weather_code,
        uv: c.uv_index,
        is_day: c.is_day.unwrap_or(1) != 0,
        clouds: c.cloud_cover.unwrap_or(0.0).clamp(0.0, 100.0) as u8,
        precip_mm: c.precipitation.unwrap_or(0.0),
        sunrise: a.daily.sunrise.first().and_then(|s| s.as_deref()).map(hhmm).unwrap_or_else(|| "--:--".into()),
        sunset: a.daily.sunset.first().and_then(|s| s.as_deref()).map(hhmm).unwrap_or_else(|| "--:--".into()),
        hourly,
        daily,
        air: None,
    })
}

/// Tokio task: 15 percenként lekér, hibánál 30 s → 8 perc backoff. A visszaadott csatornán
/// egy `()` azonnali frissítést kér.
pub fn spawn(handle: &Handle, cfg: WeatherCfg, tx: Sender<Result<WeatherSnapshot, String>>) -> tokio::sync::mpsc::Sender<()> {
    let (ctx, mut crx) = tokio::sync::mpsc::channel::<()>(1);
    handle.spawn(async move {
        let client = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(format!("http client: {e}")));
                return;
            }
        };
        let (url, air) = (url(&cfg), air_url(&cfg));
        let mut backoff = BACKOFF_MIN;
        loop {
            let res = fetch(&client, &url, &air, &cfg.name).await;
            let wait = if res.is_ok() {
                backoff = BACKOFF_MIN;
                REFRESH
            } else {
                let w = backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                w
            };
            if tx.send(res).is_err() {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_secs(wait), crx.recv()).await;
        }
    });
    ctx
}

async fn get(client: &reqwest::Client, url: &str) -> Result<String, String> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())
}

async fn fetch(client: &reqwest::Client, url: &str, air: &str, place: &str) -> Result<WeatherSnapshot, String> {
    let mut w = parse(&get(client, url).await?, place)?;
    // A levegőminőség külön végpont: ha elhasal, az időjárás attól még látszik.
    w.air = get(client, air).await.ok().and_then(|b| parse_air(&b).ok());
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "current":{"time":"2026-09-10T09:15","temperature_2m":22.1,"apparent_temperature":23.0,
        "relative_humidity_2m":41,"wind_speed_10m":12.3,"wind_direction_10m":290,
        "surface_pressure":1017.2,"weather_code":1,"uv_index":5.1,
        "is_day":1,"cloud_cover":90,"precipitation":1.2},
      "hourly":{"time":["2026-09-10T09:00","2026-09-10T10:00"],"temperature_2m":[21.0,22.5],
        "precipitation_probability":[0,null],"precipitation":[0.0,1.4],
        "weather_code":[3,61],"wind_speed_10m":[8.0,null]},
      "daily":{"time":["2026-09-10","2026-09-11"],"temperature_2m_max":[26.0,21.0],
        "temperature_2m_min":[14.0,13.0],"weather_code":[0,3],
        "sunrise":["2026-09-10T06:12","2026-09-11T06:13"],"sunset":["2026-09-10T19:04","2026-09-11T19:02"],
        "precipitation_probability_max":[10,98],"precipitation_sum":[0.0,1.2],
        "wind_speed_10m_max":[15.5,21.0],"uv_index_max":[5.0,null]}
    }"#;

    /// A régi, szűkebb válasz (új mezők nélkül) is érvényes marad.
    const OLD_FIXTURE: &str = r#"{
      "current":{"temperature_2m":1.0,"apparent_temperature":1.0,"relative_humidity_2m":1,
        "wind_speed_10m":1.0,"wind_direction_10m":1,"surface_pressure":1.0,"weather_code":0,"uv_index":null},
      "hourly":{"time":["2026-09-10T09:00"],"temperature_2m":[1.0],"precipitation_probability":[null]},
      "daily":{"time":["2026-09-10"],"temperature_2m_max":[1.0],"temperature_2m_min":[1.0],
        "weather_code":[0],"sunrise":["2026-09-10T06:12"],"sunset":["2026-09-10T19:04"]}
    }"#;

    #[test]
    fn parses_fixture() {
        let w = parse(FIXTURE, "Budapest").unwrap();
        assert_eq!(w.place, "Budapest");
        assert_eq!(w.temp, 22.1);
        assert_eq!(w.humidity, 41);
        assert_eq!(w.wind_dir, 290);
        assert_eq!(w.code, 1);
        assert_eq!(w.uv, Some(5.1));
        assert_eq!(w.sunrise, "06:12");
        assert_eq!(w.sunset, "19:04");
        assert_eq!(w.hourly.len(), 2);
        assert_eq!(w.hourly[0].hour, 9);
        assert_eq!(w.hourly[1].precip, 0, "null → 0");
        assert_eq!(w.hourly[0].code, 3);
        assert_eq!(w.hourly[1].code, 61);
        assert_eq!(w.hourly[0].wind_kmh, 8.0);
        assert_eq!(w.hourly[1].wind_kmh, 0.0, "null → 0");
        assert_eq!(w.hourly[1].precip_mm, 1.4);
        assert!(w.is_day);
        assert_eq!(w.clouds, 90);
        assert_eq!(w.precip_mm, 1.2);
        assert_eq!(w.daily.len(), 2);
        assert_eq!(w.daily[1].code, 3);
        assert_eq!(w.daily[1].precip_pct, 98);
        assert_eq!(w.daily[1].precip_mm, 1.2);
        assert_eq!(w.daily[1].wind_max, 21.0);
        assert_eq!(w.daily[0].uv_max, 5.0);
        assert_eq!(w.daily[1].uv_max, 0.0, "null → 0");
        assert_eq!(w.daily[0].date.to_string(), "2026-09-10");
        assert!(!w.is_stale());
    }

    #[test]
    fn missing_optional_fields_default_to_zero() {
        let w = parse(OLD_FIXTURE, "X").unwrap();
        assert_eq!(w.clouds, 0);
        assert!(w.is_day, "hiányzó is_day → nappal");
        assert_eq!(w.hourly[0].code, 255);
        assert_eq!(w.hourly[0].wind_kmh, 0.0);
        assert_eq!(w.daily[0].precip_pct, 0);
        assert_eq!(w.daily[0].uv_max, 0.0);
    }

    /// Modellhorizont szélén / éjféli napnál az Open-Meteo `null`-t ad vissza
    /// ezekben a régóta meglévő mezőkben is – egy `null` nem húzhatja el az
    /// egész snapshotot.
    const NULL_FIXTURE: &str = r#"{
      "current":{"temperature_2m":1.0,"apparent_temperature":1.0,"relative_humidity_2m":1,
        "wind_speed_10m":1.0,"wind_direction_10m":1,"surface_pressure":1.0,"weather_code":0,"uv_index":null},
      "hourly":{"time":["2026-09-10T09:00","2026-09-10T10:00"],"temperature_2m":[1.0,null],
        "precipitation_probability":[null,null]},
      "daily":{"time":["2026-09-10","2026-09-11"],"temperature_2m_max":[20.0,null],
        "temperature_2m_min":[null,9.0],"weather_code":[0,null],
        "sunrise":[null,"2026-09-11T06:13"],"sunset":["2026-09-10T19:04",null]}
    }"#;

    #[test]
    fn null_in_legacy_fields_does_not_fail_the_snapshot() {
        let w = parse(NULL_FIXTURE, "X").unwrap();
        assert_eq!(w.hourly[0].temp, 1.0);
        assert_eq!(w.hourly[1].temp, 0.0, "null hőfok → 0");
        assert_eq!(w.daily[0].tmax, 20.0);
        assert_eq!(w.daily[1].tmax, 0.0, "null max → 0");
        assert_eq!(w.daily[0].tmin, 0.0, "null min → 0");
        assert_eq!(w.daily[1].code, 255, "null weather_code → 255");
        assert_eq!(w.sunrise, "--:--", "null napkelte → --:--");
        assert_eq!(w.sunset, "19:04", "az első nap napnyugtája még valid");
    }

    #[test]
    fn bad_json_is_error_not_panic() {
        assert!(parse("{\"current\":{}}", "X").is_err());
    }

    #[test]
    fn url_contains_coords_and_fields() {
        let u = url(&WeatherCfg::default());
        assert!(u.contains("latitude=47.4979"));
        assert!(u.contains("forecast_days=7"));
        assert!(u.contains("forecast_hours=24"));
        for f in ["is_day", "cloud_cover", "wind_speed_10m_max", "uv_index_max", "precipitation_probability_max", "precipitation_sum"] {
            assert!(u.contains(f), "hiányzik: {f}");
        }
        assert!(u.contains("timezone=auto"));
    }
    const AIR_FIXTURE: &str = r#"{
      "current":{"time":"2026-09-11T09:00","european_aqi":32,"pm10":12.4,"pm2_5":8.1,
        "ozone":61.0,"nitrogen_dioxide":10.5,
        "alder_pollen":null,"grass_pollen":45.0,"ragweed_pollen":120.0,"olive_pollen":0.0}
    }"#;

    #[test]
    fn parses_air_fixture() {
        let a = parse_air(AIR_FIXTURE).unwrap();
        assert_eq!(a.aqi, 32);
        assert_eq!(a.pm2_5, 8.1);
        assert_eq!(a.pm10, 12.4);
        assert_eq!(a.ozone, 61.0);
        assert_eq!(a.no2, 10.5);
        // birch/mugwort hiányzik → kimarad; alder null → 0; csökkenő sorrend.
        assert_eq!(a.pollen, vec![("ragweed", 120.0), ("grass", 45.0), ("alder", 0.0), ("olive", 0.0)]);
        assert!(parse_air("{}").is_err());
        let empty = parse_air(r#"{"current":{}}"#).unwrap();
        assert_eq!(empty.aqi, 0);
        assert!(empty.pollen.is_empty());
    }

    #[test]
    fn aqi_bands_and_pollen_levels() {
        for (v, b) in [(0u16, "good"), (20, "good"), (21, "fair"), (40, "fair"), (41, "moderate"),
                       (60, "moderate"), (61, "poor"), (80, "poor"), (81, "very poor"), (100, "very poor"),
                       (101, "extremely poor")] {
            assert_eq!(aqi_band(v), b, "EAQI {v}");
        }
        for (v, l) in [(0.0f32, "none"), (1.0, "low"), (20.0, "low"), (21.0, "moderate"), (80.0, "moderate"),
                       (81.0, "high"), (200.0, "high"), (201.0, "very high")] {
            assert_eq!(pollen_level(v), l, "pollen {v}");
        }
    }

    #[test]
    fn air_url_contains_coords_and_fields() {
        let u = air_url(&WeatherCfg::default());
        assert!(!u.chars().any(char::is_whitespace), "a missing line-continuation put spaces in the URL (seen live as n/a): {u}");
        assert!(u.starts_with("https://air-quality-api.open-meteo.com/"));
        assert!(u.contains("latitude=47.4979"));
        for f in ["european_aqi", "pm2_5", "nitrogen_dioxide", "ragweed_pollen", "timezone=auto"] {
            assert!(u.contains(f), "hiányzik: {f}");
        }
    }
}

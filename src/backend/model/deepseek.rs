//! DeepSeek Responses provider.

use std::sync::Arc;

use chrono::Datelike as _;
use chrono::Timelike as _;

use super::Model;
use super::ModelPricing;
use super::openai::OpenAi;
use super::provider::HostedWebSearch;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use crate::Error;
use crate::Result;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("deepseek_provider.toml")));
pub(super) static CATALOG: std::sync::LazyLock<super::provider::ModelCatalog> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("deepseek.toml")));

static PRICING: std::sync::LazyLock<PricingManifest> = std::sync::LazyLock::new(|| {
    let pricing: PricingManifest = crate::config::embedded(include_str!("deepseek_pricing.toml"));
    pricing
        .validate()
        .expect("bundled DeepSeek tariffs must be valid");
    pricing
});

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PricingManifest {
    tariffs: Vec<Tariff>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Tariff {
    models: Vec<String>,
    effective_from: u64,
    peak_weekdays: Vec<u32>,
    peak_windows: Vec<PeakWindow>,
    off_peak: ModelPricing,
    peak: ModelPricing,
    holiday_calendar: Option<HolidayCalendar>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PeakWindow {
    start_hour_utc: u32,
    end_hour_utc: u32,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HolidayCalendar {
    start_date: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
    public_holidays: std::collections::BTreeSet<chrono::NaiveDate>,
}

impl PricingManifest {
    fn validate(&self) -> Result<()> {
        for tariff in &self.tariffs {
            tariff.validate()?;
        }
        Ok(())
    }

    fn resolve(&self, model: &str, unix_seconds: u64) -> Option<ModelPricing> {
        let tariff = self
            .tariffs
            .iter()
            .filter(|tariff| {
                tariff.effective_from <= unix_seconds
                    && tariff.models.iter().any(|candidate| candidate == model)
            })
            .max_by_key(|tariff| tariff.effective_from)?;
        let now = chrono::DateTime::from_timestamp(i64::try_from(unix_seconds).ok()?, 0)?;
        if !tariff.is_peak_window(now) {
            return Some(tariff.off_peak);
        }
        // Holiday coverage is required only when it can change the selected rate.
        let calendar = tariff.holiday_calendar.as_ref()?;
        let date = now.date_naive();
        if !(calendar.start_date..=calendar.end_date).contains(&date) {
            return None;
        }
        Some(if calendar.public_holidays.contains(&date) {
            tariff.off_peak
        } else {
            tariff.peak
        })
    }
}

impl Tariff {
    fn validate(&self) -> Result<()> {
        if self.models.is_empty() || self.models.iter().any(|model| model.trim().is_empty()) {
            return Err(Error::Config(
                "DeepSeek tariff requires model identifiers".into(),
            ));
        }
        if self.peak_weekdays.iter().any(|day| !(1..=7).contains(day))
            || self.peak_windows.iter().any(|window| {
                window.start_hour_utc >= window.end_hour_utc || window.end_hour_utc > 24
            })
        {
            return Err(Error::Config("invalid DeepSeek peak schedule".into()));
        }
        if let Some(calendar) = &self.holiday_calendar {
            calendar.validate()?;
        }
        Ok(())
    }

    fn is_peak_window(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.peak_weekdays
            .contains(&now.weekday().number_from_monday())
            && self
                .peak_windows
                .iter()
                .any(|window| (window.start_hour_utc..window.end_hour_utc).contains(&now.hour()))
    }
}

impl HolidayCalendar {
    fn validate(&self) -> Result<()> {
        if self.start_date > self.end_date
            || self
                .public_holidays
                .iter()
                .any(|date| !(self.start_date..=self.end_date).contains(date))
        {
            return Err(Error::Config(
                "invalid DeepSeek holiday calendar coverage".into(),
            ));
        }
        Ok(())
    }
}

fn pricing(base_url: &str, model: &str) -> Option<ModelPricing> {
    if !provider().uses_default_endpoint(Some(base_url)) {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    PRICING.resolve(model, now)
}

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "deepseek",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        Some(&CATALOG),
        build_provider,
    )
    .with_credentialless_endpoints()
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("DeepSeek requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("deepseek")?;
    let provider = OpenAi::with_client(
        api_key,
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_pricing_resolver(pricing)
    .with_service_tier(config.service_tier)
    .without_image_input();
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    let provider = match config.web_search {
        HostedWebSearch::Off => provider,
        HostedWebSearch::Cached => {
            return Err(Error::Config(
                "DeepSeek does not support cached web search".into(),
            ));
        }
        HostedWebSearch::Live => provider.with_web_search(),
    };
    Ok(Arc::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::provider::ProviderAuth;
    use crate::backend::model::provider::ProviderCredential;
    use crate::backend::model::provider::provider as registered_provider;

    fn timestamp(value: &str) -> u64 {
        u64::try_from(
            chrono::DateTime::parse_from_rfc3339(value)
                .expect("test timestamp")
                .timestamp(),
        )
        .expect("test time follows epoch")
    }

    #[test]
    fn tariff_windows_resolve_exact_off_peak_and_unknown_uncovered_peak() {
        let off_peak = ModelPricing::new(150_000, 3_000, 150_000, 600_000);
        for value in [
            "2026-10-04T02:00:00Z", // Sunday.
            "2026-10-05T00:59:59Z",
            "2026-10-05T04:00:00Z",
            "2026-10-05T05:59:59Z",
            "2026-10-05T10:00:00Z",
            "2026-10-10T07:00:00Z", // Saturday.
        ] {
            assert_eq!(
                PRICING.resolve("deepseek-v4-flash", timestamp(value)),
                Some(off_peak),
                "{value}"
            );
        }
        for value in [
            "2026-10-05T01:00:00Z",
            "2026-10-05T03:59:59Z",
            "2026-10-05T06:00:00Z",
            "2026-10-05T09:59:59Z",
        ] {
            assert_eq!(
                PRICING.resolve("deepseek-v4-flash", timestamp(value)),
                None,
                "{value}"
            );
        }
        assert_eq!(PRICING.resolve("deepseek-v4-flash", 0), None);
        assert_eq!(
            PRICING.resolve("unknown-model", timestamp("2026-10-04T02:00:00Z")),
            None
        );
    }

    #[test]
    fn complete_calendar_resolves_peak_and_holiday_without_extending_coverage() {
        let mut pricing: PricingManifest =
            toml::from_str(include_str!("deepseek_pricing.toml")).expect("provider tariff data");
        // Synthetic calendar fixture, not a claim about actual public holidays.
        pricing.tariffs[0].holiday_calendar = Some(toml::from_str(
            "start_date = '2026-10-05'\nend_date = '2026-10-07'\npublic_holidays = ['2026-10-06']",
        ).expect("calendar fixture"));
        pricing.validate().expect("valid calendar coverage");
        assert_eq!(
            pricing.resolve("deepseek-flash", timestamp("2026-10-05T01:00:00Z")),
            Some(ModelPricing::new(300_000, 6_000, 300_000, 1_200_000)),
        );
        assert_eq!(
            pricing.resolve("deepseek-v4-flash", timestamp("2026-10-06T07:00:00Z")),
            Some(ModelPricing::new(150_000, 3_000, 150_000, 600_000)),
        );
        assert_eq!(
            pricing.resolve("deepseek-flash", timestamp("2026-10-08T01:00:00Z")),
            None
        );
        assert_eq!(
            pricing.resolve("deepseek-v4-pro", timestamp("2026-10-04T02:00:00Z")),
            Some(ModelPricing::new(660_000, 22_000, 660_000, 1_980_000)),
        );
    }

    #[test]
    fn tariffs_reject_invalid_schedule_and_uncovered_holiday_dates() {
        let mut pricing: PricingManifest =
            toml::from_str(include_str!("deepseek_pricing.toml")).expect("provider tariff data");
        pricing.tariffs[0].peak_windows[0].end_hour_utc = 25;
        assert!(pricing.validate().is_err());
        let calendar: HolidayCalendar = toml::from_str(
            "start_date = '2026-10-05'\nend_date = '2026-10-07'\npublic_holidays = ['2026-10-08']",
        )
        .expect("synthetic calendar data");
        assert!(calendar.validate().is_err());
        assert!(
            toml::from_str::<HolidayCalendar>(
                "start_date = '2026-13-05'\nend_date = '2026-10-07'\npublic_holidays = []",
            )
            .is_err()
        );
    }

    #[test]
    fn custom_deepseek_proxies_do_not_inherit_native_prices() {
        assert_eq!(
            pricing("https://proxy.example/v1", "deepseek-v4-flash"),
            None
        );
        let definition = provider();
        let model = definition
            .build(ProviderBuildConfig {
                credential: ProviderCredential::ApiKey("test-key".into()),
                model: "deepseek-v4-flash".into(),
                base_url: Some("https://proxy.example/v1".into()),
                reasoning_effort: None,
                service_tier: None,
                web_search: HostedWebSearch::Off,
                http: reqwest::Client::new(),
                transport: crate::backend::model::ModelTransportSettings::default(),
            })
            .expect("custom provider");
        assert_eq!(model.pricing(), None);
    }

    #[test]
    fn advertised_web_search_modes_build() {
        let definition = provider();
        for web_search in definition.web_search().iter().copied() {
            definition
                .build(ProviderBuildConfig {
                    credential: ProviderCredential::ApiKey("test-key".into()),
                    model: definition.default_model().expect("default model").into(),
                    base_url: Some(MANIFEST.base_url.as_str().into()),
                    reasoning_effort: None,
                    service_tier: None,
                    web_search,
                    http: reqwest::Client::new(),
                    transport: crate::backend::model::ModelTransportSettings::default(),
                })
                .expect("advertised web search mode builds");
        }
    }

    #[test]
    fn registered_provider_builds_its_default_model() {
        let definition = registered_provider("deepseek").expect("registered provider");
        let model = definition
            .build(ProviderBuildConfig {
                credential: ProviderCredential::ApiKey("test-key".into()),
                model: "deepseek-v4-flash".into(),
                base_url: Some(MANIFEST.base_url.as_str().into()),
                reasoning_effort: None,
                service_tier: None,
                web_search: HostedWebSearch::Off,
                http: reqwest::Client::new(),
                transport: crate::backend::model::ModelTransportSettings::default(),
            })
            .expect("build provider");

        assert!(matches!(
            definition.auth(),
            ProviderAuth::ApiKey(key) if Some(key) == MANIFEST.credential_env.as_deref()
        ));
        assert_eq!(definition.models(), &CATALOG.models);
        assert_eq!(definition.web_search(), &MANIFEST.search);
        assert_eq!(model.info().reasoning_effort.as_deref(), Some("high"));
        assert!(!model.supports_image_input());
    }
}

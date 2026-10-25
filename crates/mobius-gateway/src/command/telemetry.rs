use std::sync::Arc;

use super::args::TelemetryCommand;
use super::{
    ClientCapabilities, ClientMessage, ConfigStore, Endpoint, Error, GatewayClient,
    MAX_PENDING_FRAMES, PROCESS_FILE, PathBuf, Result, ServerMessage, direct_loopback_endpoint,
    ensure_gateway_stopped, new_request_id, print_line, running_process_record,
};
use crate::telemetry::{SinkMethod, TelemetrySink};

pub(super) async fn run(
    state_dir: PathBuf,
    command: TelemetryCommand,
    load: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let (store, mut config) = ConfigStore::open(state_dir)?;
    let endpoint = direct_loopback_endpoint(&config)?;
    let running = running_process_record(&store.state_dir().join(PROCESS_FILE))?.is_some();
    if !running {
        ensure_gateway_stopped(&store, &config)?;
    }
    if matches!(command, TelemetryCommand::List) && running {
        let token = load(&endpoint)?
            .ok_or_else(|| Error::Config("local control credential unavailable".into()))?;
        let response = request(&endpoint, &token, |request_id| {
            ClientMessage::GetTelemetry { request_id }
        })
        .await?;
        return Ok(print_line(format_args!(
            "{}",
            serde_json::to_string_pretty(&response)?
        ))?);
    }
    let expected_revision = config.telemetry.revision;
    let previous =
        (!running && !matches!(command, TelemetryCommand::List)).then(|| config.telemetry.clone()); // rules-allow: the persisted sinks are kept to roll back a failed cursor sync
    match command {
        TelemetryCommand::List => {
            for sink in &mut config.telemetry.sinks {
                sink.redact_report()?;
            }
            return Ok(print_line(format_args!(
                "{}",
                serde_json::to_string_pretty(&config.telemetry)?
            ))?);
        }
        TelemetryCommand::Remove { id } => config.telemetry.sinks.retain(|sink| sink.id != id),
        TelemetryCommand::Add {
            id,
            url,
            every_seconds,
            upload_admission,
            sections,
            events,
            bearer_env,
            bearer_file,
            fields,
        } => {
            let fields = fields
                .into_iter()
                .map(|field| {
                    field
                        .split_once('=')
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                        .ok_or_else(|| Error::Config("--field requires key=value".into()))
                })
                .collect::<Result<_>>()?;
            let sections = sections
                .into_iter()
                .map(|value| {
                    serde_json::from_value(serde_json::Value::String(value)).map_err(Error::from)
                })
                .collect::<Result<_>>()?;
            let events = events
                .into_iter()
                .map(|value| {
                    serde_json::from_value(serde_json::Value::String(value)).map_err(Error::from)
                })
                .collect::<Result<_>>()?;
            config.telemetry.sinks.retain(|sink| sink.id != id);
            config.telemetry.sinks.push(TelemetrySink {
                id,
                url,
                method: SinkMethod::Post,
                every_seconds,
                upload_admission,
                sections,
                events,
                bearer_env,
                bearer_file,
                fields,
                headers: Default::default(),
                enabled: true,
            });
        }
    }
    config.telemetry.revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| Error::Config("telemetry revision overflow".into()))?;
    let config = config.validated()?;
    if running {
        let token = load(&endpoint)?
            .ok_or_else(|| Error::Config("local control credential unavailable".into()))?;
        request(&endpoint, &token, |request_id| {
            ClientMessage::ConfigureTelemetry {
                request_id,
                expected_revision,
                sinks: config.into_inner().telemetry.sinks,
                preserve_auth: Vec::new(),
            }
        })
        .await?;
    } else {
        let bots = crate::bots::BotStore::open(store.state_dir())?;
        let publication = crate::publication::Outcome::applied(store.save(&config))?;
        if let Err(error) = bots.sync_telemetry_cursors(&config.telemetry.sinks) {
            if let Some(previous) = previous {
                let mut config = config.into_inner();
                config.telemetry = previous;
                if let Err(rollback) = store.publish_live(&config) {
                    return Err(crate::publication::applied_error(Error::Config(format!(
                        "{error}; telemetry rollback did not complete: {rollback}"
                    ))));
                }
            }
            return Err(error);
        }
        publication.confirm()?;
    }
    Ok(())
}

async fn request(
    endpoint: &Endpoint,
    token: &str,
    message: impl FnOnce(Arc<str>) -> ClientMessage,
) -> Result<ServerMessage> {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let mut options = crate::client::ConnectOptions {
            catalog: crate::wire::CatalogHint {
                skip: crate::wire::READY_SECTIONS.into_iter().collect(),
                ..Default::default()
            },
            ..Default::default()
        };
        let client = GatewayClient::connect_with(endpoint, token, ClientCapabilities::default(), &mut options).await?;
        let (sender, mut events) = client.into_parts();
        let mut request_id = [0; uuid::fmt::Hyphenated::LENGTH];
        let request_id = new_request_id(&mut request_id);
        sender.send(message(request_id.into())).await?;
        for _ in 0..MAX_PENDING_FRAMES {
            let frame = events.next().await?.ok_or_else(|| Error::Protocol("gateway disconnected".into()))?;
            if let Some(error) = frame.message.response_error(Some(request_id)) { return Err(Error::Protocol(error.message.into())); }
            if matches!(&frame.message, ServerMessage::Telemetry { request_id: id, .. } if **id == *request_id) { return Ok(frame.message); }
        }
        Err(Error::Protocol("too many unrelated responses".into()))
    }).await.map_err(|elapsed| Error::Protocol(format!("telemetry configuration timed out: {elapsed}")))?
}

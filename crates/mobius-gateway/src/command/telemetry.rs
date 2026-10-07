use super::args::TelemetryCommand;
use super::*;
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
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    let expected_revision = config.telemetry.revision;
    let previous =
        (!running && !matches!(command, TelemetryCommand::List)).then(|| config.telemetry.clone());
    match command {
        TelemetryCommand::List => {
            for sink in &mut config.telemetry.sinks {
                sink.redact_report()?;
            }
            println!("{}", serde_json::to_string_pretty(&config.telemetry)?);
            return Ok(());
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
    config.validate()?;
    if running {
        let token = load(&endpoint)?
            .ok_or_else(|| Error::Config("local control credential unavailable".into()))?;
        request(&endpoint, &token, |request_id| {
            ClientMessage::ConfigureTelemetry {
                request_id,
                expected_revision,
                sinks: config.telemetry.sinks,
                preserve_auth: Vec::new(),
            }
        })
        .await?;
    } else {
        let bots = crate::bots::BotStore::open(store.state_dir())?;
        let publication = crate::publication::Outcome::applied(store.save(&config))?;
        if let Err(error) = bots.sync_telemetry_cursors(&config.telemetry.sinks) {
            if let Some(previous) = previous {
                config.telemetry = previous;
                if let Err(rollback) = store.save(&config) {
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
    message: impl FnOnce(String) -> ClientMessage,
) -> Result<ServerMessage> {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let mut options = crate::client::ConnectOptions {
            catalog: crate::wire::CatalogHint {
                skip: crate::wire::READY_SECTIONS.into_iter().collect(),
                ..Default::default()
            },
            ..Default::default()
        };
        let client = GatewayClient::connect_with(endpoint, token, ClientKind::GatewayDashboard, &mut options).await?;
        let (sender, mut events) = client.into_parts();
        let request_id = Uuid::new_v4().to_string();
        sender.send(message(request_id.clone())).await?;
        for _ in 0..MAX_PENDING_FRAMES {
            let frame = events.next().await?.ok_or_else(|| Error::Protocol("gateway disconnected".into()))?;
            if let Some(error) = frame.message.response_error(Some(&request_id)) { return Err(Error::Protocol(error.message.into())); }
            if matches!(&frame.message, ServerMessage::Telemetry { request_id: id, .. } if id == &request_id) { return Ok(frame.message); }
        }
        Err(Error::Protocol("too many unrelated responses".into()))
    }).await.map_err(|_| Error::Protocol("telemetry configuration timed out".into()))?
}

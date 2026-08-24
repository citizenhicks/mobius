use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use mobius::Error;
use mobius::Result;
use mobius::protocol::FrontendActiveInput;
use mobius::protocol::FrontendContribution;
use mobius::protocol::FrontendEvent;
use mobius::protocol::FrontendPickerOption;
use mobius::protocol::FrontendWidget;
use mobius::protocol::ModelChoice;
use mobius::protocol::Op;

use super::setup::SetupMode;

const COMMAND_PREFIX: char = '/';
const WORKSPACE_REFERENCE_TRIGGER: char = '@';
const MAX_FILES: usize = 20_000;
const MAX_DEPTH: usize = 64;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_MATCHES: usize = 8;

pub(crate) struct UiCatalog {
    commands: Vec<UiCommand>,
    references: Vec<UiReference>,
    reference_triggers: Vec<char>,
    widgets: Vec<(String, FrontendWidget)>,
    model_choices: Vec<ModelChoice>,
    active_input: Option<FrontendActiveInput>,
    accepts_file_attachments: bool,
    workspace: PathBuf,
    workspace_references: Arc<OnceLock<Vec<UiReference>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuItem {
    pub value: String,
    pub label: String,
    pub description: String,
}

#[derive(Clone)]
struct UiCommand {
    name: String,
    arguments: String,
    description: String,
    requires_idle: bool,
    handler: CommandHandler,
}

#[derive(Clone)]
enum CommandHandler {
    Help,
    GatewaySettings,
    Extensions,
    Agent,
    Workspace,
    Login,
    Pair,
    Profile,
    New,
    Clear,
    Model,
    Reasoning,
    Status,
    Interrupt,
    Exit,
    Capability { capability: String },
}

struct UiReference {
    trigger: char,
    value: String,
    description: String,
    replacement: String,
}

#[derive(Clone, Copy)]
pub(crate) struct CommandContext<'a> {
    pub active_turn: Option<&'a str>,
    pub status: &'a str,
    pub model_route: &'a str,
}

#[derive(Debug, PartialEq)]
pub(crate) enum CommandAction {
    Submit(Op),
    Gateway(GatewayAction),
    GatewaySettings,
    Extensions,
    Setup {
        mode: SetupMode,
        provider: Option<String>,
    },
    Frontend(FrontendEvent),
    ShowMenu,
    Print(String),
    Exit,
    New,
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayAction {
    Workspace(String),
    Pair,
    Profile,
}

impl UiCatalog {
    pub(crate) fn build(
        contributions: &[FrontendContribution],
        model_choices: &[ModelChoice],
        workspace: &Path,
    ) -> Result<Self> {
        let mut commands = cli_commands();
        let mut references = Vec::new();
        let mut widgets = Vec::new();

        for contribution in contributions {
            commands.extend(contribution.commands.iter().map(|command| UiCommand {
                name: command.name.clone(),
                arguments: command.arguments.clone(),
                description: command.description.clone(),
                requires_idle: command.requires_idle,
                handler: CommandHandler::Capability {
                    capability: contribution.capability.clone(),
                },
            }));
            references.extend(contribution.references.iter().map(|reference| UiReference {
                trigger: reference.trigger,
                value: reference.value.clone(),
                description: reference.description.clone(),
                replacement: format!("{}{}", reference.trigger, reference.value),
            }));
            widgets.extend(
                contribution
                    .widgets
                    .iter()
                    .cloned()
                    .map(|widget| (contribution.capability.clone(), widget)),
            );
        }

        validate(&commands, &references)?;
        let mut reference_triggers = vec![WORKSPACE_REFERENCE_TRIGGER];
        for reference in &references {
            if !reference_triggers.contains(&reference.trigger) {
                reference_triggers.push(reference.trigger);
            }
        }
        Ok(Self {
            commands,
            references,
            reference_triggers,
            widgets,
            model_choices: model_choices.to_vec(),
            active_input: contributions
                .iter()
                .find_map(|contribution| contribution.active_input.clone()),
            accepts_file_attachments: contributions
                .iter()
                .any(|contribution| contribution.accepts_file_attachments),
            workspace: workspace.to_path_buf(),
            workspace_references: Arc::new(OnceLock::new()),
        })
    }

    pub(crate) fn start_workspace_inventory(
        &self,
        local_gateway: bool,
    ) -> tokio::task::JoinHandle<()> {
        let workspace = self.workspace.clone();
        let references = Arc::clone(&self.workspace_references);
        tokio::task::spawn_blocking(move || {
            let paths = if local_gateway {
                workspace_inventory(&workspace).unwrap_or_default()
            } else {
                Vec::new()
            };
            let items = paths
                .into_iter()
                .map(|path| UiReference {
                    trigger: WORKSPACE_REFERENCE_TRIGGER,
                    replacement: workspace_replacement(&path),
                    value: path,
                    description: "file".into(),
                })
                .collect();
            let _ = references.set(items);
        })
    }

    pub(crate) fn widgets(&self) -> impl Iterator<Item = (&str, &FrontendWidget)> {
        self.widgets
            .iter()
            .map(|(capability, widget)| (capability.as_str(), widget))
    }

    pub(crate) fn model_choices(&self) -> &[ModelChoice] {
        &self.model_choices
    }

    pub(crate) fn replace_model_choices(&mut self, choices: &[ModelChoice]) {
        self.model_choices = choices.to_vec();
    }

    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub(crate) fn active_input(&self) -> Option<&FrontendActiveInput> {
        self.active_input.as_ref()
    }

    pub(crate) const fn accepts_file_attachments(&self) -> bool {
        self.accepts_file_attachments
    }

    pub(crate) fn command_suggestions(&self, input: &str, cursor: usize) -> Option<Vec<MenuItem>> {
        let query = input.strip_prefix(COMMAND_PREFIX)?;
        let token_end = input.find(char::is_whitespace).unwrap_or(input.len());
        if cursor > token_end {
            return None;
        }
        let query = query[..token_end.saturating_sub(COMMAND_PREFIX.len_utf8())].to_lowercase();
        let (exact, prefix): (Vec<_>, Vec<_>) = self
            .commands
            .iter()
            .filter(|command| command.name.starts_with(&query))
            .map(|command| (command.name == query, command.menu_item()))
            .partition(|(is_exact, _)| *is_exact);
        Some(
            exact
                .into_iter()
                .chain(prefix)
                .map(|(_, item)| item)
                .collect(),
        )
    }

    pub(crate) fn reference_triggers(&self) -> impl Iterator<Item = char> + '_ {
        self.reference_triggers.iter().copied()
    }

    pub(crate) fn reference_suggestions(&self, trigger: char, query: &str) -> Vec<MenuItem> {
        let workspace: &[UiReference] = if trigger == WORKSPACE_REFERENCE_TRIGGER {
            self.workspace_references.get().map_or(&[], Vec::as_slice)
        } else {
            &[]
        };
        let references = || {
            self.references
                .iter()
                .filter(move |reference| reference.trigger == trigger)
                .chain(workspace.iter())
        };
        let query = query.to_lowercase();
        if query.is_empty() {
            return references()
                .take(MAX_MATCHES)
                .map(UiReference::menu_item)
                .collect();
        }
        let mut matches = Vec::with_capacity(MAX_MATCHES);
        for reference in references() {
            let Some(score) = score(&reference.value, &query) else {
                continue;
            };
            let position = matches
                .iter()
                .position(
                    |(current_score, current): &((u8, usize, usize), &UiReference)| {
                        score < *current_score
                            || (score == *current_score && reference.value < current.value)
                    },
                )
                .unwrap_or(matches.len());
            if position < MAX_MATCHES {
                matches.insert(position, (score, reference));
                matches.truncate(MAX_MATCHES);
            }
        }
        matches
            .into_iter()
            .take(MAX_MATCHES)
            .map(|(_, reference)| reference.menu_item())
            .collect()
    }

    pub(crate) fn menu(&self) -> String {
        self.commands
            .iter()
            .map(UiCommand::menu_item)
            .map(|item| format!("{:<26} {}", item.label, item.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn dispatch(
        &self,
        line: &str,
        context: CommandContext<'_>,
    ) -> Option<CommandAction> {
        let input = line.strip_prefix(COMMAND_PREFIX)?;
        let (name, arguments) = input
            .split_once(char::is_whitespace)
            .map_or((input, ""), |(name, arguments)| (name, arguments.trim()));
        if name.is_empty() {
            return Some(CommandAction::ShowMenu);
        }
        let Some(command) = self.commands.iter().find(|command| command.name == name) else {
            return Some(CommandAction::Print(format!(
                "unknown command `{COMMAND_PREFIX}{name}`"
            )));
        };
        if command.requires_idle && context.active_turn.is_some() {
            return Some(CommandAction::Print(format!(
                "`{COMMAND_PREFIX}{}` is available when the agent is idle",
                command.name
            )));
        }
        Some(match &command.handler {
            CommandHandler::Help => CommandAction::ShowMenu,
            CommandHandler::GatewaySettings if arguments.is_empty() => {
                CommandAction::GatewaySettings
            }
            CommandHandler::GatewaySettings => CommandAction::Print("usage: /gateway".into()),
            CommandHandler::Extensions if arguments.is_empty() => CommandAction::Extensions,
            CommandHandler::Extensions => CommandAction::Print("usage: /extensions".into()),
            CommandHandler::Agent if arguments.is_empty() => CommandAction::Setup {
                mode: SetupMode::Agent,
                provider: None,
            },
            CommandHandler::Agent => CommandAction::Print("usage: /agent".into()),
            CommandHandler::Workspace => {
                CommandAction::Gateway(GatewayAction::Workspace(arguments.into()))
            }
            CommandHandler::Login if arguments.split_whitespace().count() <= 1 => {
                CommandAction::Setup {
                    mode: SetupMode::Login,
                    provider: (!arguments.is_empty()).then(|| arguments.into()),
                }
            }
            CommandHandler::Login => CommandAction::Print("usage: /login [provider]".into()),
            CommandHandler::Pair => CommandAction::Gateway(GatewayAction::Pair),
            CommandHandler::Profile => CommandAction::Gateway(GatewayAction::Profile),
            CommandHandler::New => CommandAction::New,
            CommandHandler::Clear => CommandAction::Clear,
            CommandHandler::Model => model_picker(&self.model_choices),
            CommandHandler::Reasoning => reasoning_picker(context.model_route, &self.model_choices),
            CommandHandler::Status => CommandAction::Print(context.status.to_string()),
            CommandHandler::Interrupt => context.active_turn.map_or_else(
                || CommandAction::Print("no active turn to interrupt".into()),
                |turn_id| {
                    CommandAction::Submit(Op::Interrupt {
                        turn_id: turn_id.to_string(),
                    })
                },
            ),
            CommandHandler::Exit => CommandAction::Exit,
            CommandHandler::Capability { capability } => {
                CommandAction::Submit(Op::CapabilityCommand {
                    capability: capability.clone(),
                    command: command.name.clone(),
                    arguments: arguments.to_string(),
                    input: None,
                    target: None,
                })
            }
        })
    }
}

impl UiCommand {
    fn menu_item(&self) -> MenuItem {
        let command = format!("{COMMAND_PREFIX}{}", self.name);
        MenuItem {
            value: command.clone(),
            label: if self.arguments.is_empty() {
                command
            } else {
                format!("{command} {}", self.arguments)
            },
            description: self.description.clone(),
        }
    }
}

impl UiReference {
    fn menu_item(&self) -> MenuItem {
        MenuItem {
            value: self.replacement.clone(),
            label: format!("{}{}", self.trigger, self.value),
            description: self.description.clone(),
        }
    }
}

fn cli_commands() -> Vec<UiCommand> {
    vec![
        command("help", "show commands", false, CommandHandler::Help),
        command(
            "gateway",
            "view, pair, or reconnect gateways",
            true,
            CommandHandler::GatewaySettings,
        ),
        command(
            "extensions",
            "install, update, trust, or remove extensions",
            true,
            CommandHandler::Extensions,
        ),
        UiCommand {
            name: "agent".into(),
            arguments: String::new(),
            description: "configure agent features".into(),
            requires_idle: true,
            handler: CommandHandler::Agent,
        },
        UiCommand {
            name: "workspace".into(),
            arguments: "<gateway-path>".into(),
            description: "start a chat in another workspace".into(),
            requires_idle: true,
            handler: CommandHandler::Workspace,
        },
        UiCommand {
            name: "login".into(),
            arguments: "[provider]".into(),
            description: "authenticate a provider and configure its agent".into(),
            requires_idle: true,
            handler: CommandHandler::Login,
        },
        command(
            "profile",
            "show gateway usage statistics",
            false,
            CommandHandler::Profile,
        ),
        command(
            "pair",
            "create a one-time code for another client",
            false,
            CommandHandler::Pair,
        ),
        command("new", "start a new chat", true, CommandHandler::New),
        command(
            "clear",
            "clear the terminal and start a new chat",
            true,
            CommandHandler::Clear,
        ),
        command(
            "model",
            "select a configured model",
            true,
            CommandHandler::Model,
        ),
        command(
            "reasoning",
            "select reasoning effort",
            true,
            CommandHandler::Reasoning,
        ),
        command(
            "status",
            "show turn, token, and capability status",
            false,
            CommandHandler::Status,
        ),
        command(
            "interrupt",
            "stop the active turn",
            false,
            CommandHandler::Interrupt,
        ),
        command("exit", "exit möbius", false, CommandHandler::Exit),
    ]
}

fn command(
    name: &str,
    description: &str,
    requires_idle: bool,
    handler: CommandHandler,
) -> UiCommand {
    UiCommand {
        name: name.into(),
        arguments: String::new(),
        description: description.into(),
        requires_idle,
        handler,
    }
}

fn validate(commands: &[UiCommand], references: &[UiReference]) -> Result<()> {
    let mut identifiers = BTreeSet::new();
    for command in commands {
        if command.name.is_empty()
            || command.name.starts_with(COMMAND_PREFIX)
            || command.name.chars().any(char::is_whitespace)
        {
            return Err(Error::Config(format!(
                "invalid frontend command `{}`",
                command.name
            )));
        }
        if !identifiers.insert(&command.name) {
            return Err(Error::Duplicate(format!(
                "frontend command `{}`",
                command.name
            )));
        }
    }

    let mut registered = BTreeSet::new();
    for reference in references {
        if reference.trigger == COMMAND_PREFIX
            || reference.trigger.is_control()
            || reference.trigger.is_whitespace()
            || reference.value.is_empty()
        {
            return Err(Error::Config(format!(
                "invalid frontend reference `{}{}`",
                reference.trigger, reference.value
            )));
        }
        if !registered.insert((reference.trigger, reference.value.as_str())) {
            return Err(Error::Duplicate(format!(
                "frontend reference `{}{}`",
                reference.trigger, reference.value
            )));
        }
    }
    Ok(())
}

fn model_picker(choices: &[ModelChoice]) -> CommandAction {
    let mut groups = Vec::new();
    let options = choices
        .iter()
        .filter(|choice| {
            let group = choice.group.as_str();
            if groups.contains(&group) {
                false
            } else {
                groups.push(group);
                true
            }
        })
        .map(|choice| FrontendPickerOption {
            label: choice.model.clone(),
            description: choice.reasoning_effort.as_ref().map_or_else(
                || choice.group.clone(),
                |effort| format!("{} · {effort}", choice.group),
            ),
            detail: String::new(),
            symbol: None,
            shows_detail: false,
            op: Op::SetModel {
                route: choice.route.clone(),
            },
        })
        .collect::<Vec<_>>();
    if options.is_empty() {
        return CommandAction::Print("no models are configured".into());
    }
    CommandAction::Frontend(FrontendEvent::Picker {
        title: "Select model".into(),
        options,
    })
}

fn reasoning_picker(route: &str, choices: &[ModelChoice]) -> CommandAction {
    let Some(current) = choices.iter().find(|choice| choice.route == route) else {
        return CommandAction::Print("current model route is unavailable".into());
    };
    let options = choices
        .iter()
        .filter(|choice| choice.group == current.group && choice.model == current.model)
        .map(|choice| FrontendPickerOption {
            label: choice
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| "default".into()),
            description: if choice.route == route {
                format!("{} · current", choice.model)
            } else {
                choice.model.clone()
            },
            detail: String::new(),
            symbol: None,
            shows_detail: false,
            op: Op::SetModel {
                route: choice.route.clone(),
            },
        })
        .collect::<Vec<_>>();
    if options.len() < 2 {
        return CommandAction::Print(format!("{} has no reasoning choices", current.model));
    }
    CommandAction::Frontend(FrontendEvent::Picker {
        title: format!("{} reasoning", current.model),
        options,
    })
}

fn workspace_replacement(path: &str) -> String {
    if path.chars().any(char::is_whitespace) && !path.contains('"') {
        format!("\"{path}\"")
    } else {
        path.to_owned()
    }
}

fn workspace_inventory(root: &Path) -> io::Result<Vec<String>> {
    let root = std::fs::canonicalize(root)?;
    let mut files = Vec::new();
    walk(&root, &root, 0, &mut files)?;
    files.sort_unstable();
    Ok(files)
}

fn walk(root: &Path, directory: &Path, depth: usize, files: &mut Vec<String>) -> io::Result<()> {
    let mut entries = std::fs::read_dir(directory)?
        .filter_map(std::result::Result::ok)
        .collect::<Vec<_>>();
    entries.sort_unstable_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        if files.len() >= MAX_FILES {
            break;
        }
        if matches!(entry.file_name().to_str(), Some(".git" | "target")) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                let _ = walk(root, &path, depth + 1, files);
            }
        } else if kind.is_file()
            && let Ok(relative) = path.strip_prefix(root)
            && let Some(path) = slash_path(relative)
            && path.len() <= MAX_PATH_BYTES
        {
            files.push(path);
        }
    }
    Ok(())
}

fn slash_path(path: &Path) -> Option<String> {
    path.components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

fn score(value: &str, query: &str) -> Option<(u8, usize, usize)> {
    let value = value.to_lowercase();
    let name = value.rsplit('/').next().unwrap_or(&value);
    let length = value.chars().count();
    if name == query {
        Some((0, 0, length))
    } else if name.starts_with(query) {
        Some((1, 0, length))
    } else if value.starts_with(query) {
        Some((2, 0, length))
    } else if let Some(index) = name.find(query) {
        Some((3, index, length))
    } else if let Some(index) = value.find(query) {
        Some((4, index, length))
    } else if let Some(gaps) = subsequence_gaps(name, query) {
        Some((5, gaps, length))
    } else {
        subsequence_gaps(&value, query).map(|gaps| (6, gaps, length))
    }
}

fn subsequence_gaps(value: &str, query: &str) -> Option<usize> {
    let mut value = value.chars().enumerate();
    let mut first = None;
    let mut last = 0;
    let mut count = 0;
    for wanted in query.chars() {
        let (index, _) = value.find(|(_, character)| *character == wanted)?;
        first.get_or_insert(index);
        last = index;
        count += 1;
    }
    Some(last + 1 - first.unwrap_or_default() - count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobius::protocol::FrontendCommand;
    use mobius::protocol::FrontendReference;

    fn model_choices() -> Vec<ModelChoice> {
        vec![
            ModelChoice {
                route: "kimi".into(),
                group: "kimi".into(),
                model: "kimi-k3".into(),
                reasoning_effort: Some("high".into()),
                context_window: Some(1_048_576),
                supports_image_input: true,
            },
            ModelChoice {
                route: "kimi-low".into(),
                group: "kimi".into(),
                model: "kimi-k3".into(),
                reasoning_effort: Some("low".into()),
                context_window: Some(1_048_576),
                supports_image_input: true,
            },
        ]
    }

    fn contribution(command: &str) -> FrontendContribution {
        FrontendContribution {
            capability: "test".into(),
            accepts_file_attachments: false,
            count: None,
            commands: vec![FrontendCommand {
                name: command.into(),
                arguments: String::new(),
                description: "test command".into(),
                requires_idle: true,
            }],
            widgets: Vec::new(),
            references: Vec::new(),
            active_input: None,
        }
    }

    #[test]
    fn bare_login_and_agent_commands_open_setup() {
        let workspace = tempfile::tempdir().expect("workspace");
        let catalog = UiCatalog::build(&[], &[], workspace.path()).expect("catalog");
        let context = CommandContext {
            active_turn: None,
            status: "idle",
            model_route: "kimi",
        };

        assert_eq!(
            catalog.dispatch("/login", context),
            Some(CommandAction::Setup {
                mode: SetupMode::Login,
                provider: None,
            })
        );
        assert_eq!(
            catalog.dispatch("/agent", context),
            Some(CommandAction::Setup {
                mode: SetupMode::Agent,
                provider: None,
            })
        );
        assert_eq!(
            catalog.dispatch("/login kimi", context),
            Some(CommandAction::Setup {
                mode: SetupMode::Login,
                provider: Some("kimi".into()),
            })
        );
        assert_eq!(
            catalog.dispatch("/agent {}", context),
            Some(CommandAction::Print("usage: /agent".into()))
        );
    }

    #[test]
    fn retains_file_attachment_capability() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut attachments = contribution("attach");
        attachments.accepts_file_attachments = true;

        let catalog = UiCatalog::build(&[attachments], &[], workspace.path()).expect("catalog");

        assert!(catalog.accepts_file_attachments());
    }

    #[test]
    fn bare_gateway_command_opens_gateway_settings() {
        let workspace = tempfile::tempdir().expect("workspace");
        let catalog = UiCatalog::build(&[], &[], workspace.path()).expect("catalog");

        assert_eq!(
            catalog.dispatch(
                "/gateway",
                CommandContext {
                    active_turn: None,
                    status: "idle",
                    model_route: "kimi",
                },
            ),
            Some(CommandAction::GatewaySettings)
        );
    }

    #[test]
    fn extensions_command_opens_the_gateway_catalog_only_when_idle() {
        let workspace = tempfile::tempdir().expect("workspace");
        let catalog = UiCatalog::build(&[], &[], workspace.path()).expect("catalog");
        let idle = CommandContext {
            active_turn: None,
            status: "idle",
            model_route: "kimi",
        };

        assert_eq!(
            catalog.dispatch("/extensions", idle),
            Some(CommandAction::Extensions)
        );
        assert_eq!(
            catalog.dispatch("/extensions extra", idle),
            Some(CommandAction::Print("usage: /extensions".into()))
        );
        assert!(matches!(
            catalog.dispatch(
                "/extensions",
                CommandContext {
                    active_turn: Some("turn"),
                    status: "working",
                    model_route: "kimi",
                },
            ),
            Some(CommandAction::Print(message)) if message.contains("agent is idle")
        ));
    }

    #[test]
    fn active_capability_command_dispatch_is_declared_data() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut cron = contribution("cron");
        cron.capability = "cron".into();
        cron.commands[0].requires_idle = false;
        let catalog = UiCatalog::build(&[cron], &[], workspace.path()).expect("catalog");
        let context = CommandContext {
            active_turn: Some("turn"),
            status: "working",
            model_route: "kimi",
        };

        assert!(matches!(
            catalog.dispatch("/cron new review pull requests", context),
            Some(CommandAction::Submit(Op::CapabilityCommand {
                capability,
                command,
                arguments,
                ..
            })) if capability == "cron"
                && command == "cron"
                && arguments == "new review pull requests"
        ));
    }

    #[test]
    fn rejects_middleware_command_collisions_with_the_shell() {
        let workspace = tempfile::tempdir().expect("workspace");
        let error = match UiCatalog::build(&[contribution("exit")], &[], workspace.path()) {
            Ok(_) => panic!("duplicate command"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "duplicate registration: frontend command `exit`"
        );
    }

    #[tokio::test]
    async fn merges_middleware_provider_and_workspace_contributions() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("source file.rs"), "").expect("workspace file");
        std::fs::create_dir(workspace.path().join(".git")).expect("git directory");
        std::fs::create_dir(workspace.path().join("target")).expect("target directory");
        std::fs::write(workspace.path().join(".git/config"), "").expect("git file");
        std::fs::write(workspace.path().join("target/artifact"), "").expect("artifact");
        let mut inspect = contribution("inspect");
        inspect.references.push(FrontendReference {
            trigger: '@',
            value: "middleware".into(),
            description: "middleware reference".into(),
        });

        let catalog = UiCatalog::build(
            &[inspect, contribution("cron")],
            &model_choices(),
            workspace.path(),
        )
        .expect("catalog");
        catalog
            .start_workspace_inventory(true)
            .await
            .expect("workspace inventory");
        let commands = catalog.menu();
        let references = catalog.reference_suggestions('@', "");

        assert!(
            commands.contains("/inspect")
                && commands.contains("/model")
                && commands.contains("/workspace")
                && commands.contains("/cron")
                && commands.contains("/pair")
                && references
                    .iter()
                    .any(|item| item.value == "\"source file.rs\"")
                && references.iter().any(|item| item.value == "@middleware")
                && !references
                    .iter()
                    .any(|item| item.value.contains("artifact") || item.value.contains(".git"))
        );
        assert_eq!(catalog.reference_triggers().collect::<Vec<_>>(), ['@']);
    }
}

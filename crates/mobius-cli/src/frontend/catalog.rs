use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use mobius::Error;
use mobius::Result;
use mobius::protocol::FrontendContribution;
use mobius::protocol::FrontendWidget;
use mobius::protocol::Op;

use super::setup::SetupMode;

const COMMAND_PREFIX: char = '/';
const WORKSPACE_REFERENCE_TRIGGER: char = '@';
const MAX_MATCHES: usize = 8;

pub(crate) struct UiCatalog {
    commands: Vec<UiCommand>,
    references: Vec<UiReference>,
    reference_triggers: Vec<char>,
    widgets: Vec<(String, FrontendWidget)>,
    accepts_file_attachments: bool,
    workspace: PathBuf,
    workspace_references: Vec<UiReference>,
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
    Bot,
    Workspace,
    Login,
    Pair,
    Profile,
    New,
    Clear,
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
}

#[derive(Debug, PartialEq)]
pub(crate) enum CommandAction {
    Submit(Op),
    Gateway(GatewayAction),
    GatewaySettings,
    Extensions,
    Bots,
    Setup {
        mode: SetupMode,
        provider: Option<String>,
    },
    ShowMenu,
    Print(String),
    Exit,
    ChooseBot {
        workspace: PathBuf,
        clear: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayAction {
    Pair,
    Profile,
}

impl UiCatalog {
    pub(crate) fn build(contributions: &[FrontendContribution], workspace: &Path) -> Result<Self> {
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
            accepts_file_attachments: contributions
                .iter()
                .any(|contribution| contribution.accepts_file_attachments),
            workspace: workspace.to_path_buf(),
            workspace_references: Vec::new(),
        })
    }

    pub(crate) fn set_workspace_paths(&mut self, paths: impl IntoIterator<Item = String>) {
        self.workspace_references = paths
            .into_iter()
            .map(|path| UiReference {
                trigger: WORKSPACE_REFERENCE_TRIGGER,
                replacement: workspace_replacement(&path),
                value: path,
                description: "file".into(),
            })
            .collect();
    }

    pub(crate) fn widgets(&self) -> impl Iterator<Item = (&str, &FrontendWidget)> {
        self.widgets
            .iter()
            .map(|(capability, widget)| (capability.as_str(), widget))
    }

    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
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
            &self.workspace_references
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
            CommandHandler::Bot if arguments.is_empty() => CommandAction::Bots,
            CommandHandler::Bot => CommandAction::Print("usage: /bot".into()),
            CommandHandler::Workspace if arguments.is_empty() => {
                CommandAction::Print("usage: /workspace <gateway-path>".into())
            }
            CommandHandler::Workspace => CommandAction::ChooseBot {
                workspace: PathBuf::from(arguments),
                clear: false,
            },
            CommandHandler::Login if arguments.split_whitespace().count() <= 1 => {
                CommandAction::Setup {
                    mode: SetupMode::Login,
                    provider: (!arguments.is_empty()).then(|| arguments.into()),
                }
            }
            CommandHandler::Login => CommandAction::Print("usage: /login [provider]".into()),
            CommandHandler::Pair => CommandAction::Gateway(GatewayAction::Pair),
            CommandHandler::Profile => CommandAction::Gateway(GatewayAction::Profile),
            CommandHandler::New => CommandAction::ChooseBot {
                workspace: self.workspace.clone(),
                clear: false,
            },
            CommandHandler::Clear => CommandAction::ChooseBot {
                workspace: self.workspace.clone(),
                clear: true,
            },
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
            name: "bot".into(),
            arguments: String::new(),
            description: "configure this Bot".into(),
            requires_idle: true,
            handler: CommandHandler::Bot,
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
            description: "authenticate a provider and configure Bot defaults".into(),
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

fn workspace_replacement(path: &str) -> String {
    if path.chars().any(char::is_whitespace) && !path.contains('"') {
        format!("\"{path}\"")
    } else {
        path.to_owned()
    }
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
        }
    }

    #[test]
    fn bare_login_and_bot_commands_open_setup() {
        let workspace = tempfile::tempdir().expect("workspace");
        let catalog = UiCatalog::build(&[], workspace.path()).expect("catalog");
        let context = CommandContext {
            active_turn: None,
            status: "idle",
        };

        assert_eq!(
            catalog.dispatch("/login", context),
            Some(CommandAction::Setup {
                mode: SetupMode::Login,
                provider: None,
            })
        );
        assert_eq!(catalog.dispatch("/bot", context), Some(CommandAction::Bots));
        assert_eq!(
            catalog.dispatch("/login kimi", context),
            Some(CommandAction::Setup {
                mode: SetupMode::Login,
                provider: Some("kimi".into()),
            })
        );
        assert_eq!(
            catalog.dispatch("/bot {}", context),
            Some(CommandAction::Print("usage: /bot".into()))
        );
    }

    #[test]
    fn retains_file_attachment_capability() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut attachments = contribution("attach");
        attachments.accepts_file_attachments = true;

        let catalog = UiCatalog::build(&[attachments], workspace.path()).expect("catalog");

        assert!(catalog.accepts_file_attachments());
    }

    #[test]
    fn bare_gateway_command_opens_gateway_settings() {
        let workspace = tempfile::tempdir().expect("workspace");
        let catalog = UiCatalog::build(&[], workspace.path()).expect("catalog");

        assert_eq!(
            catalog.dispatch(
                "/gateway",
                CommandContext {
                    active_turn: None,
                    status: "idle",
                },
            ),
            Some(CommandAction::GatewaySettings)
        );
    }

    #[test]
    fn extensions_command_opens_the_gateway_catalog_only_when_idle() {
        let workspace = tempfile::tempdir().expect("workspace");
        let catalog = UiCatalog::build(&[], workspace.path()).expect("catalog");
        let idle = CommandContext {
            active_turn: None,
            status: "idle",
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
                },
            ),
            Some(CommandAction::Print(message)) if message.contains("agent is idle")
        ));
    }

    #[test]
    fn active_capability_command_dispatch_is_declared_data() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut review = contribution("review");
        review.capability = "review".into();
        review.commands[0].requires_idle = false;
        let catalog = UiCatalog::build(&[review], workspace.path()).expect("catalog");
        let context = CommandContext {
            active_turn: Some("turn"),
            status: "working",
        };

        assert!(matches!(
            catalog.dispatch("/review pull requests", context),
            Some(CommandAction::Submit(Op::CapabilityCommand {
                capability,
                command,
                arguments,
                ..
            })) if capability == "review"
                && command == "review"
                && arguments == "pull requests"
        ));
    }

    #[test]
    fn rejects_middleware_command_collisions_with_the_shell() {
        let workspace = tempfile::tempdir().expect("workspace");
        let error = match UiCatalog::build(&[contribution("exit")], workspace.path()) {
            Ok(_) => panic!("duplicate command"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "duplicate registration: frontend command `exit`"
        );
    }

    #[test]
    fn merges_middleware_provider_and_workspace_contributions() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut inspect = contribution("inspect");
        inspect.references.push(FrontendReference {
            trigger: '@',
            value: "middleware".into(),
            description: "middleware reference".into(),
        });

        let mut catalog =
            UiCatalog::build(&[inspect, contribution("audit")], workspace.path()).expect("catalog");
        catalog.set_workspace_paths(["source file.rs".into()]);
        let commands = catalog.menu();
        let references = catalog.reference_suggestions('@', "");

        assert!(
            commands.contains("/inspect")
                && commands.contains("/bot")
                && commands.contains("/workspace")
                && commands.contains("/audit")
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
        catalog.set_workspace_paths(["new.rs".into()]);
        assert!(catalog.reference_suggestions('@', "source").is_empty());
        assert_eq!(catalog.reference_suggestions('@', "new")[0].value, "new.rs");
    }
}

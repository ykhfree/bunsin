//! The connections section of the main window: the LLM endpoint and the connectors
//! of the selected profile. The fields are the user's draft. Only Save sends them.
//! A typed token goes from its masked field into the command, and nowhere else.

use crate::bridge::{ConnectionResult, SaveError, UiCommand, UiState};
use crate::shell::{CONFLICT, Shell, labelled, labelled_as};
use crate::theme::{card, field, palette, quiet};
use bunsin_api::{
    CommandConnectorUpdate, ConnectorConnection, ConnectorUpdate, Expected, LlmConnection, LlmUpdate, SecretChange,
    TokenChange, TokenSource, ToolsReport,
};
use bunsin_core::settings::{ANTHROPIC_BASE_URL, ANTHROPIC_DEFAULT_MODEL, McpConfig, Wire, default_timeout_secs};
use bunsin_core::tool::pin;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Div, Entity, FontWeight, IntoElement, ParentElement, SharedString, Stateful,
    Styled, Window, div, prelude::*, px,
};
use std::collections::BTreeMap;

/// The API formats, in the order of the choices on screen.
const WIRES: [(Wire, &str); 3] = [
    (Wire::AnthropicMessages, "Anthropic Messages"),
    (Wire::OpenAiChat, "OpenAI Chat Completions"),
    (Wire::OpenAiResponses, "OpenAI Responses"),
];

const HELP: &str = "Leave a token field empty to keep the stored token. \
    The server never sends a token back.";
const COMMAND_HELP: &str = "The server starts this program and talks to it. \
    The operator of the server must allow that: the server starts with --allow-stdio-connectors.";
const KIND_TAKEN: &str = "A connector with this name is of the other kind. Remove it first, or use another name.";
const SECRET_HELP: &str = "Secret variables, such as an API token. The server encrypts each value, \
    gives it to the program, and does not show it again.";
const SECRET_FIELDS: &str = "Enter the name and the value of the secret variable.";
const SECRET_UNADDED: &str = "Add the secret variable or clear its fields first.";
const ENV_LINE: &str = "Write each environment variable as NAME=value, one in a line.";
const ENV_TWICE: &str = "An environment variable is in the list twice.";
const NOT_EDITABLE: &str = "This connector has an empty argument or a line break in a value. \
    This form cannot show it. Change it in the settings file.";

/// What the connector form saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An MCP server at an address.
    Address,
    /// A program that the server starts.
    Command,
}

/// The kinds, in the order of the choices on screen.
const KINDS: [(Kind, &str); 2] = [(Kind::Address, "Address"), (Kind::Command, "Command on the server")];

/// The arguments in the text of the form: one in each line. Empty lines are no arguments.
/// A line is used as it is, so an argument can hold spaces. Quotes are not removed.
fn parse_args(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The environment variables in the text of the form: `NAME=value` in each line.
/// The name ends at the first `=`. The value is used as it is.
fn parse_env(text: &str) -> Result<BTreeMap<String, String>, &'static str> {
    let mut env = BTreeMap::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once('=').ok_or(ENV_LINE)?;
        if name.is_empty() {
            return Err(ENV_LINE);
        }
        if env.insert(name.to_owned(), value.to_owned()).is_some() {
            return Err(ENV_TWICE);
        }
    }
    Ok(env)
}

/// The text of the form for a stored command, or None if a save from the form would change it:
/// the form cannot hold an empty argument or a line break inside an argument or a value.
fn command_texts(args: &[String], env: &BTreeMap<String, String>) -> Option<(String, String)> {
    let shown = |text: &str| !text.contains(['\n', '\r']);
    let args_fit = args.iter().all(|arg| !arg.is_empty() && shown(arg));
    let env_fits = env
        .iter()
        .all(|(name, value)| !name.is_empty() && !name.contains('=') && shown(name) && shown(value));
    (args_fit && env_fits).then(|| {
        let env: Vec<String> = env.iter().map(|(name, value)| format!("{name}={value}")).collect();
        (args.join("\n"), env.join("\n"))
    })
}

/// The secret variables of a connector that have a value on the server.
fn stored_secrets(connector: &ConnectorConnection) -> impl Iterator<Item = &String> {
    connector
        .secret_env
        .iter()
        .filter(|name| !connector.secret_env_missing.contains(name))
}

/// How many arguments and variables a command has, for the list and the form.
fn counts_text(args: usize, env: usize, secrets: usize) -> String {
    let plural = |count: usize, one: &str, many: &str| format!("{count} {}", if count == 1 { one } else { many });
    let mut text = format!(
        "{}, {}",
        plural(args, "argument", "arguments"),
        plural(env, "environment variable", "environment variables")
    );
    if secrets > 0 {
        text.push_str(&format!(", {}", plural(secrets, "secret variable", "secret variables")));
    }
    text
}
const KEYCHAIN_HELP: &str = "Or copy the token from an item of the login Keychain of this Mac. \
    Keychain Access shows the service of an item as Where. The token is copied once: \
    a later change in the Keychain does not reach the server.";
const KEYCHAIN_FIELDS: &str = "Enter the service (Where) and the account of the Keychain item.";
const KEYCHAIN_EMPTY: &str = "The password of the Keychain item is empty.";
const KEYCHAIN_READ: &str = "The token is in the token field now. Save sends it to the server.";
const SAVE_FIRST: &str = "Save the endpoint first. The test uses the saved settings.";
const TESTING: &str = "Testing… One short request goes to the endpoint. It waits for the answer up to the timeout.";
const TYPED_TOKEN: &str = "Empty the token field first, or save the token.";
const APPROVE_NOTE: &str = "Approving tools does not create permission rules.";

pub struct ConnectionsForm {
    /// The profile that the drafts belong to. None until the server's connections arrive.
    profile: Option<String>,
    /// The value of `UiState::connection_commands` in the last state the form received.
    commands_seen: u64,
    wire: Wire,
    llm_address: Entity<InputState>,
    llm_model: Entity<InputState>,
    llm_token: Entity<InputState>,
    name: Entity<InputState>,
    address: Entity<InputState>,
    token: Entity<InputState>,
    /// The service and the account of a Keychain item that holds the LLM token.
    keychain_service: Entity<InputState>,
    keychain_account: Entity<InputState>,
    /// What the last read of the Keychain did.
    keychain_message: Option<String>,
    /// True while the Keychain fields are shown. Most owners type the token, so they start hidden.
    keychain_open: bool,
    /// The header that carries the connector's token. Empty: `Authorization: Bearer`.
    token_header: Entity<InputState>,
    /// Whether the connector draft is an address or a command.
    kind: Kind,
    /// The absolute path of the program of a command connector.
    command: Entity<InputState>,
    /// The arguments of the command, one in each line.
    args: Entity<TextareaState>,
    /// The environment variables of the command, `NAME=value` in each line.
    env: Entity<TextareaState>,
    /// The secret variables of the command draft: a stored value that stays, or a new value.
    secrets: BTreeMap<String, SecretChange>,
    secret_name: Entity<InputState>,
    /// Masked. Add moves the typed value into `secrets`.
    secret_value: Entity<InputState>,
    /// The window's own message about the last LLM save that it did not send.
    llm_message: Option<&'static str>,
    /// The window's own message about the last connector save that it did not send.
    connector_message: Option<&'static str>,
    /// The LLM endpoint the LLM draft was made from. None if the server had none.
    llm_base: Option<LlmConnection>,
    /// True after the server refused an LLM save because the endpoint changed since the form loaded it.
    llm_conflict: bool,
    /// True if the last LLM save removed the stored token. "Save anyway" does the same.
    llm_remove_token: bool,
    /// True from a click on Test until the result of a command arrives.
    llm_testing: bool,
    /// The connector the connector draft was made from, with its name. Edit sets it.
    connector_base: Option<(String, ConnectorConnection)>,
    /// The connector whose save the server refused because it changed since the form loaded it.
    connector_conflict: Option<String>,
    /// True if the last connector save removed the stored token. "Save anyway" does the same.
    connector_remove_token: bool,
}

/// The API format, address and model that the LLM form shows for `llm`.
/// Without an endpoint the form offers the Claude API, so the owner only enters the token.
fn llm_fields(llm: Option<&LlmConnection>) -> (Wire, &str, &str) {
    match llm {
        Some(llm) => (llm.wire, &llm.base_url, &llm.model),
        None => (Wire::AnthropicMessages, ANTHROPIC_BASE_URL, ANTHROPIC_DEFAULT_MODEL),
    }
}

fn text(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_owned()
}

fn set(input: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut App) {
    input.update(cx, |input, cx| input.set_value(text, window, cx));
}

/// What a save does with the token. An empty field keeps the stored token.
/// None if the user asked to remove the token while the field holds one.
fn token_change(field: &Entity<InputState>, remove: bool, cx: &App) -> Option<TokenChange> {
    let typed = text(field, cx);
    match (remove, typed.is_empty()) {
        (false, true) => Some(TokenChange::Keep),
        (false, false) => Some(TokenChange::Set(typed)),
        (true, true) => Some(TokenChange::Clear),
        (true, false) => None,
    }
}

pub fn token_text(source: TokenSource) -> &'static str {
    match source {
        TokenSource::Stored => "token stored on the server",
        TokenSource::ServerEnvironment => "token from the server's environment",
        TokenSource::None => "no token",
    }
}

/// The outcome of Check: the tool count, and how the list differs from the approved one.
pub fn tools_text(report: &ToolsReport) -> String {
    let count = report.tools.len();
    let diff = &report.diff;
    if diff.is_clean() {
        return format!("works: {count} tool(s), the same as the approved list");
    }
    let part = |names: &[String], what: &str| match names {
        [] => format!("0 {what}"),
        names => format!("{} {what} ({})", names.len(), names.join(", ")),
    };
    format!(
        "works: {count} tool(s); compared with the approved list: {}, {}, {}",
        part(&diff.added, "new"),
        part(&diff.changed, "changed"),
        part(&diff.removed, "gone"),
    )
}

impl ConnectionsForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        // Masked, with no toggle that would show the token.
        let token = |window: &mut Window, cx: &mut App| cx.new(|cx| InputState::new(window, cx).masked(true));
        Self {
            profile: None,
            commands_seen: 0,
            wire: Wire::AnthropicMessages,
            llm_address: input("https://api.example.com", window, cx),
            llm_model: input("Model name", window, cx),
            llm_token: token(window, cx),
            name: input("Name, for example jira", window, cx),
            address: input("https://mcp.example.com/mcp", window, cx),
            token: token(window, cx),
            keychain_service: input("Where, for example api.anthropic.com", window, cx),
            keychain_account: input("Account", window, cx),
            keychain_message: None,
            keychain_open: false,
            token_header: input("Authorization: Bearer", window, cx),
            kind: Kind::Address,
            command: input("/usr/local/bin/npx", window, cx),
            args: cx.new(|cx| TextareaState::new(window, cx).placeholder("-y\nsome-mcp-server")),
            env: cx.new(|cx| TextareaState::new(window, cx).placeholder("REGION=eu")),
            secrets: BTreeMap::new(),
            secret_name: input("API_TOKEN", window, cx),
            secret_value: token(window, cx),
            llm_message: None,
            connector_message: None,
            llm_base: None,
            llm_conflict: false,
            llm_remove_token: false,
            llm_testing: false,
            connector_base: None,
            connector_conflict: None,
            connector_remove_token: false,
        }
    }

    /// Takes a new state from the server. A draft that the user changed stays. The form
    /// loads the server's connections if another profile is selected or a save just succeeded.
    /// A draft that the user did not change follows the server.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) {
        let new = state.connection_commands != self.commands_seen;
        self.commands_seen = state.connection_commands;
        let result = state.connection_result.as_ref().filter(|_| new);
        if new {
            // The bridge runs one command at a time, so the test has ended.
            self.llm_testing = false;
        }
        if self.profile != state.selected {
            self.llm_testing = false;
            self.clear_keychain(window, cx);
            // Every draft and base goes, typed tokens too.
            self.profile = None;
            self.llm_message = None;
            self.llm_conflict = false;
            set(&self.llm_token, "", window, cx);
            self.load_connector("", None, window, cx);
        }
        match result {
            Some(ConnectionResult::LlmSaved(Err(SaveError::Conflict))) => self.llm_conflict = true,
            Some(ConnectionResult::ConnectorSaved {
                name,
                result: Err(SaveError::Conflict),
            }) => self.connector_conflict = Some(name.clone()),
            _ => {}
        }
        let llm_saved = matches!(result, Some(ConnectionResult::LlmSaved(Ok(()))));
        if llm_saved {
            // The token is on the server now, even if the reload after the save failed.
            set(&self.llm_token, "", window, cx);
            self.clear_keychain(window, cx);
        }
        if matches!(result, Some(ConnectionResult::ConnectorSaved { result: Ok(()), .. })) {
            // The form is empty for the next connector.
            self.load_connector("", None, window, cx);
        }
        let (Some(profile), Some(connections)) = (&state.selected, &state.connections) else {
            return;
        };
        let llm = connections.llm.as_ref();
        if self.profile.is_none() || llm_saved || (self.llm_base.as_ref() != llm && self.llm_untouched(cx)) {
            self.load_llm(llm, window, cx);
        }
        self.profile = Some(profile.clone());
        if let Some((name, base)) = &self.connector_base {
            let server = connections.connectors.get(name);
            if server != Some(base) && self.connector_untouched(cx) {
                let name = name.clone();
                self.load_connector(&name, server, window, cx);
            }
        }
    }

    /// Changes the API format. The prefilled Claude address and model do not fit another format,
    /// so they are emptied. Fields that are empty get them back with the Claude format.
    /// Text that the user typed stays.
    fn choose_wire(&mut self, wire: Wire, window: &mut Window, cx: &mut App) {
        let (address, model) = (text(&self.llm_address, cx), text(&self.llm_model, cx));
        let claude = wire == Wire::AnthropicMessages;
        if !claude && address == ANTHROPIC_BASE_URL && model == ANTHROPIC_DEFAULT_MODEL {
            set(&self.llm_address, "", window, cx);
            set(&self.llm_model, "", window, cx);
        } else if claude && address.is_empty() && model.is_empty() {
            set(&self.llm_address, ANTHROPIC_BASE_URL, window, cx);
            set(&self.llm_model, ANTHROPIC_DEFAULT_MODEL, window, cx);
        }
        self.wire = wire;
    }

    /// Shows the server's LLM endpoint, or the defaults if there is none, and makes it the base.
    fn load_llm(&mut self, llm: Option<&LlmConnection>, window: &mut Window, cx: &mut App) {
        let (wire, address, model) = llm_fields(llm);
        self.wire = wire;
        set(&self.llm_address, address, window, cx);
        set(&self.llm_model, model, window, cx);
        set(&self.llm_token, "", window, cx);
        self.llm_base = llm.cloned();
        self.llm_conflict = false;
    }

    /// True if the LLM fields show the base and the token field is empty.
    fn llm_untouched(&self, cx: &App) -> bool {
        let (wire, address, model) = llm_fields(self.llm_base.as_ref());
        self.wire == wire
            && text(&self.llm_address, cx) == address
            && text(&self.llm_model, cx) == model
            && text(&self.llm_token, cx).is_empty()
    }

    /// Shows connector `name` as the server has it, so the user can change it, and makes it the base.
    /// A connector that the form cannot edit, or none, empties the form.
    fn load_connector(
        &mut self,
        name: &str,
        connector: Option<&ConnectorConnection>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (mut address, mut header, mut command) = ("", "", String::new());
        let (mut args, mut env) = (String::new(), String::new());
        match connector.map(|connector| (&connector.mcp, connector)) {
            Some((McpConfig::Http { url }, connector)) => {
                self.kind = Kind::Address;
                address = url;
                header = connector.token_header.as_deref().unwrap_or_default();
            }
            Some((
                McpConfig::Stdio {
                    command: path,
                    args: stored_args,
                    env: stored_env,
                },
                _,
            )) => match command_texts(stored_args, stored_env) {
                Some(texts) => {
                    self.kind = Kind::Command;
                    command = path.display().to_string();
                    (args, env) = texts;
                }
                // A save from this form would change the connector. The draft stays as it is.
                None => {
                    self.connector_message = Some(NOT_EDITABLE);
                    return;
                }
            },
            // An empty form keeps the kind that the owner chose.
            None => {}
        }
        set(&self.name, if connector.is_some() { name } else { "" }, window, cx);
        set(&self.address, address, window, cx);
        set(&self.token, "", window, cx);
        set(&self.token_header, header, window, cx);
        set(&self.command, &command, window, cx);
        self.args.update(cx, |area, cx| area.set_value(args, window, cx));
        self.env.update(cx, |area, cx| area.set_value(env, window, cx));
        // The stored values stay on the server. The draft keeps each of them until the owner changes it.
        // A name without a value on the server is not in the draft: "keep" would be refused.
        // The form names it, and the owner adds it with a value.
        self.secrets = connector
            .map(|c| {
                stored_secrets(c)
                    .map(|name| (name.clone(), SecretChange::Keep))
                    .collect()
            })
            .unwrap_or_default();
        set(&self.secret_name, "", window, cx);
        set(&self.secret_value, "", window, cx);
        self.connector_base = connector.map(|c| (name.to_owned(), c.clone()));
        self.connector_message = None;
        self.connector_conflict = None;
    }

    /// True if no secret variable is typed that Add has not taken.
    fn secret_fields_empty(&self, cx: &App) -> bool {
        text(&self.secret_name, cx).is_empty() && self.secret_value.read(cx).value().is_empty()
    }

    /// Moves the typed secret variable into the draft. Its value leaves the field.
    fn add_secret(&mut self, window: &mut Window, cx: &mut App) {
        let name = text(&self.secret_name, cx);
        // The value is used as it is. A space can be part of it.
        let value = self.secret_value.read(cx).value().to_string();
        if name.is_empty() || value.is_empty() {
            self.connector_message = Some(SECRET_FIELDS);
            return;
        }
        self.secrets.insert(name, SecretChange::Set(value));
        set(&self.secret_name, "", window, cx);
        set(&self.secret_value, "", window, cx);
        self.connector_message = None;
    }

    /// The text of an area as it is. A space at the start of a line is part of an argument.
    fn raw(area: &Entity<TextareaState>, cx: &App) -> String {
        area.read(cx).value().to_string()
    }

    /// Empties the Keychain fields and their message.
    fn clear_keychain(&mut self, window: &mut Window, cx: &mut App) {
        set(&self.keychain_service, "", window, cx);
        set(&self.keychain_account, "", window, cx);
        self.keychain_message = None;
        self.keychain_open = false;
    }

    /// True if the LLM fields are not the endpoint `llm` of the server, or the server has none.
    fn llm_differs(&self, llm: Option<&LlmConnection>, cx: &App) -> bool {
        match llm {
            Some(llm) => {
                self.wire != llm.wire
                    || text(&self.llm_address, cx) != llm.base_url
                    || text(&self.llm_model, cx) != llm.model
                    || !text(&self.llm_token, cx).is_empty()
            }
            None => true,
        }
    }

    /// True if the connector fields show the base and the token field is empty.
    fn connector_untouched(&self, cx: &App) -> bool {
        self.connector_base.as_ref().is_some_and(|(name, base)| {
            text(&self.name, cx) == *name
                && match &base.mcp {
                    McpConfig::Http { url } => {
                        self.kind == Kind::Address
                            && *url == text(&self.address, cx)
                            && text(&self.token, cx).is_empty()
                            && base.token_header.as_deref().unwrap_or_default() == text(&self.token_header, cx)
                    }
                    McpConfig::Stdio { command, args, env } => {
                        self.kind == Kind::Command
                            && command.display().to_string() == text(&self.command, cx)
                            && *args == parse_args(&Self::raw(&self.args, cx))
                            && parse_env(&Self::raw(&self.env, cx)).as_ref() == Ok(env)
                            && self.secrets.len() == stored_secrets(base).count()
                            && self.secrets.iter().all(|(name, change)| {
                                *change == SecretChange::Keep && stored_secrets(base).any(|stored| stored == name)
                            })
                            && self.secret_fields_empty(cx)
                    }
                }
        })
    }
}

impl Shell {
    /// Sends the LLM draft. The server refuses it if its endpoint is no longer the base,
    /// unless `anyway` is true.
    fn save_llm(&mut self, remove_token: bool, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.connections;
        let Some(profile) = form.profile.clone() else {
            return;
        };
        let Some(token) = token_change(&form.llm_token, remove_token, cx) else {
            form.llm_message = Some(TYPED_TOKEN);
            return;
        };
        // The form has no timeout field, so the server's value stays.
        let timeout_secs = match self.state.connections.as_ref().and_then(|c| c.llm.as_ref()) {
            Some(llm) => llm.timeout_secs,
            None => default_timeout_secs(),
        };
        form.llm_message = None;
        form.llm_conflict = false;
        form.llm_remove_token = remove_token;
        let expected = if anyway {
            Expected::Unchecked
        } else {
            Expected::Value(form.llm_base.clone())
        };
        let update = LlmUpdate {
            wire: form.wire,
            base_url: text(&form.llm_address, cx),
            model: text(&form.llm_model, cx),
            timeout_secs,
            token,
            expected,
        };
        self.send(UiCommand::SaveLlm { profile, update });
    }

    /// Drops the LLM draft and shows the server's endpoint.
    fn load_server_llm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(connections) = &self.state.connections {
            self.connections.load_llm(connections.llm.as_ref(), window, cx);
        }
    }

    /// Sends the connector draft. The server refuses it if the connector is no longer the base,
    /// unless `anyway` is true.
    fn save_connector(&mut self, remove_token: bool, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.connections;
        let Some(profile) = form.profile.clone() else {
            return;
        };
        let name = text(&form.name, cx);
        // A name keeps its kind. The server refuses the change too. This message comes first,
        // because the conflict warning would offer a "Save anyway" that cannot work.
        let other_kind = self
            .state
            .connections
            .as_ref()
            .and_then(|c| c.connectors.get(&name))
            .is_some_and(|c| matches!(c.mcp, McpConfig::Stdio { .. }) != (form.kind == Kind::Command));
        if other_kind {
            form.connector_message = Some(KIND_TAKEN);
            return;
        }
        // The base belongs to one name. Under another name the draft is a new connector.
        let base = form.connector_base.as_ref().filter(|(base, _)| *base == name);
        let expected = if anyway {
            Expected::Unchecked
        } else {
            Expected::Value(base.map(|(_, connector)| connector.clone()))
        };
        if form.kind == Kind::Command {
            let env = match parse_env(&ConnectionsForm::raw(&form.env, cx)) {
                Ok(env) => env,
                Err(message) => {
                    form.connector_message = Some(message);
                    return;
                }
            };
            // A typed variable that Add did not take would not be saved.
            if !form.secret_fields_empty(cx) {
                form.connector_message = Some(SECRET_UNADDED);
                return;
            }
            let update = CommandConnectorUpdate {
                command: text(&form.command, cx).into(),
                args: parse_args(&ConnectionsForm::raw(&form.args, cx)),
                env,
                secret_env: form.secrets.clone(),
                expected,
            };
            form.connector_message = None;
            form.connector_conflict = None;
            self.send(UiCommand::SaveCommandConnector { profile, name, update });
            return;
        }
        let Some(token) = token_change(&form.token, remove_token, cx) else {
            form.connector_message = Some(TYPED_TOKEN);
            return;
        };
        form.connector_message = None;
        form.connector_conflict = None;
        form.connector_remove_token = remove_token;
        let update = ConnectorUpdate {
            url: text(&form.address, cx),
            token,
            token_header: Some(text(&form.token_header, cx).trim().to_owned()).filter(|name| !name.is_empty()),
            expected,
        };
        self.send(UiCommand::SaveConnector { profile, name, update });
    }

    /// Copies the password of the named Keychain item into the masked token field.
    /// Nothing goes to the server before Save.
    fn read_keychain(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = &mut self.connections;
        let service = text(&form.keychain_service, cx).trim().to_owned();
        let account = text(&form.keychain_account, cx).trim().to_owned();
        if service.is_empty() || account.is_empty() {
            form.keychain_message = Some(KEYCHAIN_FIELDS.to_owned());
            return;
        }
        let found = self.keychain.password(&service, &account);
        // A token from an earlier read must not stay in the field beside "was not read".
        if !matches!(&found, Ok(password) if !password.trim().is_empty()) {
            set(&form.llm_token, "", window, cx);
        }
        form.keychain_message = Some(match found {
            Ok(password) if password.trim().is_empty() => KEYCHAIN_EMPTY.to_owned(),
            Ok(password) => {
                // A save reads every field without the space around its text, so a line end
                // after the password does not reach the server.
                set(&form.llm_token, &password, window, cx);
                KEYCHAIN_READ.to_owned()
            }
            Err(error) => format!("The token was not read: {error}."),
        });
    }

    /// Tests the saved endpoint. A draft is not tested, so the form must equal the server's settings.
    fn test_llm(&mut self, cx: &mut Context<Self>) {
        let form = &mut self.connections;
        let (Some(profile), Some(connections)) = (form.profile.clone(), &self.state.connections) else {
            return;
        };
        if form.llm_differs(connections.llm.as_ref(), cx) {
            form.llm_message = Some(SAVE_FIRST);
            return;
        }
        form.llm_message = None;
        form.llm_testing = true;
        self.send(UiCommand::TestLlm { profile });
    }

    /// Drops the connector draft and shows the server's connector of the same name.
    fn load_server_connector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(connections) = &self.state.connections {
            let name = text(&self.connections.name, cx);
            self.connections
                .load_connector(&name, connections.connectors.get(&name), window, cx);
        }
    }

    /// One saved connector with its controls, and the result of Check or Approve if it was for this one.
    fn connector_row(
        &self,
        profile: &str,
        name: &str,
        connector: &ConnectorConnection,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let send = |command: UiCommand| cx.listener(move |this: &mut Self, _, _, _| this.send(command.clone()));
        let check = UiCommand::CheckConnector {
            profile: profile.to_owned(),
            name: name.to_owned(),
        };
        let mut buttons = div().flex().gap_2().child(
            Button::new("check")
                .small()
                .outline()
                .label("Check")
                .on_click(send(check)),
        );
        let mut row = div()
            .id(SharedString::from(name.to_owned()))
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(labelled_as(
                "name",
                name.to_owned(),
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(name.to_owned())),
            ));
        match &connector.mcp {
            McpConfig::Http { url } => {
                row = row
                    .child(labelled_as("address", url.clone(), quiet(url.clone(), cx)))
                    .child(labelled_as(
                        "token",
                        token_text(connector.token),
                        quiet(token_text(connector.token), cx),
                    ))
                    .children(connector.token_header.as_ref().map(|header| {
                        let text = format!("sent in the header {header}");
                        labelled_as("token-header", text.clone(), quiet(text, cx))
                    }));
            }
            // The row shows the program and how much it gets. The arguments and the variables can
            // hold a secret, so they are shown only in the form, after Edit.
            McpConfig::Stdio { command, args, env } => {
                let runs = format!("runs the command {}", command.display());
                let counts = counts_text(args.len(), env.len(), connector.secret_env.len());
                row = row
                    .child(labelled_as("address", runs.clone(), quiet(runs, cx)))
                    .child(labelled_as("command-counts", counts.clone(), quiet(counts, cx)));
            }
        }
        let (edit_name, edited) = (name.to_owned(), connector.clone());
        let remove = UiCommand::RemoveConnector {
            profile: profile.to_owned(),
            name: name.to_owned(),
        };
        buttons = buttons
            .child(Button::new("edit").small().ghost().label("Edit").on_click(cx.listener(
                move |this, _, window, cx| {
                    this.connections.load_connector(&edit_name, Some(&edited), window, cx);
                    cx.notify();
                },
            )))
            .child(
                Button::new("remove")
                    .small()
                    .ghost()
                    .label("Remove")
                    .on_click(send(remove)),
            );
        row = row.child(buttons);
        match &self.state.connection_result {
            Some(ConnectionResult::Checked { name: checked, result }) if checked == name => match result {
                Ok(report) => {
                    row = row.child(labelled("tools", tools_text(report)));
                    if !report.diff.is_clean() {
                        // The fingerprints of the list on screen. The server refuses them if the list changed meanwhile.
                        let approve = UiCommand::ApproveTools {
                            profile: profile.to_owned(),
                            name: name.to_owned(),
                            shown: pin(&report.tools),
                        };
                        row = row
                            .child(
                                Button::new("approve-tools")
                                    .small()
                                    .primary()
                                    .label("Approve these tools")
                                    .on_click(send(approve)),
                            )
                            .child(labelled("approve-note", APPROVE_NOTE));
                    }
                }
                Err(error) => row = row.child(labelled("tools", error.clone())),
            },
            Some(ConnectionResult::ToolsApproved { name: approved, result }) if approved == name => {
                let text = match result {
                    Ok(()) => "Tools approved".to_owned(),
                    Err(error) => error.clone(),
                };
                row = row
                    .child(labelled("tools", text))
                    .child(labelled("approve-note", APPROVE_NOTE));
            }
            _ => {}
        }
        row
    }

    /// The connections section. It shows the forms only while the server's connections are known.
    pub(crate) fn connections_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cx = &*cx;
        let section = div()
            .id("connections")
            .w(px(520.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_4();
        let form = &self.connections;
        let (Some(server), Some(profile)) = (&self.state.connections, &form.profile) else {
            return section;
        };

        // The LLM endpoint.
        let llm = server.llm.as_ref();
        let llm_token = llm.map_or(TokenSource::None, |llm| llm.token);
        let llm_changed = form.llm_differs(llm, cx);
        // The result of the last test, or that a test runs now.
        let (test_text, test_works) = match &self.state.connection_result {
            _ if form.llm_testing => (TESTING.to_owned(), None),
            // A result describes the saved endpoint. Beside a changed form it would describe another one.
            _ if llm_changed => (String::new(), None),
            Some(ConnectionResult::LlmTested(Ok(test))) if test.works => {
                (format!("Works: {}", test.detail), Some(true))
            }
            Some(ConnectionResult::LlmTested(Ok(test))) => (format!("Does not work: {}", test.detail), Some(false)),
            Some(ConnectionResult::LlmTested(Err(error))) => (error.clone(), Some(false)),
            _ => (String::new(), None),
        };
        let test_color = match test_works {
            Some(true) => palette(cx).done,
            Some(false) => palette(cx).stopped,
            None => cx.theme().muted_foreground,
        };
        let llm_result = match &self.state.connection_result {
            Some(ConnectionResult::LlmSaved(Err(SaveError::Failed(error)))) => error.clone(),
            // "Saved" describes the form only while it equals the server's settings.
            Some(ConnectionResult::LlmSaved(Ok(()))) if !llm_changed => "Saved".to_owned(),
            _ => String::new(),
        };
        let selected_wire = WIRES.iter().position(|(wire, _)| *wire == form.wire);
        let mut llm_section =
            card(cx)
                .child(div().font_weight(FontWeight::SEMIBOLD).child("LLM endpoint"))
                .child(labelled_as("connections-help", HELP, quiet(HELP, cx)))
                .child(
                    RadioGroup::vertical("llm-wire")
                        .selected_index(selected_wire)
                        .children(WIRES.map(|(_, label)| label))
                        .on_click(cx.listener(|this, ix: &usize, window, cx| {
                            this.connections.choose_wire(WIRES[*ix].0, window, cx);
                            cx.notify();
                        })),
                )
                .child(field(
                    "Address",
                    Input::new(&form.llm_address)
                        .id("llm-address")
                        .aria_label("LLM address"),
                    cx,
                ))
                .child(field(
                    "Model",
                    Input::new(&form.llm_model).id("llm-model").aria_label("Model name"),
                    cx,
                ))
                .child(field(
                    "Token",
                    Input::new(&form.llm_token).id("llm-token").aria_label("LLM token"),
                    cx,
                ))
                // The Keychain fields open on request. Closed, they take one line.
                .child(
                    Button::new("open-keychain")
                        .self_start()
                        .small()
                        .ghost()
                        .label(if form.keychain_open {
                            "Hide the Keychain fields"
                        } else {
                            "Copy the token from the Keychain"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.connections.keychain_open = !this.connections.keychain_open;
                            cx.notify();
                        })),
                )
                .children(form.keychain_open.then(|| {
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(labelled_as("keychain-help", KEYCHAIN_HELP, quiet(KEYCHAIN_HELP, cx)))
                        .child(
                            div()
                                .flex()
                                .items_end()
                                .gap_2()
                                .child(
                                    field(
                                        "Where (service)",
                                        Input::new(&form.keychain_service)
                                            .id("keychain-service")
                                            .aria_label("Keychain service"),
                                        cx,
                                    )
                                    .flex_1(),
                                )
                                .child(
                                    field(
                                        "Account",
                                        Input::new(&form.keychain_account)
                                            .id("keychain-account")
                                            .aria_label("Keychain account"),
                                        cx,
                                    )
                                    .flex_1(),
                                )
                                .child(
                                    Button::new("read-keychain")
                                        .outline()
                                        .label("Read from Keychain")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.read_keychain(window, cx);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(labelled(
                            "keychain-message",
                            form.keychain_message.clone().unwrap_or_default(),
                        )))
                }))
                .child(labelled_as(
                    "llm-token-state",
                    token_text(llm_token),
                    quiet(format!("Now: {}", token_text(llm_token)), cx),
                ))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(Button::new("save-llm").primary().label("Save").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.save_llm(false, false, cx);
                                cx.notify();
                            },
                        )))
                        .child(Button::new("test-llm").outline().label("Test").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.test_llm(cx);
                                cx.notify();
                            },
                        ))),
                )
                .child(labelled_as(
                    "llm-test",
                    test_text.clone(),
                    div().children((!test_text.is_empty()).then(|| {
                        div()
                            .text_sm()
                            .text_color(test_color)
                            .child(SharedString::from(test_text))
                    })),
                ));
        if llm_token == TokenSource::Stored {
            llm_section = llm_section.child(
                Button::new("remove-llm-token")
                    .self_start()
                    .ghost()
                    .label("Remove stored token")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_llm(true, false, cx);
                        cx.notify();
                    })),
            );
        }
        let llm_section = llm_section
            // The status texts share one box, so empty ones leave no band of gaps.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .text_sm()
                    .child(labelled("llm-message", form.llm_message.unwrap_or_default()))
                    .child(labelled("llm-result", llm_result)),
            )
            .children(form.llm_conflict.then(|| {
                Self::conflict_warning(
                    "llm",
                    CONFLICT,
                    Self::load_server_llm,
                    |this, cx| this.save_llm(this.connections.llm_remove_token, true, cx),
                    cx,
                )
            }));

        // The saved connectors, each in its own ID scope.
        let rows: Vec<_> = server
            .connectors
            .iter()
            .map(|(name, connector)| self.connector_row(profile, name, connector, cx))
            .collect();

        // The connector form. Edit fills it. Save adds or changes the named connector.
        let editing = server.connectors.get(&text(&form.name, cx));
        let connector_result = match &self.state.connection_result {
            Some(ConnectionResult::ConnectorSaved { name, result: Ok(()) }) => format!("Saved: {name}"),
            Some(ConnectionResult::ConnectorRemoved { name, result: Ok(()) }) => format!("Removed: {name}"),
            Some(ConnectionResult::ConnectorSaved {
                result: Err(SaveError::Failed(error)),
                ..
            })
            | Some(ConnectionResult::ConnectorRemoved { result: Err(error), .. }) => error.clone(),
            _ => String::new(),
        };
        let selected_kind = KINDS.iter().position(|(kind, _)| *kind == form.kind);
        // Only the fields of the chosen kind are shown.
        let kind_fields: Vec<AnyElement> = match form.kind {
            Kind::Address => vec![
                field(
                    "Address of the MCP server",
                    Input::new(&form.address)
                        .id("connector-address")
                        .aria_label("Connector address"),
                    cx,
                )
                .into_any_element(),
                field(
                    "Token. Leave it empty if the server needs none.",
                    Input::new(&form.token)
                        .id("connector-token")
                        .aria_label("Connector token"),
                    cx,
                )
                .into_any_element(),
                field(
                    "Header that carries the token. Leave it empty for Authorization: Bearer.",
                    Input::new(&form.token_header)
                        .id("connector-token-header")
                        .aria_label("Header for the token"),
                    cx,
                )
                .into_any_element(),
            ],
            Kind::Command => {
                // The rows name each secret variable. A value is never on screen.
                let secret_rows: Vec<AnyElement> = form
                    .secrets
                    .iter()
                    .map(|(name, change)| {
                        let state = match change {
                            SecretChange::Keep => "stored on the server",
                            SecretChange::Set(_) => "new value, not saved yet",
                        };
                        let removed = name.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(labelled(format!("secret-{name}"), format!("{name}: {state}")))
                            .child(
                                Button::new(SharedString::from(format!("remove-secret-{name}")))
                                    .small()
                                    .ghost()
                                    .label("Remove")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.connections.secrets.remove(&removed);
                                        cx.notify();
                                    })),
                            )
                            .into_any_element()
                    })
                    .collect();
                // The settings can name a secret variable that has no value yet.
                let missing = match &form.connector_base {
                    Some((_, base)) if !base.secret_env_missing.is_empty() => format!(
                        "No value is stored for: {}. Add each with its value. A save without it removes the variable.",
                        base.secret_env_missing.join(", ")
                    ),
                    _ => String::new(),
                };
                let args = parse_args(&ConnectionsForm::raw(&form.args, cx)).len();
                let counts = match parse_env(&ConnectionsForm::raw(&form.env, cx)) {
                    Ok(env) => counts_text(args, env.len(), form.secrets.len()),
                    Err(message) => message.to_owned(),
                };
                vec![
                    labelled_as("command-help", COMMAND_HELP, quiet(COMMAND_HELP, cx)).into_any_element(),
                    field(
                        "Command: the absolute path of the program only",
                        Input::new(&form.command).id("connector-command").aria_label("Command"),
                        cx,
                    )
                    .into_any_element(),
                    // The text areas have no ID of their own. Tests find them inside these elements.
                    field(
                        "Arguments: one in each line, without quotes",
                        div()
                            .id("connector-args")
                            .child(Textarea::new(&form.args).aria_label("Arguments").h(px(76.))),
                        cx,
                    )
                    .into_any_element(),
                    field(
                        "Environment variables that are not secret: NAME=value, one in each line",
                        div()
                            .id("connector-env")
                            .child(Textarea::new(&form.env).aria_label("Environment variables").h(px(76.))),
                        cx,
                    )
                    .into_any_element(),
                    labelled_as("secret-help", SECRET_HELP, quiet(SECRET_HELP, cx)).into_any_element(),
                    div()
                        .text_sm()
                        .text_color(palette(cx).waiting)
                        .child(labelled("secret-missing", missing))
                        .into_any_element(),
                    div().flex().flex_col().gap_1().children(secret_rows).into_any_element(),
                    div()
                        .flex()
                        .items_end()
                        .gap_2()
                        .child(
                            field(
                                "Name of the secret variable",
                                Input::new(&form.secret_name)
                                    .id("secret-name")
                                    .aria_label("Name of the secret variable"),
                                cx,
                            )
                            .flex_1(),
                        )
                        .child(
                            field(
                                "Value",
                                // Masked, with no toggle that would show the value.
                                Input::new(&form.secret_value)
                                    .id("secret-value")
                                    .aria_label("Value of the secret variable"),
                                cx,
                            )
                            .flex_1(),
                        )
                        .child(Button::new("add-secret").outline().label("Add").on_click(cx.listener(
                            |this, _, window, cx| {
                                this.connections.add_secret(window, cx);
                                cx.notify();
                            },
                        )))
                        .into_any_element(),
                    labelled_as("connector-counts", counts.clone(), quiet(counts, cx)).into_any_element(),
                ]
            }
        };
        let mut connector_form = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(quiet("Add a connector. To change one, use its name or Edit.", cx))
            .child(
                RadioGroup::horizontal("connector-kind")
                    .selected_index(selected_kind)
                    .children(KINDS.map(|(_, label)| label))
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.connections.kind = KINDS[*ix].0;
                        cx.notify();
                    })),
            )
            .child(field(
                "Name",
                Input::new(&form.name).id("connector-name").aria_label("Connector name"),
                cx,
            ))
            .children(kind_fields)
            .child(
                Button::new("save-connector")
                    .self_start()
                    .primary()
                    .label("Save connector")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_connector(false, false, cx);
                        cx.notify();
                    })),
            );
        if editing.is_some_and(|c| c.token == TokenSource::Stored && matches!(c.mcp, McpConfig::Http { .. })) {
            connector_form = connector_form.child(
                Button::new("remove-connector-token")
                    .self_start()
                    .ghost()
                    .label("Remove stored token")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_connector(true, false, cx);
                        cx.notify();
                    })),
            );
        }
        let connector_form = connector_form
            .child(labelled(
                "connector-message",
                form.connector_message.unwrap_or_default(),
            ))
            .child(labelled("connector-result", connector_result))
            // The warning belongs to the connector named in the form.
            .children(
                (form.connector_conflict.as_ref() == Some(&text(&form.name, cx))).then(|| {
                    Self::conflict_warning(
                        "connector",
                        CONFLICT,
                        Self::load_server_connector,
                        |this, cx| this.save_connector(this.connections.connector_remove_token, true, cx),
                        cx,
                    )
                }),
            );

        let count = server.connectors.len();
        section.child(llm_section).child(
            card(cx)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("Connectors"))
                        .child(labelled_as(
                            "connector-count",
                            format!("Connectors: {count}"),
                            quiet(
                                if count == 0 {
                                    "None yet".to_owned()
                                } else {
                                    format!("{count} saved")
                                },
                                cx,
                            ),
                        )),
                )
                .child(div().id("connector-list").flex().flex_col().gap_2().children(rows))
                .child(connector_form),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunsin_core::tool::{PinDiff, ToolSpec};

    fn report(tools: &[&str], diff: PinDiff) -> ToolsReport {
        let tools = tools
            .iter()
            .map(|name| ToolSpec {
                name: (*name).into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            })
            .collect();
        ToolsReport { tools, diff }
    }

    #[test]
    fn arguments_are_lines_and_variables_split_at_the_first_equals_sign() {
        assert_eq!(
            parse_args("-y\n\n some server \r\n--flag=a b"),
            ["-y", " some server ", "--flag=a b"]
        );
        assert!(parse_args("").is_empty());
        let env = parse_env("TOKEN=a=b\nEMPTY=\n\nSPACED= x ").unwrap();
        let expected = [("EMPTY", ""), ("SPACED", " x "), ("TOKEN", "a=b")];
        assert_eq!(
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(parse_env("TOKEN"), Err(ENV_LINE));
        assert_eq!(parse_env("=value"), Err(ENV_LINE));
        assert_eq!(parse_env("A=1\nA=2"), Err(ENV_TWICE));
    }

    #[test]
    fn a_stored_command_is_shown_only_if_the_form_gives_it_back_unchanged() {
        let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
        };
        let args = vec!["-y".to_owned(), "some server".to_owned()];
        let stored = env(&[("TOKEN", "a=b"), ("EMPTY", "")]);
        let (args_text, env_text) = command_texts(&args, &stored).unwrap();
        // What the form shows is parsed back to what was stored.
        assert_eq!(parse_args(&args_text), args);
        assert_eq!(parse_env(&env_text).unwrap(), stored);
        // The form cannot hold these, so it does not show the connector.
        assert_eq!(command_texts(&[String::new()], &env(&[])), None);
        assert_eq!(command_texts(&["two\nlines".to_owned()], &env(&[])), None);
        assert_eq!(command_texts(&[], &env(&[("TOKEN", "line\nbreak")])), None);
        assert_eq!(command_texts(&[], &env(&[("A=B", "v")])), None);
        assert_eq!(counts_text(1, 0, 0), "1 argument, 0 environment variables");
        assert_eq!(
            counts_text(0, 1, 2),
            "0 arguments, 1 environment variable, 2 secret variables"
        );
        assert_eq!(
            counts_text(2, 2, 1),
            "2 arguments, 2 environment variables, 1 secret variable"
        );
    }

    #[test]
    fn each_token_source_has_its_own_text() {
        assert_eq!(token_text(TokenSource::Stored), "token stored on the server");
        assert_eq!(
            token_text(TokenSource::ServerEnvironment),
            "token from the server's environment"
        );
        assert_eq!(token_text(TokenSource::None), "no token");
    }

    #[test]
    fn the_check_text_names_the_tools_that_differ() {
        assert_eq!(
            tools_text(&report(&["post"], PinDiff::default())),
            "works: 1 tool(s), the same as the approved list"
        );
        let diff = PinDiff {
            added: vec!["search".into(), "read".into()],
            removed: vec!["old".into()],
            changed: vec![],
        };
        assert_eq!(
            tools_text(&report(&["post", "search", "read"], diff)),
            "works: 3 tool(s); compared with the approved list: 2 new (search, read), 0 changed, 1 gone (old)"
        );
    }
}

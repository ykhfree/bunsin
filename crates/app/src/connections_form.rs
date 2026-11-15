//! The connections section of the main window: the LLM endpoint and the connectors
//! of the selected profile. The fields are the user's draft. Only Save sends them.
//! A typed token goes from its masked field into the command, and nowhere else.

use crate::bridge::{ConnectionResult, SaveError, UiCommand, UiState};
use crate::shell::{CONFLICT, Shell, labelled};
use bunsin_api::{
    ConnectorConnection, ConnectorUpdate, Expected, LlmConnection, LlmUpdate, TokenChange, TokenSource, ToolsReport,
};
use bunsin_core::settings::{ANTHROPIC_BASE_URL, ANTHROPIC_DEFAULT_MODEL, McpConfig, Wire, default_timeout_secs};
use bunsin_core::tool::pin;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::{
    App, AppContext, Context, Div, Entity, IntoElement, ParentElement, SharedString, Stateful, Styled, Window, div,
    prelude::*, px,
};

/// The API formats, in the order of the choices on screen.
const WIRES: [(Wire, &str); 2] = [
    (Wire::AnthropicMessages, "Anthropic Messages"),
    (Wire::OpenAiChat, "OpenAI Chat Completions"),
];

const HELP: &str = "Leave a token field empty to keep the stored token. \
    The server never sends a token back.";
const STDIO: &str = "runs as a command on the server; set by the server operator";
const STDIO_REFUSED: &str = "This connector runs as a command on the server. Only the server operator can change it.";
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

/// The address of a connector that the form can edit.
fn http_url(connector: &ConnectorConnection) -> Option<&str> {
    match &connector.mcp {
        McpConfig::Http { url } => Some(url),
        McpConfig::Stdio { .. } => None,
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
            llm_message: None,
            connector_message: None,
            llm_base: None,
            llm_conflict: false,
            llm_remove_token: false,
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
        if self.profile != state.selected {
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
        let url = connector.and_then(http_url);
        set(&self.name, if url.is_some() { name } else { "" }, window, cx);
        set(&self.address, url.unwrap_or_default(), window, cx);
        set(&self.token, "", window, cx);
        self.connector_base = connector
            .filter(|_| url.is_some())
            .map(|c| (name.to_owned(), c.clone()));
        self.connector_message = None;
        self.connector_conflict = None;
    }

    /// True if the connector fields show the base and the token field is empty.
    fn connector_untouched(&self, cx: &App) -> bool {
        self.connector_base.as_ref().is_some_and(|(name, base)| {
            text(&self.name, cx) == *name
                && http_url(base) == Some(text(&self.address, cx).as_str())
                && text(&self.token, cx).is_empty()
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
        // The server would replace a stdio connector with this HTTP one.
        let stdio = self
            .state
            .connections
            .as_ref()
            .and_then(|c| c.connectors.get(&name))
            .is_some_and(|c| matches!(c.mcp, McpConfig::Stdio { .. }));
        if stdio {
            form.connector_message = Some(STDIO_REFUSED);
            return;
        }
        let Some(token) = token_change(&form.token, remove_token, cx) else {
            form.connector_message = Some(TYPED_TOKEN);
            return;
        };
        form.connector_message = None;
        form.connector_conflict = None;
        form.connector_remove_token = remove_token;
        let expected = if anyway {
            Expected::Unchecked
        } else {
            // The base belongs to one name. Under another name the draft is a new connector.
            let base = form.connector_base.as_ref().filter(|(base, _)| *base == name);
            Expected::Value(base.map(|(_, connector)| connector.clone()))
        };
        let update = ConnectorUpdate {
            url: text(&form.address, cx),
            token,
            expected,
        };
        self.send(UiCommand::SaveConnector { profile, name, update });
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
        let mut buttons = div()
            .flex()
            .gap_2()
            .child(Button::new("check").label("Check").on_click(send(check)));
        let mut row = div()
            .id(SharedString::from(name.to_owned()))
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_1()
            .child(labelled("name", name.to_owned()));
        match &connector.mcp {
            McpConfig::Http { url } => {
                row = row
                    .child(labelled("address", url.clone()))
                    .child(labelled("token", token_text(connector.token)));
                let (edit_name, edited) = (name.to_owned(), connector.clone());
                let remove = UiCommand::RemoveConnector {
                    profile: profile.to_owned(),
                    name: name.to_owned(),
                };
                buttons = buttons
                    .child(
                        Button::new("edit")
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.connections.load_connector(&edit_name, Some(&edited), window, cx);
                                cx.notify();
                            })),
                    )
                    .child(Button::new("remove").label("Remove").on_click(send(remove)));
            }
            McpConfig::Stdio { .. } => row = row.child(labelled("address", STDIO)),
        }
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
        let section = div()
            .id("connections")
            .w(px(380.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .overflow_y_scroll()
            .child("Connections")
            .child(labelled("connections-help", HELP));
        let form = &self.connections;
        let (Some(server), Some(profile)) = (&self.state.connections, &form.profile) else {
            return section;
        };

        // The LLM endpoint.
        let llm = server.llm.as_ref();
        let llm_token = llm.map_or(TokenSource::None, |llm| llm.token);
        let llm_changed = match llm {
            Some(llm) => {
                form.wire != llm.wire
                    || text(&form.llm_address, cx) != llm.base_url
                    || text(&form.llm_model, cx) != llm.model
                    || !text(&form.llm_token, cx).is_empty()
            }
            None => true,
        };
        let llm_result = match &self.state.connection_result {
            Some(ConnectionResult::LlmSaved(Err(SaveError::Failed(error)))) => error.clone(),
            // "Saved" describes the form only while it equals the server's settings.
            Some(ConnectionResult::LlmSaved(Ok(()))) if !llm_changed => "Saved".to_owned(),
            _ => String::new(),
        };
        let selected_wire = WIRES.iter().position(|(wire, _)| *wire == form.wire);
        let mut llm_section = div()
            .flex()
            .flex_col()
            .gap_2()
            .child("LLM endpoint")
            .child(
                RadioGroup::horizontal("llm-wire")
                    .selected_index(selected_wire)
                    .children(WIRES.map(|(_, label)| label))
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.connections.wire = WIRES[*ix].0;
                        cx.notify();
                    })),
            )
            .child(
                Input::new(&form.llm_address)
                    .id("llm-address")
                    .aria_label("LLM address"),
            )
            .child(Input::new(&form.llm_model).id("llm-model").aria_label("Model name"))
            .child(Input::new(&form.llm_token).id("llm-token").aria_label("LLM token"))
            .child(labelled("llm-token-state", token_text(llm_token)))
            .child(
                Button::new("save-llm")
                    .label("Save")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_llm(false, false, cx);
                        cx.notify();
                    })),
            );
        if llm_token == TokenSource::Stored {
            llm_section = llm_section.child(Button::new("remove-llm-token").label("Remove stored token").on_click(
                cx.listener(|this, _, _, cx| {
                    this.save_llm(true, false, cx);
                    cx.notify();
                }),
            ));
        }
        let llm_section = llm_section
            .child(labelled("llm-message", form.llm_message.unwrap_or_default()))
            .child(labelled("llm-result", llm_result))
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
        let mut connector_form = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(Input::new(&form.name).id("connector-name").aria_label("Connector name"))
            .child(
                Input::new(&form.address)
                    .id("connector-address")
                    .aria_label("Connector address"),
            )
            .child(
                Input::new(&form.token)
                    .id("connector-token")
                    .aria_label("Connector token"),
            )
            .child(
                Button::new("save-connector")
                    .label("Save connector")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_connector(false, false, cx);
                        cx.notify();
                    })),
            );
        if editing.is_some_and(|c| c.token == TokenSource::Stored && matches!(c.mcp, McpConfig::Http { .. })) {
            connector_form = connector_form.child(
                Button::new("remove-connector-token")
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

        section
            .child(llm_section)
            .child("Connectors")
            .child(labelled(
                "connector-count",
                format!("Connectors: {}", server.connectors.len()),
            ))
            .child(div().id("connector-list").flex().flex_col().gap_2().children(rows))
            .child(connector_form)
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

//! `unharnessed login [provider]` and `unharnessed logout [provider]`: the
//! provider sign-ins in a terminal. Same flows as the VS Code panel
//! (`provider_login`), with the terminal as the auth panel: progress lines
//! print, the sign-in page opens in the browser, prompts read a line.

use tokio::sync::mpsc;

use crate::auth_panel::{AuthPanelHandle, AuthPanelRequest, PasteStyle, PrimeTeamPick};
use crate::provider_auth::{
    AuthFlow, AuthStatusStyle, AuthType, ProviderAuthCommands, ProviderAuthOutcome, ProviderRow,
};
use crate::provider_login::ProviderAuth;
use crate::terminal::{prompt, set_echo, Input, LineInput, Style};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Login,
    Logout,
}

/// `unharnessed login [provider]`.
pub(crate) fn run_login_command(args: &[String]) -> i32 {
    run_auth_command(args, Action::Login)
}

/// `unharnessed logout [provider]`.
pub(crate) fn run_logout_command(args: &[String]) -> i32 {
    run_auth_command(args, Action::Logout)
}

fn run_auth_command(args: &[String], action: Action) -> i32 {
    let name = match action {
        Action::Login => "login",
        Action::Logout => "logout",
    };
    let mut wanted: Option<&str> = None;
    for arg in args {
        if arg.starts_with('-') {
            eprintln!("Error: unknown option {arg} for {name}");
            return 1;
        }
        if wanted.is_some() {
            eprintln!("Error: {name} takes at most one provider");
            return 1;
        }
        wanted = Some(arg);
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };
    let auth = ProviderAuth::new(cwd, crate::config::get_agent_dir());
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };
    let style = Style::detect();
    let result = runtime.block_on(async {
        let mut input = LineInput::start();
        match action {
            Action::Login => sign_in(&auth, wanted, &mut input, style).await,
            Action::Logout => sign_out(&auth, wanted, &mut input, style).await,
        }
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    match result {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(message) => {
            eprintln!("{}", style.red(&format!("Error: {message}")));
            1
        }
    }
}

/// Sign in to one provider: `wanted` names it by id, otherwise a numbered
/// menu asks. `Ok(true)` once a credential is stored, `Ok(false)` when the
/// person cancelled.
pub(crate) async fn sign_in(
    auth: &ProviderAuth,
    wanted: Option<&str>,
    input: &mut LineInput,
    style: Style,
) -> Result<bool, String> {
    let rows = auth.login_options().await;
    let candidates = matching_rows(rows, wanted, "login")?;
    let Some(row) = choose(&candidates, "Sign in to a model provider:", input, style).await else {
        println!("Sign-in cancelled.");
        return Ok(false);
    };
    if !row.available {
        return Err(format!(
            "{} sign-in is not available in this build",
            row.name
        ));
    }
    let outcome = match row.flow {
        AuthFlow::ApiKeyPrompt => {
            let hidden = cfg!(unix);
            println!(
                "Paste your {} API key{}.",
                row.name,
                if hidden {
                    ""
                } else {
                    " (it shows as you type)"
                }
            );
            match read_line("API key:", hidden, input).await {
                Some(key) if !key.trim().is_empty() => auth.login(&row, Some(key.trim())).await,
                _ => ProviderAuthOutcome::Cancelled,
            }
        }
        AuthFlow::TerminalFlow => run_panel_flow(auth, &row, input, style).await,
    };
    let signed_in = report(outcome, style)?;
    if signed_in && row.id == "anthropic" {
        if let Some(warning) = auth.anthropic_subscription_warning().await {
            println!("{}", style.yellow(warning));
        }
    }
    Ok(signed_in)
}

async fn sign_out(
    auth: &ProviderAuth,
    wanted: Option<&str>,
    input: &mut LineInput,
    style: Style,
) -> Result<bool, String> {
    let rows = auth.logout_options().await;
    if rows.is_empty() {
        println!("No stored credentials.");
        return Ok(true);
    }
    let candidates = matching_rows(rows, wanted, "logout")?;
    let Some(row) = choose(
        &candidates,
        "Remove the stored credential of:",
        input,
        style,
    )
    .await
    else {
        println!("Nothing removed.");
        return Ok(false);
    };
    report(auth.logout(&row).await, style)
}

/// The rows `wanted` names (by provider id, case-insensitive), or all of
/// them.
fn matching_rows(
    rows: Vec<ProviderRow>,
    wanted: Option<&str>,
    command: &str,
) -> Result<Vec<ProviderRow>, String> {
    let Some(wanted) = wanted else {
        return Ok(rows);
    };
    let matching: Vec<ProviderRow> = rows
        .into_iter()
        .filter(|row| row.id.eq_ignore_ascii_case(wanted))
        .collect();
    if matching.is_empty() {
        return Err(format!(
            "no provider {wanted} for {command}; run `{} {command}` to see the list",
            crate::config::APP_NAME
        ));
    }
    Ok(matching)
}

/// The one row, or a numbered menu: subscriptions first, then API keys.
/// `None` when the person cancels (an empty answer or the end of input).
async fn choose(
    rows: &[ProviderRow],
    title: &str,
    input: &mut LineInput,
    style: Style,
) -> Option<ProviderRow> {
    if rows.len() == 1 {
        return Some(rows[0].clone());
    }
    let ordered: Vec<&ProviderRow> = rows
        .iter()
        .filter(|row| row.auth_type == AuthType::Oauth)
        .chain(rows.iter().filter(|row| row.auth_type == AuthType::ApiKey))
        .collect();
    println!("{}", style.bold(title));
    let width = ordered.len().to_string().len();
    let mut heading: Option<AuthType> = None;
    for (index, row) in ordered.iter().enumerate() {
        if heading != Some(row.auth_type) {
            heading = Some(row.auth_type);
            let label = match row.auth_type {
                AuthType::Oauth => "Subscriptions (sign in with your browser)",
                AuthType::ApiKey => "API keys",
            };
            println!("\n  {}", style.dim(label));
        }
        let status = match (&row.status, row.available) {
            (_, false) => style.dim("not available"),
            (Some(status), true) => match status.style {
                AuthStatusStyle::Success => style.green(&status.label),
                AuthStatusStyle::Warning => style.yellow(&status.label),
                AuthStatusStyle::Muted => style.dim(&status.label),
            },
            (None, true) => String::new(),
        };
        println!(
            "  {:>width$}. {:<44} {status}",
            index + 1,
            row.name,
            width = width
        );
    }
    println!();
    loop {
        let answer = read_line(
            &format!("Number (1-{}, Enter to cancel):", ordered.len()),
            false,
            input,
        )
        .await?;
        let answer = answer.trim();
        if answer.is_empty() {
            return None;
        }
        match answer.parse::<usize>() {
            Ok(number) if (1..=ordered.len()).contains(&number) => {
                let row = ordered[number - 1];
                if row.available {
                    return Some(row.clone());
                }
                println!("{} sign-in is not available in this build.", row.name);
            }
            _ => println!("Type a number from 1 to {}.", ordered.len()),
        }
    }
}

/// Read one line after `label`. `None` at the end of input. Ctrl+C quits
/// the process (exit code 130), with the terminal's echo back on.
async fn read_line(label: &str, secret: bool, input: &mut LineInput) -> Option<String> {
    // Echo off before the prompt shows: a key pasted at once stays hidden.
    let hidden = secret && set_echo(false);
    prompt(&format!("{label} "));
    let event = input.next().await;
    finish_read(hidden, &event);
    match event {
        Input::Line(line) => Some(line),
        Input::Eof => None,
        Input::Interrupt => quit_on_interrupt(),
    }
}

/// After a read: echo back on, and the line break the hidden input did not
/// echo.
fn finish_read(hidden: bool, event: &Input) {
    if hidden {
        set_echo(true);
        println!();
    } else if !matches!(event, Input::Line(_)) {
        println!();
    }
}

fn quit_on_interrupt() -> ! {
    set_echo(true);
    println!();
    std::process::exit(130);
}

/// Run a browser or device sign-in with the terminal as its panel.
async fn run_panel_flow(
    auth: &ProviderAuth,
    row: &ProviderRow,
    input: &mut LineInput,
    style: Style,
) -> ProviderAuthOutcome {
    let (requests, mut inbox) = mpsc::unbounded_channel();
    let flow = auth.login_on_panel(row, AuthPanelHandle::new(requests));
    tokio::pin!(flow);
    loop {
        tokio::select! {
            outcome = &mut flow => {
                // The flow is done: show what it sent last, then stop.
                while let Ok(request) = inbox.try_recv() {
                    show_request(request, input, style).await;
                }
                return outcome;
            }
            request = inbox.recv() => match request {
                Some(request) => show_request(request, input, style).await,
                // Every handle is gone: the flow is about to settle.
                None => return flow.await,
            },
            event = input.next() => {
                // Nothing asked for a line: a stray Enter is dropped.
                if event == Input::Interrupt {
                    quit_on_interrupt();
                }
            }
        }
    }
}

async fn show_request(request: AuthPanelRequest, input: &mut LineInput, style: Style) {
    match request {
        AuthPanelRequest::Progress { message, .. } => println!("{}", style.dim(&message)),
        AuthPanelRequest::Waiting { message } => println!("{message}"),
        AuthPanelRequest::AuthUrl { url, instructions } => {
            println!(
                "{}",
                instructions.unwrap_or_else(|| "Complete the sign-in in your browser.".to_string())
            );
            println!("{}", style.cyan(&url));
            println!(
                "{}",
                style.dim("Opening your browser. If it does not open, copy the link above.")
            );
            uh_core::platform::browser::open_in_browser(&url);
        }
        AuthPanelRequest::PastePrompt {
            prompt: label,
            style: paste_style,
            allow_empty,
            mut reply,
            ..
        } => loop {
            let hidden = paste_style == PasteStyle::Masked && set_echo(false);
            prompt(&format!("{} ", label.trim_end()));
            let event = tokio::select! {
                // The flow went on without the answer (the browser sent it).
                () = reply.closed() => {
                    finish_read(hidden, &Input::Eof);
                    break;
                }
                event = input.next() => event,
            };
            finish_read(hidden, &event);
            match event {
                Input::Line(line) => {
                    let value = line.trim();
                    if value.is_empty() && !allow_empty {
                        continue;
                    }
                    let _ = reply.send(Some(value.to_string()));
                    break;
                }
                Input::Eof => {
                    let _ = reply.send(None);
                    break;
                }
                Input::Interrupt => quit_on_interrupt(),
            }
        },
        AuthPanelRequest::SelectTeam {
            teams,
            current,
            reply,
        } => {
            println!("{}", style.bold("Which account?"));
            let mark = |selected: bool| if selected { " (current)" } else { "" };
            println!("  0. Personal account{}", mark(current.is_none()));
            for (index, team) in teams.iter().enumerate() {
                println!(
                    "  {}. {}{}",
                    index + 1,
                    team.name,
                    mark(current.as_deref() == Some(team.team_id.as_str()))
                );
            }
            let pick = loop {
                let Some(answer) =
                    read_line("Number (Enter keeps the current one):", false, input).await
                else {
                    break PrimeTeamPick::Cancelled;
                };
                match answer.trim() {
                    "" => break PrimeTeamPick::Cancelled,
                    "0" => break PrimeTeamPick::PersonalAccount,
                    other => match other.parse::<usize>() {
                        Ok(number) if (1..=teams.len()).contains(&number) => {
                            break PrimeTeamPick::Team(teams[number - 1].clone());
                        }
                        _ => println!("Type a number from 0 to {}.", teams.len()),
                    },
                }
            };
            let _ = reply.send(pick);
        }
        AuthPanelRequest::ProviderSettled { .. } | AuthPanelRequest::McpSettled { .. } => {}
    }
}

/// Print the outcome: `Ok(true)` for a success, `Ok(false)` for a cancel.
fn report(outcome: ProviderAuthOutcome, style: Style) -> Result<bool, String> {
    match outcome {
        ProviderAuthOutcome::Status(message) => {
            println!("{}", style.green(&message));
            Ok(true)
        }
        ProviderAuthOutcome::Error(message) => Err(message),
        ProviderAuthOutcome::Cancelled => {
            println!("Cancelled.");
            Ok(false)
        }
    }
}

//! The interactive chat: `unharnessed` in a terminal, with no `-p`. One
//! session, one kernel, a conversation typed line by line.
//!
//! It runs on the same engine and the same turn boundaries as `-p`
//! (provider retry, compaction, goals, autonomous mode, wakes); only the
//! surface differs. The answer streams as it is written, each `ipython`
//! cell shows its code and the start of its output, Ctrl+C stops the turn,
//! and an idle session still wakes when a background `bash()` ends.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uh_agent::stream::AssistantMessageEvent;
use uh_agent::types::{AgentEvent, AgentMessage, Message, StopReason, ToolResultContent};
use uh_core::session_engine::engine::SessionEngine;
use uh_core::session_engine::wakes::Wake;
use uh_types::ai::Model;
use uh_types::slash_commands::{SlashCommandExecution, SlashCommandRegistry};

use crate::headless_autonomous::HeadlessAutonomous;
use crate::mode::RunOptions;
use crate::print_goal::PrintGoalSurface;
use crate::provider_auth::ProviderAuthCommands;
use crate::terminal::{prompt, Input, LineInput, Style};

/// Lines of a cell's code shown before the rest is summarised.
const CODE_LINES: usize = 12;
/// Lines of a cell's output shown before the rest is summarised.
const OUTPUT_LINES: usize = 8;
/// Characters of one shown line.
const LINE_CHARS: usize = 200;
/// Lines that arrive this close together are one paste, sent as one message.
const PASTE_WINDOW: Duration = Duration::from_millis(40);
/// A second Ctrl+C at the prompt within this window quits.
const QUIT_WINDOW: Duration = Duration::from_secs(2);
/// Ctrl+C presses during one turn that give up on it and quit.
const FORCE_QUIT_PRESSES: u32 = 3;

/// Run the chat. Returns the process exit code.
pub(crate) fn run(options: &RunOptions) -> Result<i32, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let code = runtime.block_on(chat_main(options));
    // The stdin reader may still be blocked in a read nothing can cancel.
    runtime.shutdown_timeout(Duration::from_secs(1));
    code
}

async fn chat_main(options: &RunOptions) -> Result<i32, String> {
    let style = Style::detect();
    let mut input = LineInput::start();
    let config = &options.config;

    // Nothing to talk to yet: sign in first, right here.
    if !has_usable_model(options).await {
        println!(
            "{}",
            style.bold("No model provider is set up yet. Sign in to one to start.")
        );
        let auth = crate::provider_login::ProviderAuth::new(&config.cwd, &config.agent_dir);
        match crate::login_command::sign_in(&auth, None, &mut input, style).await {
            Ok(true) => println!(),
            Ok(false) => return Ok(1),
            Err(message) => return Err(message),
        }
    }

    // The kernel setup (first run, or after an update) must not draw over
    // the chat: its steps print as dim lines, uv's own output is kept for
    // the error.
    uh_core::kernel::bootstrap::set_quiet_progress(Arc::new(move |message: &str| {
        println!("{}", style.dim(message));
    }));

    // A bare `-r` has no session picker here: it continues the last one.
    let mut options = options.clone();
    if options.session.resume_bare {
        options.session.resume_bare = false;
        options.session.continue_recent = true;
    }

    let headless = crate::print_runtime::build_headless_engine(&options).await?;
    let engine = Arc::new(headless.engine);
    if let Some(goal) = &options.config.initial_goal {
        engine
            .seed_initial_goal(&goal.objective, goal.token_budget.map(u64::from))
            .await
            .map_err(|error| format!("{error:#}"))?;
    }

    // The same wiring as `-p`, in text mode (the goal and autonomous
    // surfaces stay silent; the renderer below shows the turns).
    let goal = Arc::new(PrintGoalSurface::new(false));
    goal.seed_publish_baseline(&engine).await;
    let goal_accounting = goal.wire_accounting(&engine, engine.session.agent()).await;
    let autonomous = Arc::new(match options.config.autonomous.as_ref() {
        Some(autonomous) => HeadlessAutonomous::from_cli(autonomous, &options.config.cwd),
        None => HeadlessAutonomous::disabled(&options.config.cwd),
    });
    let accounting = autonomous.wire_accounting(engine.session.agent()).await;
    crate::print_autonomous::wire_continuation_hook(
        &engine,
        engine.session.agent(),
        &headless.model,
        &goal,
        &autonomous,
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let boundary = crate::print_boundary::TurnBoundary::new(false)
        .with_retry(crate::print_boundary::PrintRetry {
            cwd: options.config.cwd.clone(),
            agent_dir: options.config.agent_dir.clone(),
            provider_target: Arc::clone(&headless.provider_target),
            failover: std::env::var_os("UNHARNESSED_FAUX_SCRIPT").is_none(),
            max_park: Duration::from_mins(10),
        })
        .with_cancel(Arc::clone(&cancel));

    let renderer = Arc::new(Renderer::new(style));
    let listener = Arc::clone(&renderer);
    let subscription = engine
        .session
        .agent()
        .subscribe(move |event, _signal| {
            let listener = Arc::clone(&listener);
            Box::pin(async move {
                listener.on_event(&event);
                Ok(())
            })
        })
        .await;

    print_banner(&engine, &headless.model, &options, style).await;

    let mut chat = Chat {
        autonomous_state: autonomous.state_handle(),
        engine: Arc::clone(&engine),
        model: headless.model.clone(),
        api_key: headless.api_key.clone(),
        goal,
        autonomous,
        boundary,
        cancel,
        input,
        renderer,
        style,
        pending: VecDeque::new(),
        quit: false,
        last_idle_interrupt: None,
    };
    if let Some(first) = options.initial_message.clone() {
        chat.submit(first, options.initial_images.clone()).await;
    }
    for message in options.messages.clone() {
        if chat.quit {
            break;
        }
        chat.submit(message, Vec::new()).await;
    }
    chat.run().await;

    goal_accounting.unsubscribe().await;
    accounting.unsubscribe().await;
    subscription.unsubscribe().await;
    // The kernel's last snapshot, so `-c` brings the variables back.
    engine.dispose_kernel().await;
    if !options.session.no_session {
        println!(
            "{}",
            style.dim(&format!(
                "Session saved. Continue it with: {} -c",
                crate::config::APP_NAME
            ))
        );
    }
    Ok(0)
}

/// Whether a model with credentials exists (or the run brings its own key).
async fn has_usable_model(options: &RunOptions) -> bool {
    if std::env::var_os("UNHARNESSED_FAUX_SCRIPT").is_some() || options.config.api_key.is_some() {
        return true;
    }
    let agent_dir = options.config.agent_dir.clone();
    tokio::task::spawn_blocking(move || {
        let auth = uh_core::auth::AuthStorage::create(&agent_dir);
        let mut registry =
            uh_core::models::ModelRegistry::create(auth, agent_dir.join("models.json"));
        registry.load_private_authorization_from_cache();
        !registry.get_available().is_empty()
    })
    .await
    .unwrap_or(false)
}

async fn print_banner(engine: &SessionEngine, model: &Model, options: &RunOptions, style: Style) {
    println!(
        "{} {}",
        style.bold("Unharnessed"),
        style.dim(crate::config::version())
    );
    println!(
        "{}",
        style.dim(&format!(
            "{}/{} · {}",
            model.provider,
            model.id,
            options.config.cwd.display()
        ))
    );
    let resumed = engine.session.agent().state().await.messages.len();
    if resumed > 0 {
        println!(
            "{}",
            style.dim(&format!("Continuing a session of {resumed} messages."))
        );
    }
    let auth =
        crate::provider_login::ProviderAuth::new(&options.config.cwd, &options.config.agent_dir);
    if model.provider == "anthropic" {
        if let Some(warning) = auth.anthropic_subscription_warning().await {
            println!("{}", style.yellow(warning));
        }
    }
    println!(
        "{}",
        style.dim("The agent runs code with your permissions, without asking first.")
    );
    println!(
        "{}",
        style.dim("/help for commands · Ctrl+C stops the agent · Ctrl+D quits")
    );
}

const HELP: &str = "\
Commands:
  /help                  this help
  /exit, /quit           quit (Ctrl+D also quits)
  /compact [focus]       summarise the conversation now; the kernel keeps its variables
  /goal <objective>      a goal the agent works on until it is done
  /goal                  show the goal; /goal pause, /goal resume, /goal clear
  /autonomous on|off     keep going until your gates pass (see --help)

Keys:
  Ctrl+C                 stop the agent; twice at the prompt to quit
  \\ at the end of a line continue the message on the next line

Sign in or switch accounts with `unharnessed login` (then start again).
Pick a model at start with --model <id> (`unharnessed model list`).";

struct Chat {
    engine: Arc<SessionEngine>,
    model: Model,
    api_key: Option<String>,
    goal: Arc<PrintGoalSurface>,
    autonomous: Arc<HeadlessAutonomous>,
    autonomous_state: Arc<tokio::sync::Mutex<uh_core::autonomous::AutonomousRuntimeState>>,
    boundary: crate::print_boundary::TurnBoundary,
    cancel: Arc<AtomicBool>,
    input: LineInput,
    renderer: Arc<Renderer>,
    style: Style,
    /// Messages typed while a turn ran, sent once it ends.
    pending: VecDeque<String>,
    quit: bool,
    last_idle_interrupt: Option<Instant>,
}

/// What a turn starts from.
enum Start {
    Prompt(String, Vec<uh_agent::types::ImageContent>),
    Wake(Wake),
}

impl Chat {
    async fn run(&mut self) {
        while !self.quit {
            if let Some(message) = self.next_pending() {
                self.submit(message, Vec::new()).await;
                continue;
            }
            prompt(&format!("\n{} ", self.style.bold(">")));
            let event = tokio::select! {
                event = self.input.next() => Ok(event),
                wake = self.engine.next_wake(None) => Err(wake),
            };
            match event {
                Ok(Input::Line(line)) => {
                    self.last_idle_interrupt = None;
                    let Some(message) = self.read_message(line).await else {
                        continue;
                    };
                    self.submit(message, Vec::new()).await;
                }
                Ok(Input::Interrupt) => {
                    let again = self
                        .last_idle_interrupt
                        .is_some_and(|at| at.elapsed() < QUIT_WINDOW);
                    if again {
                        println!();
                        self.quit = true;
                    } else {
                        self.last_idle_interrupt = Some(Instant::now());
                        println!("\n{}", self.style.dim("Press Ctrl+C again to quit."));
                    }
                }
                Ok(Input::Eof) => {
                    println!();
                    self.quit = true;
                }
                Err(Some(wake)) => {
                    println!();
                    self.run_turn(Start::Wake(wake)).await;
                }
                Err(None) => {}
            }
        }
    }

    /// What was typed during the last turn: a command alone, or the lines
    /// up to the next command as one message (a paste stays whole).
    fn next_pending(&mut self) -> Option<String> {
        let first = self.pending.pop_front()?;
        if first.trim_start().starts_with('/') {
            return Some(first);
        }
        let mut lines = vec![first];
        while let Some(next) = self.pending.front() {
            if next.trim_start().starts_with('/') {
                break;
            }
            lines.extend(self.pending.pop_front());
        }
        Some(lines.join("\n"))
    }

    /// The whole message from its first line: the lines of a paste, and
    /// the lines a trailing `\` continues. `None` for an empty message.
    async fn read_message(&mut self, first: String) -> Option<String> {
        let mut lines = vec![first];
        loop {
            // A paste arrives as several lines at once.
            match tokio::time::timeout(PASTE_WINDOW, self.input.next()).await {
                Ok(Input::Line(line)) => {
                    lines.push(line);
                    continue;
                }
                Ok(Input::Interrupt) => {
                    println!();
                    return None;
                }
                Ok(Input::Eof) => {
                    self.quit = true;
                    break;
                }
                Err(_) => {}
            }
            let last = lines.last_mut().expect("one line at least");
            if last.ends_with('\\') {
                last.pop();
                prompt(&format!("{} ", self.style.dim(".")));
                match self.input.next().await {
                    Input::Line(line) => {
                        lines.push(line);
                        continue;
                    }
                    Input::Interrupt => {
                        println!();
                        return None;
                    }
                    Input::Eof => self.quit = true,
                }
            }
            break;
        }
        let message = lines.join("\n");
        (!message.trim().is_empty()).then_some(message)
    }

    /// One message from the person: a local command, a session command, or
    /// a prompt for the model.
    async fn submit(&mut self, message: String, images: Vec<uh_agent::types::ImageContent>) {
        let trimmed = message.trim();
        if trimmed.starts_with('/') && self.local_command(trimmed) {
            return;
        }
        if let Some(command) = self.engine.session.classify_session_command(trimmed) {
            self.session_command(&command).await;
            return;
        }
        self.run_turn(Start::Prompt(message, images)).await;
    }

    /// The commands the chat answers itself. `true` when handled.
    fn local_command(&mut self, text: &str) -> bool {
        match text.split_whitespace().next() {
            Some("/help" | "/?") => {
                println!("{HELP}");
                return true;
            }
            Some("/exit" | "/quit" | "/q") => {
                self.quit = true;
                return true;
            }
            _ => {}
        }
        let registry = SlashCommandRegistry::builtin_cached();
        let Some(resolved) = registry.parse(text) else {
            return false;
        };
        match resolved.name {
            "quit" => {
                self.quit = true;
                true
            }
            name => {
                let Some(command) = registry.get(name) else {
                    return false;
                };
                if command.execution != SlashCommandExecution::Client {
                    return false;
                }
                if name == "login" || name == "logout" {
                    println!(
                        "Run `{} {name}` in another terminal, then start this chat again.",
                        crate::config::APP_NAME
                    );
                } else {
                    println!("/{name} is not available in the terminal chat. /help lists what is.");
                }
                true
            }
        }
    }

    async fn session_command(
        &mut self,
        command: &uh_core::session_engine::slash_commands::SessionSlashCommand,
    ) {
        if command.name == "compact" {
            println!("{}", self.style.dim("Compacting the conversation…"));
        }
        let execution = crate::print_session_command::execute_prompt_session_command(
            &self.engine,
            &self.goal,
            &self.model,
            self.api_key.clone(),
            &self.autonomous_state,
            command,
        )
        .await;
        // The first row echoes the command; the rest are its results (a
        // failure's included).
        let mut shown = false;
        for row in execution.messages.iter().skip(1) {
            let text = row.content.text();
            if !text.trim().is_empty() {
                println!("{text}");
                shown = true;
            }
        }
        if execution.compaction.is_some() {
            println!("{}", self.style.dim("Conversation compacted."));
        } else if let Some(skipped) = execution.compaction_skipped {
            println!("{}", self.style.dim(skipped));
        }
        if let Some(error) = &execution.error {
            if !shown {
                println!("{}", self.style.red(error));
            }
            return;
        }
        // A `/goal` start or resume runs its first turn now.
        if let Some(continuation) = execution.continuation_message {
            let engine = Arc::clone(&self.engine);
            let goal = Arc::clone(&self.goal);
            let model = self.model.clone();
            let api_key = self.api_key.clone();
            let boundary = &mut self.boundary;
            let work = async move {
                goal.run_session_command_continuation(
                    &engine,
                    boundary,
                    &model,
                    api_key.clone(),
                    &continuation,
                )
                .await?;
                goal.drive_boundary(&engine, boundary, &model, api_key)
                    .await?;
                Ok::<(), String>(())
            };
            let result = drive(
                work,
                &self.engine,
                &self.cancel,
                &mut self.input,
                &mut self.pending,
                &mut self.quit,
                self.style,
            )
            .await;
            self.renderer.end_line();
            if let Err(error) = result {
                println!("{}", self.style.red(&format!("Error: {error}")));
            }
        }
    }

    /// One turn, from a prompt or a wake, through the same boundaries as
    /// `-p`.
    async fn run_turn(&mut self, start: Start) {
        self.cancel.store(false, Ordering::Relaxed);
        let engine = Arc::clone(&self.engine);
        let goal = Arc::clone(&self.goal);
        let autonomous = Arc::clone(&self.autonomous);
        let model = self.model.clone();
        let api_key = self.api_key.clone();
        let boundary = &mut self.boundary;
        let work = async move {
            boundary
                .run_pre_turn(&engine, &model, api_key.clone())
                .await?;
            match start {
                Start::Prompt(text, images) => engine
                    .session
                    .prompt_with_images(
                        &text,
                        images,
                        uh_core::session_engine::PromptOptions::default(),
                    )
                    .await
                    .map(|_| ()),
                Start::Wake(Wake::BashDone(messages)) => engine
                    .session
                    .prompt_injected_messages(&messages)
                    .await
                    .map(|_| ()),
                Start::Wake(Wake::Marker(text)) => engine
                    .session
                    .prompt(text, uh_core::session_engine::PromptOptions::default())
                    .await
                    .map(|_| ()),
                Start::Wake(Wake::GoalContinuation) => {
                    match engine.take_owed_goal_continuation().await {
                        Some(message) => engine
                            .session
                            .prompt_injected_message(&message)
                            .await
                            .map(|_| ()),
                        None => return Ok(()),
                    }
                }
            }
            .map_err(|error| format!("{error:#}"))?;
            engine.session.agent().wait_for_idle().await;
            crate::print_runtime::settle_turn(
                &engine,
                boundary,
                &goal,
                &autonomous,
                &model,
                api_key,
            )
            .await
        };
        let result = drive(
            work,
            &self.engine,
            &self.cancel,
            &mut self.input,
            &mut self.pending,
            &mut self.quit,
            self.style,
        )
        .await;
        self.renderer.end_line();
        if let Err(error) = result {
            println!("{}", self.style.red(&format!("Error: {error}")));
        }
        if self.engine.goal_state().await.status == uh_types::goal::GoalStatus::Blocked {
            println!(
                "{}",
                self.style
                    .yellow("The goal is blocked. /goal shows why; /goal resume tries again.")
            );
        }
    }
}

/// Run `work` while the person can still type: Ctrl+C stops the agent
/// (three presses quit), lines wait in `pending`, the end of input quits
/// once the turn is over.
async fn drive<F>(
    work: F,
    engine: &SessionEngine,
    cancel: &AtomicBool,
    input: &mut LineInput,
    pending: &mut VecDeque<String>,
    quit: &mut bool,
    style: Style,
) -> F::Output
where
    F: std::future::Future,
{
    tokio::pin!(work);
    let mut presses = 0;
    loop {
        tokio::select! {
            output = &mut work => return output,
            event = input.next() => match event {
                Input::Line(line) => {
                    if !line.trim().is_empty() {
                        pending.push_back(line);
                    }
                }
                Input::Eof => *quit = true,
                Input::Interrupt => {
                    presses += 1;
                    if presses >= FORCE_QUIT_PRESSES {
                        println!("\n{}", style.dim("Quitting."));
                        engine.dispose_kernel().await;
                        std::process::exit(130);
                    }
                    cancel.store(true, Ordering::Relaxed);
                    let agent = engine.session.agent();
                    agent.abort();
                    agent.clear_all_queues();
                    println!("\n{}", style.yellow("Stopped."));
                }
            }
        }
    }
}

/// Prints the agent's events as they come.
struct Renderer {
    style: Style,
    state: Mutex<RenderState>,
}

#[derive(Default)]
struct RenderState {
    /// The cursor is not at the start of a line.
    mid_line: bool,
    /// The current assistant message already said it was thinking.
    thinking_shown: bool,
    /// Text of the current assistant message has started.
    text_started: bool,
}

impl Renderer {
    fn new(style: Style) -> Self {
        Renderer {
            style,
            state: Mutex::new(RenderState::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RenderState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Move to a fresh line if the cursor is mid-line.
    fn end_line(&self) {
        let mut state = self.state();
        if state.mid_line {
            println!();
            state.mid_line = false;
        }
    }

    fn on_event(&self, event: &AgentEvent) {
        let style = self.style;
        match event {
            AgentEvent::MessageStart { message } => {
                if is_assistant(message) {
                    let mut state = self.state();
                    state.thinking_shown = false;
                    state.text_started = false;
                }
            }
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match assistant_message_event.as_ref() {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    let mut state = self.state();
                    if !state.text_started {
                        state.text_started = true;
                        if state.mid_line {
                            println!();
                        }
                    }
                    prompt(delta);
                    state.mid_line = !delta.ends_with('\n');
                }
                AssistantMessageEvent::ThinkingStart { .. } => {
                    let mut state = self.state();
                    if !state.thinking_shown {
                        state.thinking_shown = true;
                        if state.mid_line {
                            println!();
                        }
                        println!("{}", style.dim("thinking…"));
                        state.mid_line = false;
                    }
                }
                _ => {}
            },
            AgentEvent::MessageEnd { message } => {
                let AgentMessage::Standard(Message::Assistant(assistant)) = message else {
                    return;
                };
                let mut state = self.state();
                if state.mid_line {
                    println!();
                    state.mid_line = false;
                }
                if assistant.stop_reason == StopReason::Error {
                    let text = assistant
                        .error_message
                        .as_deref()
                        .filter(|text| !text.is_empty())
                        .unwrap_or("the request failed");
                    println!("{}", style.red(&format!("Error: {text}")));
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                let mut state = self.state();
                if state.mid_line {
                    println!();
                    state.mid_line = false;
                }
                println!("{} {}", style.cyan("●"), style.bold(tool_name));
                let code = args
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    .map_or_else(|| args.to_string(), str::to_string);
                for line in clip_lines(&code, CODE_LINES) {
                    println!("  {} {}", style.dim("│"), line);
                }
            }
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => {
                let _state = self.state();
                let text: String = result
                    .content
                    .iter()
                    .map(|content| match content {
                        ToolResultContent::Text(text) => text.text.as_str(),
                        ToolResultContent::Image(_) => "[image]",
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let lines = clip_lines(text.trim_end(), OUTPUT_LINES);
                if lines.is_empty() {
                    println!("  {}", style.dim("└ (no output)"));
                }
                for (index, line) in lines.iter().enumerate() {
                    let lead = if index == 0 { "└" } else { " " };
                    let line = if *is_error {
                        style.red(line)
                    } else {
                        style.dim(line)
                    };
                    println!("  {} {}", style.dim(lead), line);
                }
            }
            _ => {}
        }
    }
}

fn is_assistant(message: &AgentMessage) -> bool {
    matches!(message, AgentMessage::Standard(Message::Assistant(_)))
}

/// The first `max` lines of `text`, each cut at [`LINE_CHARS`], plus a
/// line counting the rest.
fn clip_lines(text: &str, max: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let all: Vec<&str> = text.lines().collect();
    let mut shown: Vec<String> = all
        .iter()
        .take(max)
        .map(|line| {
            if line.chars().count() > LINE_CHARS {
                let cut: String = line.chars().take(LINE_CHARS).collect();
                format!("{cut}…")
            } else {
                (*line).to_string()
            }
        })
        .collect();
    if all.len() > max {
        let rest = all.len() - max;
        shown.push(format!(
            "… {rest} more line{}",
            if rest == 1 { "" } else { "s" }
        ));
    }
    shown
}

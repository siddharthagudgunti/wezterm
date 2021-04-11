use crate::connui::ConnectionUI;
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState};
use crate::localpane::LocalPane;
use crate::pane::{alloc_pane_id, Pane, PaneId};
use crate::tab::{SplitDirection, Tab, TabId};
use crate::window::WindowId;
use crate::Mux;
use anyhow::{anyhow, bail, Context, Error};
use async_trait::async_trait;
use filedescriptor::{socketpair, FileDescriptor};
use portable_pty::cmdbuilder::CommandBuilder;
use portable_pty::{ExitStatus, MasterPty, PtySize};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{BufWriter, Read, Write};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::Duration;
use termwiz::cell::unicode_column_width;
use termwiz::input::{InputEvent, InputParser};
use termwiz::lineedit::*;
use termwiz::render::terminfo::TerminfoRenderer;
use termwiz::surface::Change;
use termwiz::terminal::{ScreenSize, Terminal, TerminalWaker};
use wezterm_ssh::{ConfigMap, Session, SessionEvent, SshChildProcess, SshPty};

#[derive(Default)]
struct PasswordPromptHost {
    history: BasicHistory,
    echo: bool,
}
impl LineEditorHost for PasswordPromptHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }

    fn highlight_line(&self, line: &str, cursor_position: usize) -> (Vec<OutputElement>, usize) {
        if self.echo {
            (vec![OutputElement::Text(line.to_string())], cursor_position)
        } else {
            // Rewrite the input so that we can obscure the password
            // characters when output to the terminal widget
            let placeholder = "🔑";
            let grapheme_count = unicode_column_width(line);
            let mut output = vec![];
            for _ in 0..grapheme_count {
                output.push(OutputElement::Text(placeholder.to_string()));
            }
            (output, unicode_column_width(placeholder) * cursor_position)
        }
    }
}

pub fn ssh_connect_with_ui(
    remote_address: &str,
    username: Option<&str>,
    ui: &mut ConnectionUI,
) -> anyhow::Result<Session> {
    let cloned_ui = ui.clone();
    cloned_ui.run_and_log_error(move || {
        let mut ssh_config = wezterm_ssh::Config::new();
        ssh_config.add_default_config_files();

        let (remote_host_name, port) = {
            let parts: Vec<&str> = remote_address.split(':').collect();

            if parts.len() == 2 {
                (parts[0], Some(parts[1].parse::<u16>()?))
            } else {
                (remote_address, None)
            }
        };

        let mut ssh_config = ssh_config.for_host(&remote_host_name);
        if let Some(username) = username {
            ssh_config.insert("user".to_string(), username.to_string());
        }
        if let Some(port) = port {
            ssh_config.insert("port".to_string(), port.to_string());
        }

        ui.output_str(&format!("Connecting to {} using SSH\n", remote_address));
        let (session, events) = Session::connect(ssh_config.clone())?;

        while let Ok(event) = smol::block_on(events.recv()) {
            match event {
                SessionEvent::Banner(banner) => {
                    if let Some(banner) = banner {
                        ui.output_str(&format!("{}\n", banner));
                    }
                }
                SessionEvent::HostVerify(verify) => {
                    ui.output_str(&format!("{}\n", verify.message));
                    let ok = if let Ok(line) = ui.input("Enter [y/n]> ") {
                        match line.as_ref() {
                            "y" | "Y" | "yes" | "YES" => true,
                            "n" | "N" | "no" | "NO" | _ => false,
                        }
                    } else {
                        false
                    };
                    smol::block_on(verify.answer(ok)).context("send verify response")?;
                }
                SessionEvent::Authenticate(auth) => {
                    if !auth.username.is_empty() {
                        ui.output_str(&format!("Authentication for {}\n", auth.username));
                    }
                    if !auth.instructions.is_empty() {
                        ui.output_str(&format!("{}\n", auth.instructions));
                    }
                    let mut answers = vec![];
                    for prompt in &auth.prompts {
                        let mut prompt_lines = prompt.prompt.split('\n').collect::<Vec<_>>();
                        let editor_prompt = prompt_lines.pop().unwrap();
                        for line in &prompt_lines {
                            ui.output_str(&format!("{}\n", line));
                        }
                        let res = if prompt.echo {
                            ui.input(editor_prompt)
                        } else {
                            ui.password(editor_prompt)
                        };
                        if let Ok(line) = res {
                            answers.push(line);
                        } else {
                            anyhow::bail!("Authentication was cancelled");
                        }
                    }
                    smol::block_on(auth.answer(answers))?;
                }
                SessionEvent::Error(err) => {
                    anyhow::bail!("Error: {}", err);
                }
                SessionEvent::Authenticated => return Ok(session),
            }
        }
        bail!("unable to authenticate session");
    })
}

/// Represents a connection to remote host via ssh.
/// The domain is created with the ssh config prior to making the
/// connection.  The connection is established by the first spawn()
/// call.
/// In order to show the authentication dialog inline in that spawned
/// pane, we play some tricks with wrapped versions of the pty, child
/// and the reader and writer instances so that we can inject the
/// interactive setup.  The bulk of that is driven by `connect_ssh_session`.
pub struct RemoteSshDomain {
    session: Session,
    id: DomainId,
    name: String,
    events: RefCell<Option<smol::channel::Receiver<SessionEvent>>>,
}

impl RemoteSshDomain {
    pub fn with_ssh_config(name: &str, ssh_config: ConfigMap) -> anyhow::Result<Self> {
        let id = alloc_domain_id();
        let (session, events) = Session::connect(ssh_config.clone())?;
        Ok(Self {
            id,
            name: format!("SSH to {}", name),
            session,
            events: RefCell::new(Some(events)),
        })
    }
}

/// Carry out the authentication process and create the initial pty.
fn connect_ssh_session(
    session: Session,
    events: smol::channel::Receiver<SessionEvent>,
    mut stdin_read: BoxedReader,
    stdin_tx: Sender<BoxedWriter>,
    stdout_write: &mut BufWriter<FileDescriptor>,
    stdout_tx: Sender<BoxedReader>,
    child_tx: Sender<SshChildProcess>,
    pty_tx: Sender<SshPty>,
    size: PtySize,
    command_line: Option<String>,
    env: HashMap<String, String>,
) -> anyhow::Result<()> {
    struct StdoutShim<'a> {
        size: PtySize,
        stdout: &'a mut BufWriter<FileDescriptor>,
    }

    impl<'a> Write for StdoutShim<'a> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.stdout.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.stdout.flush()
        }
    }

    impl<'a> termwiz::render::RenderTty for StdoutShim<'a> {
        fn get_size_in_cells(&mut self) -> termwiz::Result<(usize, usize)> {
            Ok((self.size.cols as _, self.size.rows as _))
        }
    }

    /// a termwiz Terminal for use with the line editor
    struct TerminalShim<'a> {
        stdout: &'a mut StdoutShim<'a>,
        stdin: &'a mut BoxedReader,
        size: PtySize,
        renderer: TerminfoRenderer,
        parser: InputParser,
        input_queue: VecDeque<InputEvent>,
    }

    impl<'a> termwiz::terminal::Terminal for TerminalShim<'a> {
        fn set_raw_mode(&mut self) -> termwiz::Result<()> {
            use termwiz::escape::csi::{DecPrivateMode, DecPrivateModeCode, Mode, CSI};

            macro_rules! decset {
                ($variant:ident) => {
                    write!(
                        self.stdout,
                        "{}",
                        CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                            DecPrivateModeCode::$variant
                        )))
                    )?;
                };
            }

            decset!(BracketedPaste);
            self.flush()?;

            Ok(())
        }

        fn flush(&mut self) -> termwiz::Result<()> {
            self.stdout.flush()?;
            Ok(())
        }

        fn set_cooked_mode(&mut self) -> termwiz::Result<()> {
            Ok(())
        }

        fn enter_alternate_screen(&mut self) -> termwiz::Result<()> {
            termwiz::bail!("TerminalShim has no alt screen");
        }

        fn exit_alternate_screen(&mut self) -> termwiz::Result<()> {
            termwiz::bail!("TerminalShim has no alt screen");
        }

        fn get_screen_size(&mut self) -> termwiz::Result<ScreenSize> {
            Ok(ScreenSize {
                cols: self.size.cols as _,
                rows: self.size.rows as _,
                xpixel: self.size.pixel_width as _,
                ypixel: self.size.pixel_height as _,
            })
        }

        fn set_screen_size(&mut self, _size: ScreenSize) -> termwiz::Result<()> {
            termwiz::bail!("TerminalShim cannot set screen size");
        }

        fn render(&mut self, changes: &[Change]) -> termwiz::Result<()> {
            self.renderer.render_to(changes, self.stdout)?;
            Ok(())
        }

        fn poll_input(&mut self, _wait: Option<Duration>) -> termwiz::Result<Option<InputEvent>> {
            if let Some(event) = self.input_queue.pop_front() {
                return Ok(Some(event));
            }

            let mut buf = [0u8; 64];
            let n = self.stdin.read(&mut buf)?;
            let input_queue = &mut self.input_queue;
            self.parser
                .parse(&buf[0..n], |evt| input_queue.push_back(evt), n == buf.len());
            Ok(self.input_queue.pop_front())
        }

        fn waker(&self) -> TerminalWaker {
            // TODO: TerminalWaker assumes that we're a SystemTerminal but that
            // isn't the case here.
            panic!("TerminalShim::waker called!?");
        }
    }

    let renderer = crate::termwiztermtab::new_wezterm_terminfo_renderer();
    let mut shim = TerminalShim {
        stdout: &mut StdoutShim {
            stdout: stdout_write,
            size,
        },
        size,
        renderer,
        stdin: &mut stdin_read,
        parser: InputParser::new(),
        input_queue: VecDeque::new(),
    };

    impl<'a> TerminalShim<'a> {
        fn output_line(&mut self, s: &str) -> termwiz::Result<()> {
            let mut s = s.replace("\n", "\r\n");
            s.push_str("\r\n");
            self.render(&[Change::Text(s)])
        }
    }

    // Process authentication related events
    while let Ok(event) = smol::block_on(events.recv()) {
        match event {
            SessionEvent::Banner(banner) => {
                if let Some(banner) = banner {
                    shim.output_line(&banner)?;
                }
            }
            SessionEvent::HostVerify(verify) => {
                shim.output_line(&verify.message)?;
                let mut editor = LineEditor::new(&mut shim);
                let mut host = PasswordPromptHost::default();
                host.echo = true;
                editor.set_prompt("Enter [y/n]> ");
                let ok = if let Some(line) = editor.read_line(&mut host)? {
                    match line.as_ref() {
                        "y" | "Y" | "yes" | "YES" => true,
                        "n" | "N" | "no" | "NO" | _ => false,
                    }
                } else {
                    false
                };
                smol::block_on(verify.answer(ok)).context("send verify response")?;
            }
            SessionEvent::Authenticate(auth) => {
                if !auth.username.is_empty() {
                    shim.output_line(&format!("Authentication for {}", auth.username))?;
                }
                if !auth.instructions.is_empty() {
                    shim.output_line(&auth.instructions)?;
                }
                let mut answers = vec![];
                for prompt in &auth.prompts {
                    let mut prompt_lines = prompt.prompt.split('\n').collect::<Vec<_>>();
                    let editor_prompt = prompt_lines.pop().unwrap();
                    for line in &prompt_lines {
                        shim.output_line(line)?;
                    }
                    let mut editor = LineEditor::new(&mut shim);
                    let mut host = PasswordPromptHost::default();
                    editor.set_prompt(editor_prompt);
                    host.echo = prompt.echo;
                    if let Some(line) = editor.read_line(&mut host)? {
                        answers.push(line);
                    } else {
                        anyhow::bail!("Authentication was cancelled");
                    }
                }
                smol::block_on(auth.answer(answers))?;
            }
            SessionEvent::Error(err) => {
                shim.output_line(&format!("Error: {}", err))?;
            }
            SessionEvent::Authenticated => {
                // Our session has been authenticated: we can now
                // set up the real pty for the pane
                match smol::block_on(session.request_pty(
                    &config::configuration().term,
                    size,
                    command_line.as_ref().map(|s| s.as_str()),
                    Some(env),
                )) {
                    Err(err) => {
                        shim.output_line(&format!("Failed to spawn command: {:#}", err))?;
                        break;
                    }
                    Ok((pty, child)) => {
                        drop(shim);

                        // Obtain the real stdin/stdout for the pty
                        let reader = pty.try_clone_reader()?;
                        let writer = pty.try_clone_writer()?;

                        // And send them to the wrapped reader/writer
                        stdin_tx
                            .send(Box::new(writer))
                            .map_err(|e| anyhow!("{:#}", e))?;
                        stdout_tx
                            .send(Box::new(reader))
                            .map_err(|e| anyhow!("{:#}", e))?;

                        // Likewise, send the real pty and child to
                        // the wrappers
                        pty_tx.send(pty)?;
                        child_tx.send(child)?;

                        // Now when we return, our stdin_read and
                        // stdout_write will close and that will cause
                        // the PtyReader and PtyWriter to recv the
                        // the new reader/writer above and continue.
                        //
                        // The pty and child will be picked up when
                        // they are next polled or resized.

                        return Ok(());
                    }
                }
            }
        }
    }

    Ok(())
}

#[async_trait(?Send)]
impl Domain for RemoteSshDomain {
    async fn spawn(
        &self,
        size: PtySize,
        command: Option<CommandBuilder>,
        _command_dir: Option<String>,
        window: WindowId,
    ) -> Result<Rc<Tab>, Error> {
        let pane_id = alloc_pane_id();

        let cmd = match command {
            Some(c) => c,
            None => CommandBuilder::new_default_prog(),
        };

        let command_line = if cmd.is_default_prog() {
            None
        } else {
            Some(cmd.as_unix_command_line()?)
        };
        let mut env: HashMap<String, String> = cmd
            .iter_env_as_str()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env.insert("WEZTERM_PANE".to_string(), pane_id.to_string());

        let pty: Box<dyn portable_pty::MasterPty>;
        let child: Box<dyn portable_pty::Child>;
        let writer: BoxedWriter;

        if let Some(events) = self.events.borrow_mut().take() {
            // We get to establish the session!
            //
            // Since we want spawn to return the Pane in which
            // we'll carry out interactive auth, we generate
            // some shim/wrapper versions of the pty, child
            // and reader/writer.

            let (stdout_read, stdout_write) = socketpair()?;
            let (reader_tx, reader_rx) = channel();
            let (stdin_read, stdin_write) = socketpair()?;
            let (writer_tx, writer_rx) = channel();

            let pty_reader = PtyReader {
                reader: Box::new(stdout_read),
                rx: reader_rx,
            };

            let pty_writer = PtyWriter {
                writer: Box::new(stdin_write),
                rx: writer_rx,
            };
            writer = Box::new(pty_writer);

            let (child_tx, child_rx) = channel();

            child = Box::new(WrappedSshChild {
                child: None,
                rx: child_rx,
                exited: None,
            });

            let (pty_tx, pty_rx) = channel();

            pty = Box::new(WrappedSshPty {
                inner: RefCell::new(WrappedSshPtyInner::Connecting {
                    size,
                    reader: Some(pty_reader),
                    connected: pty_rx,
                }),
            });

            // And with those created, we can now spawn a new thread
            // to perform the blocking (from its perspective) terminal
            // UI to carry out any authentication.
            let session = self.session.clone();
            let stdin_read: BoxedReader = Box::new(stdin_read);
            let mut stdout_write = BufWriter::new(stdout_write);
            std::thread::spawn(move || {
                if let Err(err) = connect_ssh_session(
                    session,
                    events,
                    stdin_read,
                    writer_tx,
                    &mut stdout_write,
                    reader_tx,
                    child_tx,
                    pty_tx,
                    size,
                    command_line,
                    env,
                ) {
                    let _ = write!(stdout_write, "{:#}", err);
                    log::error!("Failed to connect ssh: {:#}", err);
                }
                let _ = stdout_write.flush();
            });
        } else {
            let (concrete_pty, concrete_child) = self
                .session
                .request_pty(
                    &config::configuration().term,
                    size,
                    command_line.as_ref().map(|s| s.as_str()),
                    Some(env),
                )
                .await?;

            pty = Box::new(concrete_pty);
            child = Box::new(concrete_child);
            writer = Box::new(pty.try_clone_writer()?);
        };

        // Wrap up the pty etc. in a LocalPane.  That allows for
        // eg: tmux integration to be tunnelled via the remote
        // session without duplicating a lot of logic over here.

        let terminal = wezterm_term::Terminal::new(
            crate::pty_size_to_terminal_size(size),
            std::sync::Arc::new(config::TermConfig {}),
            "WezTerm",
            config::wezterm_version(),
            writer,
        );

        let mux = Mux::get().unwrap();
        let pane: Rc<dyn Pane> = Rc::new(LocalPane::new(pane_id, terminal, child, pty, self.id));
        let tab = Rc::new(Tab::new(&size));
        tab.assign_pane(&pane);

        mux.add_tab_and_active_pane(&tab)?;
        mux.add_tab_to_window(&tab, window)?;

        Ok(tab)
    }

    async fn split_pane(
        &self,
        command: Option<CommandBuilder>,
        _command_dir: Option<String>,
        tab: TabId,
        pane_id: PaneId,
        direction: SplitDirection,
    ) -> anyhow::Result<Rc<dyn Pane>> {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_tab(tab) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", tab),
        };

        let pane_index = match tab
            .iter_panes()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
        {
            Some(p) => p.index,
            None => anyhow::bail!("invalid pane id {}", pane_id),
        };

        let split_size = match tab.compute_split_size(pane_index, direction) {
            Some(s) => s,
            None => anyhow::bail!("invalid pane index {}", pane_index),
        };

        let config = config::configuration();
        let cmd = match command {
            Some(mut cmd) => {
                config.apply_cmd_defaults(&mut cmd);
                cmd
            }
            None => config.build_prog(None)?,
        };
        let pane_id = alloc_pane_id();

        let command_line = if cmd.is_default_prog() {
            None
        } else {
            Some(cmd.as_unix_command_line()?)
        };
        let mut env: HashMap<String, String> = cmd
            .iter_env_as_str()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env.insert("WEZTERM_PANE".to_string(), pane_id.to_string());

        let (pty, child) = self
            .session
            .request_pty(
                &config::configuration().term,
                split_size.size(),
                command_line.as_ref().map(|s| s.as_str()),
                Some(env),
            )
            .await?;

        let writer = pty.try_clone_writer()?;

        let terminal = wezterm_term::Terminal::new(
            crate::pty_size_to_terminal_size(split_size.second),
            std::sync::Arc::new(config::TermConfig {}),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer),
        );

        let pane: Rc<dyn Pane> = Rc::new(LocalPane::new(
            pane_id,
            terminal,
            Box::new(child),
            Box::new(pty),
            self.id,
        ));

        tab.split_and_insert(pane_index, direction, Rc::clone(&pane))?;

        mux.add_pane(&pane)?;

        Ok(pane)
    }

    fn domain_id(&self) -> DomainId {
        self.id
    }

    fn domain_name(&self) -> &str {
        &self.name
    }

    async fn attach(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn detach(&self) -> anyhow::Result<()> {
        bail!("detach not implemented");
    }

    fn state(&self) -> DomainState {
        DomainState::Attached
    }
}

#[derive(Debug)]
struct WrappedSshChild {
    child: Option<SshChildProcess>,
    rx: Receiver<SshChildProcess>,
    exited: Option<ExitStatus>,
}

impl WrappedSshChild {
    fn check_connected(&mut self) {
        if self.child.is_none() {
            match self.rx.try_recv() {
                Ok(c) => {
                    self.child.replace(c);
                }
                Err(TryRecvError::Empty) => {}
                Err(err) => {
                    log::error!("WrappedSshChild err: {:#?}", err);
                    self.exited.replace(ExitStatus::with_exit_code(1));
                }
            }
        }
    }
}

impl portable_pty::Child for WrappedSshChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(Some(status.clone()));
        }

        self.check_connected();

        if let Some(child) = self.child.as_mut() {
            child.try_wait()
        } else if let Some(status) = self.exited.as_ref() {
            Ok(Some(status.clone()))
        } else {
            Ok(None)
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        // There is no way to send a signal via libssh2.
        // Just pretend that we did. :-/
        Ok(())
    }

    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(status.clone());
        }

        self.check_connected();

        if let Some(child) = self.child.as_mut() {
            child.wait()
        } else {
            match self.rx.recv() {
                Ok(c) => {
                    self.child.replace(c);
                    self.child.as_mut().unwrap().wait()
                }
                Err(_) => {
                    self.exited.replace(ExitStatus::with_exit_code(1));
                    return Ok(self.exited.as_ref().cloned().unwrap());
                }
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        None
    }
}

type BoxedReader = Box<(dyn Read + Send + 'static)>;
type BoxedWriter = Box<(dyn Write + Send + 'static)>;

struct WrappedSshPty {
    inner: RefCell<WrappedSshPtyInner>,
}

enum WrappedSshPtyInner {
    Connecting {
        reader: Option<PtyReader>,
        connected: Receiver<SshPty>,
        size: PtySize,
    },
    Connected {
        reader: Option<PtyReader>,
        pty: SshPty,
    },
}

struct PtyReader {
    reader: BoxedReader,
    rx: Receiver<BoxedReader>,
}

struct PtyWriter {
    writer: BoxedWriter,
    rx: Receiver<BoxedWriter>,
}

impl std::io::Write for WrappedSshPty {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        log::error!("boo");
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "you are expected to write via try_clone_writer",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        log::error!("boo");
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "you are expected to write via try_clone_writer",
        ))
    }
}

impl WrappedSshPtyInner {
    fn check_connected(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Connecting {
                reader,
                connected,
                size,
                ..
            } => {
                if let Ok(pty) = connected.try_recv() {
                    let res = pty.resize(*size);
                    *self = Self::Connected {
                        pty,
                        reader: reader.take(),
                    };
                    res
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}

impl portable_pty::MasterPty for WrappedSshPty {
    fn resize(&self, new_size: PtySize) -> anyhow::Result<()> {
        let mut inner = self.inner.borrow_mut();
        match &mut *inner {
            WrappedSshPtyInner::Connecting { ref mut size, .. } => {
                *size = new_size;
                inner.check_connected()
            }
            WrappedSshPtyInner::Connected { pty, .. } => pty.resize(new_size),
        }
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        let mut inner = self.inner.borrow_mut();
        match &*inner {
            WrappedSshPtyInner::Connecting { size, .. } => {
                let size = *size;
                inner.check_connected()?;
                Ok(size)
            }
            WrappedSshPtyInner::Connected { pty, .. } => pty.get_size(),
        }
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<(dyn Read + Send + 'static)>> {
        let mut inner = self.inner.borrow_mut();
        inner.check_connected()?;
        match &mut *inner {
            WrappedSshPtyInner::Connected { ref mut reader, .. }
            | WrappedSshPtyInner::Connecting { ref mut reader, .. } => match reader.take() {
                Some(r) => Ok(Box::new(r)),
                None => anyhow::bail!("reader already taken"),
            },
        }
    }

    fn try_clone_writer(&self) -> anyhow::Result<Box<(dyn Write + Send + 'static)>> {
        anyhow::bail!("writer must be created during bootstrap");
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<i32> {
        let mut inner = self.inner.borrow_mut();
        let _ = inner.check_connected();
        None
    }
}

impl std::io::Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.writer.write(buf) {
            Ok(len) if len > 0 => Ok(len),
            res => match self.rx.recv() {
                Ok(writer) => {
                    self.writer = writer;
                    self.writer.write(buf)
                }
                _ => res,
            },
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.writer.flush() {
            Ok(_) => Ok(()),
            res => match self.rx.recv() {
                Ok(writer) => {
                    self.writer = writer;
                    self.writer.flush()
                }
                _ => res,
            },
        }
    }
}

impl std::io::Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.reader.read(buf) {
            Ok(len) if len > 0 => Ok(len),
            res => match self.rx.recv() {
                Ok(reader) => {
                    self.reader = reader;
                    self.reader.read(buf)
                }
                _ => res,
            },
        }
    }
}
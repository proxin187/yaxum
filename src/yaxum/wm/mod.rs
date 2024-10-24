use crate::config::{Config, Padding};
use crate::log::{self, Severity};
use crate::server::Server;
use crate::startup;

use yaxi::display::request::GetGeometryResponse;
use yaxi::display::{self, Display, Atom, TryClone};
use yaxi::proto::{Event, EventMask, EventKind, KeyMask, Button, Cursor, RevertTo, WindowClass, PointerMode, KeyboardMode};
use yaxi::window::{Window, WindowKind, WindowArguments, ValuesBuilder, PropFormat, PropMode};

use std::os::unix::net::UnixStream;

use proto::Request;


pub struct Client {
    window: Window<UnixStream>,
    float: bool,
}

impl Client {
    pub fn new(window: Window<UnixStream>, float: bool) -> Client {
        Client {
            window,
            float,
        }
    }
}

pub struct Workspaces {
    workspaces: Vec<Vec<Client>>,
    current: usize,
}

impl Workspaces {
    pub fn new() -> Workspaces {
        Workspaces {
            workspaces: Vec::new(),
            current: 0,
        }
    }

    pub fn resize(&mut self, size: usize) {
        if size >= self.len() {
            self.workspaces.resize_with(size, Vec::new);
        } else if size > 0 {
            let excess = self.workspaces.drain(size..self.len()).flatten().collect::<Vec<Client>>();

            self.workspaces[size - 1].extend(excess);

            self.workspaces.truncate(self.len() - size);
        }
    }

    pub fn len(&self) -> usize {
        self.workspaces.len()
    }

    pub fn insert(&mut self, client: Client) {
        self.workspaces[self.current].push(client);
    }

    pub fn remove(&mut self, index: usize) {
        self.workspaces[self.current].remove(index);
    }

    pub fn find(&self, wid: u32) -> Option<usize> {
        self.workspaces[self.current].iter().position(|client| client.window.id() == wid)
    }

    pub fn is_float(&self, wid: u32) -> bool {
        match self.find(wid) {
            Some(index) => self.workspaces[self.current][index].float,
            None => false,
        }
    }

    pub fn change_focus<F>(&mut self, wid: u32, f: F) -> Result<(), Box<dyn std::error::Error>> where F: Fn(usize) -> usize {
        if let Some(client) = self.find(wid).and_then(|index| self.workspaces[self.current].get_mut(f(index))) {
            client.window.set_input_focus(RevertTo::Parent)?;
        }

        Ok(())
    }

    pub fn map_clients<F>(&mut self, f: F) -> Result<(), Box<dyn std::error::Error>> where F: Fn(&mut Client) -> Result<(), Box<dyn std::error::Error>> {
        for workspace in self.workspaces.iter_mut() {
            for client in workspace {
                f(client)?;
            }
        }

        Ok(())
    }

    pub fn tile(&mut self, mut area: Area, gaps: u16) -> Result<(), Box<dyn std::error::Error>> {
        for (w_idx, workspace) in self.workspaces.iter_mut().enumerate() {
            if w_idx == self.current {
                let floating = workspace.iter().map(|client| client.float).collect::<Vec<bool>>();

                for (index, client) in workspace.iter_mut().enumerate() {
                    if !client.float {
                        let tiled_clients_left = floating[index + 1..].iter().filter(|float| !**float).count();

                        let win = (tiled_clients_left > 0).then(|| area.split()).unwrap_or(area);

                        client.window.mov_resize(win.x + gaps, win.y + gaps, win.width - (gaps * 2), win.height - (gaps * 2))?;
                    }

                    client.window.map(WindowKind::Window)?;
                }
            } else {
                for client in workspace {
                    client.window.unmap(WindowKind::Window)?;
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Area {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl Area {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Area {
        Area {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        (x >= self.x && self.y >= self.y) && (self.x + self.width > x && self.y + self.height > y)
    }

    pub fn pad(&self, padding: Padding) -> Area {
        Area {
            x: self.x + padding.left,
            y: self.y + padding.top,
            width: self.width - padding.right - padding.left,
            height: self.height - padding.bottom - padding.top,
        }
    }

    pub fn split(&mut self) -> Area {
        let area = self.clone();

        if self.width > self.height {
            *self = Area::new(area.x + (area.width / 2), area.y, area.width / 2, area.height);

            Area::new(area.x, area.y, area.width / 2, area.height)
        } else {
            *self = Area::new(area.x, area.y + (area.height / 2), area.width, area.height / 2);

            Area::new(area.x, area.y, area.width, area.height / 2)
        }
    }
}

pub struct Monitor {
    area: Area,
    workspace: Workspaces,
}

pub struct Monitors {
    monitors: Vec<Monitor>,
    root: Window<UnixStream>,
}

impl Monitors {
    pub fn new(root: Window<UnixStream>) -> Monitors {
        Monitors {
            monitors: Vec::new(),
            root,
        }
    }

    pub fn append(&mut self, monitor: Monitor) {
        self.monitors.push(monitor);
    }

    pub fn is_tiled(&mut self, wid: u32) -> bool {
        self.monitors.iter()
            .map(|monitor| monitor.workspace.is_float(wid))
            .any(|float| !float)
    }

    pub fn focused<F>(&mut self, f: F) -> Result<(), Box<dyn std::error::Error>> where F: Fn(&mut Monitor) -> Result<(), Box<dyn std::error::Error>> {
        let pointer = self.root.query_pointer()?;

        for monitor in &mut self.monitors {
            if monitor.area.contains(pointer.root_x, pointer.root_y) {
                f(monitor)?;
            }
        }

        Ok(())
    }

    pub fn all<F>(&mut self, f: F) -> Result<(), Box<dyn std::error::Error>> where F: Fn(&mut Monitor) -> Result<(), Box<dyn std::error::Error>> {
        for monitor in &mut self.monitors {
            f(monitor)?;
        }

        Ok(())
    }
}

pub struct Grab {
    button: Button,
    window: Window<UnixStream>,
    geometry: GetGeometryResponse,
    x: u16,
    y: u16,
}

impl Grab {
    pub fn new(button: Button, window: Window<UnixStream>, geometry: GetGeometryResponse, x: u16, y: u16) -> Grab {
        Grab {
            button,
            window,
            geometry,
            x,
            y,
        }
    }
}

pub struct WindowManager {
    display: Display<UnixStream>,
    root: Window<UnixStream>,
    monitors: Monitors,
    server: Server,
    config: Config,
    grab: Option<Grab>,
    should_close: bool,
}

impl WindowManager {
    pub fn new() -> Result<WindowManager, Box<dyn std::error::Error>> {
        let display = display::open_unix(1)?;
        let root = display.default_root_window()?;

        Ok(WindowManager {
            display,
            root: *root.try_clone()?,
            monitors: Monitors::new(root),
            server: Server::new(),
            config: Config::default(),
            grab: None,
            should_close: false,
        })
    }

    fn setup(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.root.select_input(&[
            EventMask::SubstructureNotify,
            EventMask::SubstructureRedirect,
            EventMask::EnterWindow,
            EventMask::FocusChange,
        ])?;

        for button in [Button::Button1, Button::Button3] {
            self.root.grab_button(
                button,
                vec![KeyMask::Mod4],
                vec![EventMask::ButtonPress, EventMask::ButtonRelease, EventMask::ButtonMotion],
                Cursor::Nop,
                PointerMode::Asynchronous,
                KeyboardMode::Asynchronous,
                true,
                0,
            )?;
        }

        self.server.listen()?;

        self.set_supporting_ewmh()?;

        self.load_monitors()?;

        startup::startup()?;

        Ok(())
    }

    fn load_monitors(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut xinerama = self.display.query_xinerama()?;

        for screen in xinerama.query_screens()? {
            self.monitors.append(Monitor {
                area: Area::new(screen.x, screen.y, screen.width, screen.height),
                workspace: Workspaces::new(),
            });
        }

        Ok(())
    }

    fn set_supporting_ewmh(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let net_wm_check = self.display.intern_atom("_NET_SUPPORTING_WM_CHECK", false)?;
        let net_wm_name = self.display.intern_atom("_NET_WM_NAME", false)?;
        let utf8_string = self.display.intern_atom("UTF8_STRING", false)?;

        let mut window = self.root.create_window(WindowArguments {
            depth: self.root.depth(),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            class: WindowClass::InputOutput,
            border_width: 0,
            visual: self.root.visual(),
            values: ValuesBuilder::new(vec![]),
        })?;

        window.change_property(net_wm_check, Atom::WINDOW, PropFormat::Format32, PropMode::Replace, &window.id().to_le_bytes())?;

        window.change_property(net_wm_name, utf8_string, PropFormat::Format8, PropMode::Replace, b"yaxwm")?;

        self.root.change_property(net_wm_check, Atom::WINDOW, PropFormat::Format32, PropMode::Replace, &window.id().to_le_bytes())?;

        Ok(())
    }

    fn tile(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.monitors.all(|monitor| {
            monitor.workspace.tile(monitor.area.pad(self.config.padding), self.config.windows.gaps)
        })?;

        Ok(())
    }

    fn focused_client<F>(&mut self, f: F) -> Result<(), Box<dyn std::error::Error>> where F: Fn(&mut Client) -> Result<(), Box<dyn std::error::Error>> {
        let focus = self.display.get_input_focus()?;

        self.monitors.focused(|monitor| {
            if let Some(index) = monitor.workspace.find(focus.window) {
                f(&mut monitor.workspace.workspaces[monitor.workspace.current][index])?;
            }

            Ok(())
        })
    }

    fn mov_resize_focused<F>(&mut self, transform: F) -> Result<(), Box<dyn std::error::Error>> where F: Fn(u16, u16, u16, u16) -> (u16, u16, u16, u16) {
        self.focused_client(|client| {
            if client.float {
                let geometry = client.window.get_geometry()?;

                let (x, y, width, height) = transform(geometry.x, geometry.y, geometry.width, geometry.height);

                client.window.mov_resize(x, y, width, height)?;
            }

            Ok(())
        })
    }

    fn set_focused_border(&mut self, focused: u32) -> Result<(), Box<dyn std::error::Error>> {
        if focused != self.root.id() && focused != 1 && focused != 0 {
            let borders = self.config.windows.borders;

            self.monitors.focused(|monitor| {
                monitor.workspace.map_clients(|client| {
                    client.window.set_border_width(borders.width)?;

                    client.window.set_border_pixel(borders.normal)?;

                    Ok(())
                })?;

                Ok(())
            })?;

            self.display.window_from_id(focused)?.set_border_pixel(borders.focused)?;
        }

        Ok(())
    }

    fn update_borders(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let focus = self.display.get_input_focus()?;

        self.set_focused_border(focus.window)
    }

    fn handle_incoming(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        for sequence in self.server.incoming()? {
            println!("sequence: {:?}", sequence);

            match sequence.request {
                Request::Workspace => {
                    self.monitors.focused(|monitor| {
                        if sequence.value.max(1) - 1 < monitor.workspace.len() as u32 {
                            monitor.workspace.current = sequence.value.max(1) as usize - 1;
                        }

                        monitor.workspace.tile(monitor.area.pad(self.config.padding), self.config.windows.gaps)
                    })?;
                },
                Request::Kill => {
                    self.focused_client(|client| client.window.kill())?;
                },
                Request::Close => {
                },
                Request::FocusUp | Request::FocusDown | Request::FocusMaster => {
                    let focus = self.display.get_input_focus()?;

                    self.monitors.focused(|monitor| {
                        match sequence.request {
                            Request::FocusUp => monitor.workspace.change_focus(focus.window, |index| index.max(1) - 1),
                            Request::FocusDown => monitor.workspace.change_focus(focus.window, |index| index + 1),
                            Request::FocusMaster => monitor.workspace.change_focus(focus.window, |_| 0),
                            _ => Ok(()),
                        }
                    })?;
                },
                Request::PaddingTop | Request::PaddingBottom | Request::PaddingLeft | Request::PaddingRight | Request::WindowGaps => {
                    match sequence.request {
                        Request::PaddingTop => self.config.padding.top = sequence.value as u16,
                        Request::PaddingBottom => self.config.padding.bottom = sequence.value as u16,
                        Request::PaddingLeft => self.config.padding.left = sequence.value as u16,
                        Request::PaddingRight => self.config.padding.right = sequence.value as u16,
                        Request::WindowGaps => self.config.windows.gaps = sequence.value as u16,
                        _ => unreachable!(),
                    }

                    self.tile()?;
                },
                Request::FocusedBorder | Request::NormalBorder | Request::BorderWidth => {
                    match sequence.request {
                        Request::FocusedBorder => self.config.windows.borders.focused = sequence.value,
                        Request::NormalBorder => self.config.windows.borders.normal = sequence.value,
                        Request::BorderWidth => self.config.windows.borders.width = sequence.value as u16,
                        _ => unreachable!(),
                    }

                    self.update_borders()?;
                },
                Request::FloatToggle => {
                    self.focused_client(|client| {
                        client.float = !client.float;

                        Ok(())
                    })?;

                    self.tile()?;
                },
                Request::FloatRight => self.mov_resize_focused(|x, y, width, height| (x + sequence.value as u16, y, width, height))?,
                Request::FloatLeft => self.mov_resize_focused(|x, y, width, height| (x - (sequence.value as u16).min(x), y, width, height))?,
                Request::FloatUp => self.mov_resize_focused(|x, y, width, height| (x, y - (sequence.value as u16).min(y), width, height))?,
                Request::FloatDown => self.mov_resize_focused(|x, y, width, height| (x, y + sequence.value as u16, width, height))?,
                Request::ResizeRight => self.mov_resize_focused(|x, y, width, height| (x, y, width + sequence.value as u16, height))?,
                Request::ResizeLeft => self.mov_resize_focused(|x, y, width, height| (x, y, width - (sequence.value as u16).min(width), height))?,
                Request::ResizeUp => self.mov_resize_focused(|x, y, width, height| (x, y, width, height - (sequence.value as u16).min(height)))?,
                Request::ResizeDown => self.mov_resize_focused(|x, y, width, height| (x, y, width, height + sequence.value as u16))?,
                Request::EnableMouse => self.config.windows.mouse_movement = true,
                Request::DisableMouse => self.config.windows.mouse_movement = false,
                Request::WorkspacePerMonitor => {
                    self.monitors.all(|monitor| {
                        monitor.workspace.resize(sequence.value as usize);

                        Ok(())
                    })?;
                },
                Request::Unknown => {},
            }
        }

        Ok(())
    }

    fn handle_event(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        match self.display.next_event()? {
            Event::MapRequest { window, .. } => {
                log::write(format!("map request: {}\n", window), Severity::Info)?;

                self.monitors.focused(|monitor| {
                    monitor.workspace.insert(Client::new(self.display.window_from_id(window)?, false));

                    Ok(())
                })?;

                self.tile()?;

                let mut window = self.display.window_from_id(window)?;

                window.select_input(&[EventMask::SubstructureNotify, EventMask::SubstructureRedirect, EventMask::EnterWindow, EventMask::FocusChange])?;

                window.set_input_focus(RevertTo::Parent)?;

                self.set_focused_border(window.id())?;
            },
            Event::UnmapNotify { window, .. } => {
                log::write(format!("unmap notify: {}\n", window), Severity::Info)?;

                self.monitors.all(|monitor| {
                    if let Some(index) = monitor.workspace.find(window) {
                        monitor.workspace.remove(index);

                        monitor.workspace.change_focus(window, |index| index - 1)?;
                    }

                    Ok(())
                })?;

                self.tile()?;
            },
            Event::EnterNotify { window, .. } => {
                log::write(format!("enter notify: {}\n", window), Severity::Info)?;

                if window != self.root.id() {
                    self.display.window_from_id(window)?.set_input_focus(RevertTo::Parent)?;
                }
            },
            Event::FocusIn { window, .. } => {
                self.set_focused_border(window)?;
            },
            Event::ButtonEvent { kind, coordinates, subwindow, button, .. } => match kind {
                EventKind::Press => {
                    if !self.monitors.is_tiled(subwindow) && self.config.windows.mouse_movement {
                        let mut window = self.display.window_from_id(subwindow)?;

                        window.raise()?;

                        window.grab_pointer(
                            vec![EventMask::PointerMotion, EventMask::ButtonRelease],
                            Cursor::Nop,
                            PointerMode::Asynchronous,
                            KeyboardMode::Asynchronous,
                            true,
                            0,
                        )?;

                        let geometry = window.get_geometry()?;

                        self.grab.replace(Grab::new(button, window, geometry, coordinates.root_x, coordinates.root_y));
                    }
                },
                EventKind::Release => {
                    if self.grab.is_some() {
                        self.display.ungrab_pointer()?;

                        self.grab = None;
                    }
                },
            },
            Event::MotionNotify { coordinates, .. } => {
                if let Some(grab) = &mut self.grab {
                    let x_diff = coordinates.root_x as i16 - grab.x as i16;
                    let y_diff = coordinates.root_y as i16 - grab.y as i16;

                    match grab.button {
                        Button::Button1 => {
                            grab.window.mov((grab.geometry.x as i16 + x_diff) as u16, (grab.geometry.y as i16 + y_diff) as u16)?;
                        },
                        Button::Button3 => {
                            grab.window.resize((grab.geometry.width as i16 + x_diff) as u16, (grab.geometry.height as i16 + y_diff) as u16)?;
                        },
                        _ => {},
                    }
                }
            },
            Event::ConfigureRequest { window, .. } => {
                // TODO: looks like there is something wrong with configure request,
                //
                // maybe xterm waits for a configure notify after it sends the configure request?

                log::write(format!("configure request: {}\n", window), Severity::Info)?;

                /*
                let mut window = self.display.window_from_id(window)?;

                window.configure();
                */
            },
            _ => {},
        }

        Ok(())
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.setup()?;

        // TODO: multi monitor support using xinerama

        log::write("yaxum is running\n", Severity::Info)?;

        while !self.should_close {
            if self.display.poll_event()? {
                self.handle_event()?;
            }

            self.handle_incoming()?;
        }

        Ok(())
    }
}


